use std::collections::HashMap;
use std::sync::RwLock;
use std::{net::Ipv4Addr, slice::from_raw_parts_mut, sync::Arc, thread::spawn};

use lockfree::queue::Queue;
use log::error;

use crate::device::{Aeth, Bth, Ipv4, NReth, Udp};
use crate::qp::QpContext;
use crate::types::{Key, MemAccessTypeFlag, Msn, Psn, QpType, Qpn};

use crate::utils::calculate_packet_cnt;
use crate::{
    device::{
        ToCardWorkRbDescBuilder, ToCardWorkRbDescCommon, ToHostWorkRbDescAethCode,
        ToHostWorkRbDescOpcode, ToHostWorkRbDescRead,
    },
    Error,
};
use crate::{Sge, WorkDescriptorSender};

/// Command about ACK and NACK
/// Typically, the message is sent by checker thread, which is responsible for checking the packet ordering.
pub(crate) struct RespAckCommand {
    pub(crate) dpqn: Qpn,
    pub(crate) msn: Msn,
    pub(crate) psn: Psn,
    pub(crate) last_retry_psn: Option<Psn>,
}

impl RespAckCommand {
    pub(crate) fn new_ack(dpqn: Qpn, msn: Msn, last_psn: Psn) -> Self {
        Self {
            dpqn,
            msn,
            psn: last_psn,
            last_retry_psn: None,
        }
    }

    pub(crate) fn new_nack(dpqn: Qpn, msn: Msn, psn: Psn, last_retry_psn: Psn) -> Self {
        Self {
            dpqn,
            msn,
            psn,
            last_retry_psn: Some(last_retry_psn),
        }
    }
}

/// Command about read response
pub(crate) struct RespReadRespCommand {
    pub(crate) desc: ToHostWorkRbDescRead,
}

/// The response command sent by other threads
///
/// Currently, it supports two types of response:
/// * Acknowledge(ack,nack)
/// * Read Response
pub(crate) enum RespCommand {
    Acknowledge(RespAckCommand), // Acknowledge or Negative Acknowledge
    ReadResponse(RespReadRespCommand),
}

/// A thread that is responsible for sending the response to the other side
pub(crate) struct DescResponser {
    _thread: std::thread::JoinHandle<()>,
}

impl DescResponser {
    pub fn new(
        device: Arc<dyn WorkDescriptorSender>,
        recving_queue: std::sync::mpsc::Receiver<RespCommand>,
        ack_buffers: AcknowledgeBuffer,
        qp_table: Arc<RwLock<HashMap<Qpn, QpContext>>>,
    ) -> Self {
        let _thread = spawn(|| Self::working_thread(device, recving_queue, ack_buffers, qp_table));
        Self { _thread }
    }

    fn working_thread(
        device: Arc<dyn WorkDescriptorSender>,
        recving_queue: std::sync::mpsc::Receiver<RespCommand>,
        ack_buffers: AcknowledgeBuffer,
        qp_table: Arc<RwLock<HashMap<Qpn, QpContext>>>,
    ) {
        loop {
            match recving_queue.recv() {
                Ok(RespCommand::Acknowledge(ack)) => {
                    // send ack to device
                    let ack_buf = ack_buffers.alloc().unwrap();
                    let (src_ip, dst_ip, common) = match qp_table.read().unwrap().get(&ack.dpqn) {
                        Some(qp) => {
                            let dst_ip = qp.dqp_ip;
                            let src_ip = qp.local_ip;
                            let common = ToCardWorkRbDescCommon {
                                total_len: ACKPACKET_SIZE as u32,
                                rkey: Key::default(),
                                raddr: 0,
                                dqp_ip: dst_ip,
                                dqpn: ack.dpqn,
                                mac_addr: qp.dqp_mac_addr,
                                pmtu: qp.pmtu,
                                flags: MemAccessTypeFlag::IbvAccessNoFlags,
                                qp_type: QpType::RawPacket,
                                psn: Psn::default(),
                                msn: ack.msn,
                            };
                            (src_ip, dst_ip, common)
                        }
                        None => {
                            error!("Failed to get QP from QP table: {:?}", ack.dpqn);
                            continue;
                        }
                    };

                    let last_retry_psn = ack.last_retry_psn;
                    if let Err(e) = write_packet(
                        ack_buf,
                        src_ip,
                        dst_ip,
                        ack.dpqn,
                        ack.msn,
                        ack.psn,
                        last_retry_psn,
                    ) {
                        error!("Failed to write ack/nack packet: {:?}", e);
                        continue;
                    }
                    let sge = ack_buffers.convert_buf_into_sge(&ack_buf, ACKPACKET_SIZE as u32);
                    let desc = ToCardWorkRbDescBuilder::new_write()
                        .with_common(common)
                        .with_sge(sge)
                        .build();
                    match desc {
                        Ok(desc) => {
                            if let Err(e) = device.send_work_desc(desc) {
                                error!("Failed to push ack/nack packet: {:?}", e);
                            }
                        }
                        Err(e) => {
                            error!("Failed to create descripotr: {:?}", e);
                        }
                    }

                    ack_buffers.free(ack_buf);
                }
                Ok(RespCommand::ReadResponse(resp)) => {
                    // send read response to device
                    let dpqn = resp.desc.common.dqpn;
                    let common = match qp_table.read().unwrap().get(&dpqn) {
                        Some(qp) => {
                            let mut common = ToCardWorkRbDescCommon {
                                total_len: resp.desc.len,
                                rkey: resp.desc.rkey,
                                raddr: resp.desc.raddr,
                                dqp_ip: qp.dqp_ip,
                                dqpn: dpqn,
                                mac_addr: qp.dqp_mac_addr,
                                pmtu: qp.pmtu,
                                flags: MemAccessTypeFlag::IbvAccessNoFlags,
                                qp_type: qp.qp_type,
                                psn: Psn::default(),
                                msn: resp.desc.common.msn,
                            };
                            let packet_cnt =
                                calculate_packet_cnt(qp.pmtu, resp.desc.raddr, resp.desc.len);
                            let first_pkt_psn = {
                                let mut send_psn = qp.sending_psn.lock().unwrap();
                                let first_pkt_psn = *send_psn;
                                *send_psn = send_psn.wrapping_add(packet_cnt);
                                first_pkt_psn
                            };
                            common.psn = first_pkt_psn;
                            common
                        }
                        None => {
                            error!("Failed to get QP from QP table: {:?}", dpqn);
                            continue;
                        }
                    };

                    let sge = Sge {
                        addr: resp.desc.laddr,
                        len: resp.desc.len,
                        key: resp.desc.lkey,
                    };
                    let desc = ToCardWorkRbDescBuilder::new_read_resp()
                        .with_common(common)
                        .with_sge(sge)
                        .build();
                    match desc {
                        Ok(desc) => {
                            if let Err(e) = device.send_work_desc(desc) {
                                error!("Failed to push read response: {:?}", e);
                            }
                        }
                        Err(e) => {
                            error!("Failed to create descripotr: {:?}", e);
                        }
                    }
                }
                Err(_) => {
                    // The only error is pipe broken, so just exit the thread
                    return;
                }
            }
        }
    }
}

type Slot = &'static mut [u8];

/// A structure to hold the acknowledge buffer
///
/// TODO: currently, it does not support the auto buffer recycling.
///
/// The element is `Option<Slot>` because the `Queue` need to initialize some nodes as Sentinel
/// while the reference can not be initialized as `None`.
pub(crate) struct AcknowledgeBuffer {
    free_list: Queue<Option<Slot>>,
    start_va: usize,
    length: usize,
    lkey: Key,
}

impl AcknowledgeBuffer {
    pub const ACKNOWLEDGE_BUFFER_SLOT_SIZE: usize = 64;
    /// Create a new acknowledge buffer
    pub fn new(start_va: usize, length: usize, lkey: Key) -> Self {
        assert!(
            length % Self::ACKNOWLEDGE_BUFFER_SLOT_SIZE == 0,
            "The length should be multiple of 64"
        );
        let free_list = Queue::new();
        let mut va = start_va;
        let slots: usize = length / Self::ACKNOWLEDGE_BUFFER_SLOT_SIZE;

        for _ in 0..slots {
            // SAFETY: the buffer given by the user should be valid, can be safely converted to array
            let buf =
                unsafe { from_raw_parts_mut(va as *mut u8, Self::ACKNOWLEDGE_BUFFER_SLOT_SIZE) };
            free_list.push(Some(buf));
            va += Self::ACKNOWLEDGE_BUFFER_SLOT_SIZE;
        }
        Self {
            free_list,
            start_va,
            length,
            lkey,
        }
    }

    pub fn alloc(&self) -> Option<Slot> {
        // FIXME: currently, we just recycle all the buffer in the free list.
        let result = self.free_list.pop();
        match result {
            Some(Some(buf)) => Some(buf),
            Some(None) => None,
            None => {
                // The buffer is already freed, so we just try to allocate another buffer
                let mut va = self.start_va;
                let slots: usize = self.length / Self::ACKNOWLEDGE_BUFFER_SLOT_SIZE;
                for _ in 0..slots {
                    // SAFETY: the buffer given by the user should be valid, can be safely converted to array
                    let buf = unsafe {
                        from_raw_parts_mut(va as *mut u8, Self::ACKNOWLEDGE_BUFFER_SLOT_SIZE)
                    };
                    self.free_list.push(Some(buf));
                    va += Self::ACKNOWLEDGE_BUFFER_SLOT_SIZE;
                }
                self.free_list.pop().map(|x| x.unwrap())
            }
        }
    }

    pub fn free(&self, buf: Slot) {
        // check if the buffer is within the range
        let start = self.start_va as *const u8;
        let end = start.wrapping_add(self.length);
        let buf_start = buf.as_ptr();
        let buf_end = buf_start.wrapping_add(AcknowledgeBuffer::ACKNOWLEDGE_BUFFER_SLOT_SIZE);
        assert!(
            buf_start >= start && buf_end <= end && buf.len() == Self::ACKNOWLEDGE_BUFFER_SLOT_SIZE,
            "The buffer is out of range"
        );
        self.free_list.push(Some(buf));
    }

    pub fn convert_buf_into_sge(&self, buf: &Slot, real_length: u32) -> Sge {
        Sge {
            addr: buf.as_ptr() as u64,
            len: real_length,
            key: self.lkey,
        }
    }
}

/// Write the IP header and UDP header
fn write_packet(
    buf: &mut [u8],
    src_addr: Ipv4Addr,
    dst_addr: Ipv4Addr,
    dpqn: Qpn,
    msn: Msn,
    psn: Psn,
    last_retry_psn: Option<Psn>,
) -> Result<(), Error> {
    let buf = &mut buf[..ACKPACKET_SIZE];
    // write a ip header
    let mut ip_header = Ipv4(buf);
    ip_header.set_version_and_len(IP_DEFAULT_VERSION_AND_LEN as u32);
    ip_header.set_dscp_ecn(0);
    ip_header.set_total_length(u32::from_be_bytes([
        0,
        0,
        ACKPACKET_SIZE.try_into().unwrap(),
        0,
    ]));
    ip_header.set_identification(0x27);
    ip_header.set_fragment_offset(0);
    ip_header.set_ttl(IP_DEFAULT_TTL as u32);
    ip_header.set_protocol(IP_DEFAULT_PROTOCOL as u32);
    let src_addr: u32 = src_addr.into();
    ip_header.set_source(src_addr.to_be());
    let dst_addr: u32 = dst_addr.into();
    ip_header.set_destination(dst_addr.to_be());
    // Set the checksum to 0, and calculate the checksum later
    ip_header.set_checksum(0);
    let checksum = calculate_ipv4_checksum(ip_header.0).to_be();
    ip_header.set_checksum(checksum.into());

    let udp_buf = &mut ip_header.0[IPV4_HEADER_SIZE..];
    let mut udp_header = Udp(udp_buf);
    udp_header.set_src_port(RDMA_DEFAULT_PORT.to_be());
    udp_header.set_dst_port(RDMA_DEFAULT_PORT.to_be());
    udp_header.set_length((ACKPACKET_WITHOUT_IPV4_HEADER_SIZE as u16).to_be());
    // It might redundant to calculate checksum, as the ICRC will calculate the another checksum
    udp_header.set_checksum(0);

    let bth_hdr_buf = &mut ip_header.0[IPV4_HEADER_SIZE + UDP_HEADER_SIZE..];
    let mut bth_header = Bth(bth_hdr_buf);
    bth_header.set_opcode(ToHostWorkRbDescOpcode::Acknowledge as u32);
    bth_header.set_pad_count(0);
    bth_header.set_pkey(0);
    bth_header.set_ecn_and_resv6(0);

    bth_header.set_dqpn(dpqn.into_be());
    bth_header.set_psn(psn.into_be());

    let is_nak = last_retry_psn.is_some();
    let aeth_hdr_buf = &mut ip_header.0[IPV4_HEADER_SIZE + UDP_HEADER_SIZE + BTH_HEADER_SIZE..];
    let mut aeth_header = Aeth(aeth_hdr_buf);
    if is_nak {
        aeth_header.set_aeth_code(ToHostWorkRbDescAethCode::Nak as u32);
    } else {
        aeth_header.set_aeth_code(ToHostWorkRbDescAethCode::Ack as u32);
    }
    aeth_header.set_aeth_value(0);
    aeth_header.set_msn(msn.into_be().into());

    let mut nreth_header = NReth(
        &mut ip_header.0[IPV4_HEADER_SIZE + UDP_HEADER_SIZE + BTH_HEADER_SIZE + AETH_HEADER_SIZE..],
    );
    if is_nak {
        let last_retry_psn = last_retry_psn.unwrap().into_be();
        nreth_header.set_last_retry_psn(last_retry_psn);
    } else {
        nreth_header.set_last_retry_psn(0);
    }
    // calculate the ICRC
    let total_buf = &mut ip_header.0[..ACKPACKET_SIZE];
    let icrc = calculate_icrc(total_buf)?;
    total_buf[ACKPACKET_SIZE - 4..].copy_from_slice(&icrc.to_le_bytes());
    Ok(())
}

const IPV4_HEADER_SIZE: usize = 20;
const UDP_HEADER_SIZE: usize = 8;
const BTH_HEADER_SIZE: usize = 12;
const IPV4_UDP_BTH_HEADER_SIZE: usize = IPV4_HEADER_SIZE + UDP_HEADER_SIZE + BTH_HEADER_SIZE;
const AETH_HEADER_SIZE: usize = 4;
const NRETH_HEADER_SIZE: usize = 4;
const ICRCSIZE: usize = 4;
const ACKPACKET_SIZE: usize = IPV4_HEADER_SIZE
    + UDP_HEADER_SIZE
    + BTH_HEADER_SIZE
    + AETH_HEADER_SIZE
    + NRETH_HEADER_SIZE
    + ICRCSIZE;
const ACKPACKET_WITHOUT_IPV4_HEADER_SIZE: usize = ACKPACKET_SIZE - IPV4_HEADER_SIZE;

const IP_DEFAULT_VERSION_AND_LEN: u8 = 0x45;
const IP_DEFAULT_TTL: u8 = 64;
const IP_DEFAULT_PROTOCOL: u8 = 17;
const RDMA_DEFAULT_PORT: u16 = 4791;

/// Calculate the RDMA packet ICRC.
///
/// the `data` passing in should include the space for the ICRC(4 bytes).
fn calculate_icrc(data: &[u8]) -> Result<u32, Error> {
    let mut hasher = crc32fast::Hasher::new();
    let prefix = [0xffu8; 8];
    let mut buf = [0; IPV4_UDP_BTH_HEADER_SIZE];
    hasher.update(&prefix);

    buf.copy_from_slice(data[..IPV4_UDP_BTH_HEADER_SIZE].as_ref());
    let mut ip_header = Ipv4(&mut buf);
    ip_header.set_dscp_ecn(0xff);
    ip_header.set_ttl(0xff);
    ip_header.set_checksum(0xffff);

    let mut udp_header = Udp(&mut buf[IPV4_HEADER_SIZE..]);
    udp_header.set_checksum(0xffff);

    let mut bth_header = Bth(&mut buf[IPV4_HEADER_SIZE + UDP_HEADER_SIZE..]);
    bth_header.set_ecn_and_resv6(0xff);

    hasher.update(&buf);
    // the rest of header and payload
    hasher.update(&data[IPV4_UDP_BTH_HEADER_SIZE..data.len() - 4]);
    Ok(hasher.finalize())
}

/// Calculate the checksum of the IPv4 header
///
/// The `header` should be a valid IPv4 header, and the checksum field is set to 0.
fn calculate_ipv4_checksum(header: &[u8]) -> u16 {
    let mut sum = 0u32;

    for i in (0..IPV4_HEADER_SIZE).step_by(2) {
        let word = if i + 1 < header.len() {
            ((header[i] as u16) << 8) | header[i + 1] as u16
        } else {
            (header[i] as u16) << 8
        };
        sum += word as u32;
    }

    while sum >> 16 != 0 {
        sum = (sum & 0xFFFF) + (sum >> 16);
    }

    !sum as u16
}

#[cfg(test)]
mod tests {
    use std::{
        net::Ipv4Addr,
        sync::{Arc, Mutex},
        thread::sleep,
    };

    use eui48::MacAddress;

    use crate::{
        device::{ToCardWorkRbDesc, ToHostWorkRbDescCommon, ToHostWorkRbDescRead},
        qp::QpContext,
        responser::{calculate_ipv4_checksum, ACKPACKET_SIZE},
        types::{Key, MemAccessTypeFlag, Msn, Pmtu, Psn, Qpn},
    };

    use super::calculate_icrc;
    #[test]
    fn test_icrc_computing() {
        let packet = [
            0x45, 0x00, 0x00, 0xbc, 0x00, 0x01, 0x00, 0x00, 0x40, 0x11, 0xf8, 0xda, 0xc0, 0xa8,
            0x00, 0x02, 0xc0, 0xa8, 0x00, 0x03, 0x12, 0xb7, 0x12, 0xb7, 0x00, 0xa8, 0x00, 0x00,
            0x0a, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00, 0x02, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x7f, 0x7e, 0x91, 0x00, 0x00, 0x00, 0x01, 0x70, 0x9a, 0x33, 0x00, 0x00, 0x00, 0x80,
            0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
            0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
            0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
            0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
            0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
            0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
            0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
            0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
            0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
            0xff, 0xff, 0xc6, 0x87, 0x22, 0x98,
        ];
        let icrc = calculate_icrc(&packet).unwrap();
        assert_eq!(icrc, u32::from_le_bytes([0xc6, 0x87, 0x22, 0x98]));
        let packet = [
            69, 0, 0, 0, 0, 0, 0, 0, 64, 17, 124, 232, 127, 0, 0, 3, 127, 0, 0, 2, 18, 183, 18,
            183, 0, 32, 0, 0, 17, 0, 0, 0, 0, 0, 0, 2, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
            0,
        ];
        let icrc = calculate_icrc(&packet).unwrap();
        assert_eq!(icrc, u32::from_le_bytes([64, 33, 163, 207]));


    }
    #[test]
    fn test_calculate_ipv4_checksum() {
        // capture from a real packet
        let ref_header: [u8; 20] = [
            0x45, 0x00, 0x00, 0x34, 0xb4, 0xef, 0x40, 0x00, 0x25, 0x06, 0x00, 0x00, 0x14, 0x59,
            0xed, 0x08, 0xac, 0x1b, 0xe8, 0xda,
        ];
        let expected_checksum: u16 = u16::from_be_bytes([0x0a, 0x7d]);
        let checksum = calculate_ipv4_checksum(&ref_header);
        assert_eq!(checksum, expected_checksum);
        let ref_header = [
            0x45, 0x00, 0x02, 0x0c, 0x3f, 0x3b, 0x40, 0x00, 0x29, 0x06, 0x00, 0x00, 0x8c, 0x52,
            0x72, 0x15, 0xac, 0x1b, 0xe8, 0xda,
        ];
        let expected_checksum: u16 = u16::from_be_bytes([0x7d, 0x53]);
        let checksum = calculate_ipv4_checksum(&ref_header);
        assert_eq!(checksum, expected_checksum);
        let ref_header = [
            0x45, 0x0, 0x0, 0xbc, 0x0, 0x1, 0x0, 0x0, 0x40, 0x11, 0x0, 0x0, 0xc0, 0xa8, 0x0, 0x2,
            0xc0, 0xa8, 0x0, 0x3,
        ];
        let checksum = calculate_ipv4_checksum(&ref_header);
        let expected_checksum: u16 = u16::from_be_bytes([0xf8, 0xda]);
        assert_eq!(checksum, expected_checksum);
    }

    const BUFFER_SIZE: usize = 1024 * super::AcknowledgeBuffer::ACKNOWLEDGE_BUFFER_SLOT_SIZE;
    #[test]
    fn test_desc_responser() {
        let (sender, receiver) = std::sync::mpsc::channel();
        let buffer = Box::new([0u8; BUFFER_SIZE]);
        let buffer = Box::leak(buffer);
        let ack_buffers =
            super::AcknowledgeBuffer::new(buffer.as_ptr() as usize, BUFFER_SIZE, Key::new(0x1000));
        struct Dummy(Mutex<Vec<ToCardWorkRbDesc>>);
        impl super::WorkDescriptorSender for Dummy {
            fn send_work_desc(&self, desc: ToCardWorkRbDesc) -> Result<(), crate::Error> {
                self.0.lock().unwrap().push(desc);
                Ok(())
            }
        }
        let dummy = std::sync::Arc::new(Dummy(Mutex::new(Vec::new())));
        let qp_table =
            std::sync::Arc::new(std::sync::RwLock::new(std::collections::HashMap::new()));
        qp_table.write().unwrap().insert(
            Qpn::new(3),
            QpContext {
                qp_type: crate::types::QpType::Rc,
                pmtu: Pmtu::Mtu4096,
                pd: crate::Pd { handle: 1 },
                qpn: Qpn::new(3),
                rq_acc_flags: MemAccessTypeFlag::IbvAccessNoFlags,
                local_ip: Ipv4Addr::LOCALHOST,
                local_mac_addr: MacAddress::new([0; 6]),
                dqp_ip: Ipv4Addr::LOCALHOST,
                dqp_mac_addr: MacAddress::new([0; 6]),
                sending_psn: Mutex::new(Psn::new(0)),
            },
        );
        let _ =
            super::DescResponser::new(Arc::<Dummy>::clone(&dummy), receiver, ack_buffers, qp_table);
        sender
            .send(super::RespCommand::Acknowledge(super::RespAckCommand {
                dpqn: Qpn::new(3),
                msn: Msn::new(0),
                psn: Psn::new(0),
                last_retry_psn: None,
            }))
            .unwrap();
        sender
            .send(super::RespCommand::Acknowledge(super::RespAckCommand {
                dpqn: Qpn::new(3),
                msn: Msn::new(0),
                psn: Psn::new(0),
                last_retry_psn: Some(Psn::new(12)),
            }))
            .unwrap();
        sender
            .send(super::RespCommand::ReadResponse(
                super::RespReadRespCommand {
                    desc: ToHostWorkRbDescRead {
                        common: ToHostWorkRbDescCommon {
                            status: crate::device::ToHostWorkRbDescStatus::Normal,
                            trans: crate::device::ToHostWorkRbDescTransType::Rc,
                            dqpn: Qpn::new(3),
                            pad_cnt: 0,
                            msn: Msn::new(0),
                            expected_psn: Psn::default(),
                        },
                        len: 10,
                        laddr: 10,
                        lkey: Key::new(10),
                        raddr: 0,
                        rkey: Key::new(10),
                    },
                },
            ))
            .unwrap();
        drop(sender);

        // check
        sleep(std::time::Duration::from_millis(10));
        let mut v = dummy.0.lock().unwrap();
        assert_eq!(v.len(), 3);
        let desc = v.pop().unwrap();
        match desc {
            crate::device::ToCardWorkRbDesc::ReadResp(desc) => {
                assert_eq!(desc.common.dqpn.get(), 3);
                assert_eq!(desc.common.total_len, 10_u32);
                assert_eq!(desc.common.rkey.get(), 10);
                assert_eq!(desc.common.raddr, 0);
                assert_eq!(desc.common.dqp_ip, std::net::Ipv4Addr::LOCALHOST);
                assert_eq!(desc.common.mac_addr, MacAddress::default());
                assert!(matches!(desc.common.pmtu, Pmtu::Mtu4096));
                assert_eq!(desc.common.flags.bits(), 0);
                assert!(matches!(desc.common.qp_type, crate::types::QpType::Rc));
                assert_eq!(desc.common.psn.get(), 0);
                assert_eq!(desc.sge0.len, 10_u32);
                assert_eq!(desc.sge0.key.get(), 10);
            }
            crate::device::ToCardWorkRbDesc::Read(_)
            | crate::device::ToCardWorkRbDesc::Write(_)
            | crate::device::ToCardWorkRbDesc::WriteWithImm(_) => {
                panic!("Unexpected desc type");
            }
        }
        let desc = v.pop().unwrap();
        // NACK
        match desc {
            crate::device::ToCardWorkRbDesc::Write(desc) => {
                assert_eq!(desc.common.dqpn.get(), 3);
                assert_eq!(desc.common.total_len, ACKPACKET_SIZE as u32);
                assert_eq!(desc.common.rkey.get(), 0);
                assert_eq!(desc.common.raddr, 0);
                assert_eq!(desc.common.dqp_ip, std::net::Ipv4Addr::LOCALHOST);
                assert_eq!(desc.common.mac_addr, MacAddress::default());
                assert!(matches!(desc.common.pmtu, Pmtu::Mtu4096));
                assert_eq!(desc.common.flags.bits(), 0);
                assert!(matches!(
                    desc.common.qp_type,
                    crate::types::QpType::RawPacket
                ));
                assert_eq!(desc.common.psn.get(), 0);
                assert_eq!(desc.sge0.len, ACKPACKET_SIZE as u32);
                assert_eq!(desc.sge0.key.get(), 0x1000);
            }
            crate::device::ToCardWorkRbDesc::Read(_)
            | crate::device::ToCardWorkRbDesc::WriteWithImm(_)
            | crate::device::ToCardWorkRbDesc::ReadResp(_) => {
                panic!("Unexpected desc type");
            }
        }

        // ACK
        let desc = v.pop().unwrap();
        match desc {
            crate::device::ToCardWorkRbDesc::Write(desc) => {
                assert_eq!(desc.common.dqpn.get(), 3);
                assert_eq!(desc.common.total_len, ACKPACKET_SIZE as u32);
                assert_eq!(desc.common.rkey.get(), 0);
                assert_eq!(desc.common.raddr, 0);
                assert_eq!(desc.common.dqp_ip, std::net::Ipv4Addr::LOCALHOST);
                assert_eq!(desc.common.mac_addr, MacAddress::default());
                assert!(matches!(desc.common.pmtu, Pmtu::Mtu4096));
                assert_eq!(desc.common.flags.bits(), 0);
                assert!(matches!(
                    desc.common.qp_type,
                    crate::types::QpType::RawPacket
                ));
                assert_eq!(desc.common.psn.get(), 0);
                assert_eq!(desc.sge0.len, ACKPACKET_SIZE as u32);
                assert_eq!(desc.sge0.key.get(), 0x1000);
            }
            crate::device::ToCardWorkRbDesc::Read(_)
            | crate::device::ToCardWorkRbDesc::WriteWithImm(_)
            | crate::device::ToCardWorkRbDesc::ReadResp(_) => {
                panic!("Unexpected desc type");
            }
        }
    }

    #[test]
    fn test_acknowledge_buffer() {
        let mem = Box::leak(Box::new(
            [0u8; 1024 * super::AcknowledgeBuffer::ACKNOWLEDGE_BUFFER_SLOT_SIZE],
        ));
        let base_va = mem.as_ptr() as usize;
        let buffer = super::AcknowledgeBuffer::new(base_va, 1024 * 64, Key::new(0x1000));
        for i in 0..1024 {
            let buf = buffer.alloc().unwrap();
            assert_eq!(
                buf.as_ptr() as usize,
                mem.as_ptr() as usize + i * super::AcknowledgeBuffer::ACKNOWLEDGE_BUFFER_SLOT_SIZE
            );
        }
        // Now the buffer is full, it recycles the buffer
        for i in 0..1024 {
            let buf = buffer.alloc().unwrap();
            assert_eq!(buf.as_ptr() as usize, mem.as_ptr() as usize + i * 64);
        }
    }
}
