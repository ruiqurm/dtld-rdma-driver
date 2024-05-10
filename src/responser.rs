use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::{net::Ipv4Addr, sync::Arc, thread::spawn};

use crate::buf::{PacketBuf, RDMA_ACK_BUFFER_SLOT_SIZE};
use crate::device::descriptor::{Aeth, Bth, Ipv4, NReth, Udp};
use crate::qp::QpContext;
use crate::types::{Key, MemAccessTypeFlag, Msn, Psn, QpType, Qpn};
use flume::Receiver;
use log::error;

use crate::device::{
    ToCardWorkRbDescBuilder, ToCardWorkRbDescCommon, ToHostWorkRbDescAethCode,
    ToHostWorkRbDescOpcode, ToHostWorkRbDescRead,
};
use crate::utils::calculate_packet_cnt;
use crate::{Error, Sge, WorkDescriptorSender};
use parking_lot::RwLock;

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

    pub(crate) fn new_nack(dpqn: Qpn, msg_seq_num: Msn, psn: Psn, last_retry_psn: Psn) -> Self {
        Self {
            dpqn,
            msn: msg_seq_num,
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
#[derive(Debug)]
pub(crate) struct DescResponser {
    thread: Option<std::thread::JoinHandle<()>>,
    stop_flag: Arc<AtomicBool>,
}

impl DescResponser {
    pub(crate) fn new(
        device: Arc<dyn WorkDescriptorSender>,
        recving_queue: Receiver<RespCommand>,
        ack_buffers: PacketBuf<RDMA_ACK_BUFFER_SLOT_SIZE>,
        qp_table: Arc<RwLock<HashMap<Qpn, QpContext>>>,
    ) -> Self {
        let stop_flag = Arc::new(AtomicBool::new(false));
        let thread_stop_flag = Arc::clone(&stop_flag);
        let thread = spawn(move || {
            Self::working_thread(
                device,
                recving_queue,
                &ack_buffers,
                &qp_table,
                &thread_stop_flag,
            );
        });
        Self {
            thread: Some(thread),
            stop_flag,
        }
    }

    fn create_ack_common(
        command: &RespAckCommand,
        qp: &QpContext,
        dst_ip: Ipv4Addr,
    ) -> ToCardWorkRbDescCommon {
        // `ACKPACKET_SIZE` is a constant, it is safe to cast it to u32
        #[allow(clippy::cast_possible_truncation)]
        ToCardWorkRbDescCommon {
            total_len: ACKPACKET_SIZE as u32,
            rkey: Key::default(),
            raddr: 0,
            dqp_ip: dst_ip,
            dqpn: command.dpqn,
            mac_addr: qp.dqp_mac_addr,
            pmtu: qp.pmtu,
            flags: MemAccessTypeFlag::IbvAccessNoFlags,
            qp_type: QpType::RawPacket,
            psn: Psn::default(),
            msn: command.msn,
        }
    }

    fn create_read_resp_common(
        qp_table: &RwLock<HashMap<Qpn, QpContext>>,
        resp: &RespReadRespCommand,
    ) -> Result<ToCardWorkRbDescCommon, Error> {
        let dpqn = resp.desc.common.dqpn;
        if let Some(qp) = qp_table.read().get(&dpqn) {
            let mut common = ToCardWorkRbDescCommon {
                total_len: resp.desc.len,
                rkey: resp.desc.rkey,
                raddr: resp.desc.raddr,
                dqp_ip: qp.dqp_ip,
                dqpn: resp.desc.common.dqpn,
                mac_addr: qp.dqp_mac_addr,
                pmtu: qp.pmtu,
                flags: MemAccessTypeFlag::IbvAccessNoFlags,
                qp_type: qp.qp_type,
                psn: Psn::default(),
                msn: resp.desc.common.msn,
            };
            let packet_cnt = calculate_packet_cnt(qp.pmtu, resp.desc.raddr, resp.desc.len);
            let first_pkt_psn = {
                let mut send_psn = qp.sending_psn.lock();
                let first_pkt_psn = *send_psn;
                *send_psn = send_psn.wrapping_add(packet_cnt);
                first_pkt_psn
            };
            common.psn = first_pkt_psn;
            Ok(common)
        } else {
            Err(Error::Invalid(format!("QP {dpqn:?}")))
        }
    }

    // may lead to false positive here
    #[allow(clippy::needless_pass_by_value)]
    fn working_thread(
        device: Arc<dyn WorkDescriptorSender>,
        recving_queue: Receiver<RespCommand>,
        ack_buffers: &PacketBuf<RDMA_ACK_BUFFER_SLOT_SIZE>,
        qp_table: &RwLock<HashMap<Qpn, QpContext>>,
        stop_flag: &AtomicBool,
    ) {
        // We don't care the time stop_flag is set, so we use `Relaxed` ordering
        while !stop_flag.load(Ordering::Relaxed) {
            let desc = match recving_queue.recv() {
                Ok(RespCommand::Acknowledge(ack)) => {
                    // send ack to device
                    let mut ack_buf = ack_buffers.recycle_buf();
                    // if we can not read qp_table here
                    #[allow(clippy::unwrap_used)]
                    let (src_ip, dst_ip, common) = {
                        let table = qp_table.read();
                        if let Some(qp) = table.get(&ack.dpqn) {
                            let dst_ip = qp.dqp_ip;
                            let src_ip = qp.local_ip;
                            let common = Self::create_ack_common(&ack, qp, dst_ip);
                            (src_ip, dst_ip, common)
                        } else {
                            error!("Failed to get QP from QP table: {:?}", ack.dpqn);
                            continue;
                        }
                    };
                    let last_retry_psn = ack.last_retry_psn;
                    write_packet(
                        ack_buf.as_mut_slice(),
                        src_ip,
                        dst_ip,
                        ack.dpqn,
                        ack.msn,
                        ack.psn,
                        last_retry_psn,
                    );
                    #[allow(clippy::cast_possible_truncation)]
                    let sge = ack_buf.into_sge(ACKPACKET_SIZE as u32);
                    ToCardWorkRbDescBuilder::new_write()
                        .with_common(common)
                        .with_sge(sge)
                        .build()
                }
                Ok(RespCommand::ReadResponse(resp)) => {
                    // send read response to device
                    let common = match Self::create_read_resp_common(qp_table, &resp) {
                        Ok(common) => common,
                        Err(e) => {
                            error!("responser failed to send read response: {:?}", e);
                            continue;
                        }
                    };

                    let sge = Sge {
                        addr: resp.desc.laddr,
                        len: resp.desc.len,
                        key: resp.desc.lkey,
                    };
                    ToCardWorkRbDescBuilder::new_read_resp()
                        .with_common(common)
                        .with_sge(sge)
                        .build()
                }
                Err(_) => {
                    // The only error is pipe broken, so just exit the thread
                    return;
                }
            };
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
        }
    }
}

impl Drop for DescResponser {
    fn drop(&mut self) {
        self.stop_flag.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            if let Err(e) = thread.join() {
                error!("Failed to join the responser thread: {:?}", e);
            }
        }
    }
}

/// Write the IP header and UDP header
///
/// # Panic
/// We assume the `buf` is large enough to hold the packet, in other words, the length should
/// at least be `ACKPACKET_SIZE`. If the length is less than `ACKPACKET_SIZE`, it will panic.
#[allow(clippy::indexing_slicing)]
fn write_packet(
    buf: &mut [u8],
    src_addr: Ipv4Addr,
    dst_addr: Ipv4Addr,
    dpqn: Qpn,
    msg_seq_num: Msn,
    psn: Psn,
    last_retry_psn: Option<Psn>,
) {
    let buf = &mut buf[..ACKPACKET_SIZE];
    // write a ip header
    let mut ip_header = Ipv4(buf);
    ip_header.set_version_and_len(u32::from(IP_DEFAULT_VERSION_AND_LEN));
    ip_header.set_dscp_ecn(0);

    // The `total_length` take a 16 bits **big-endian** number as input.
    // Only the third and forth bytes are used, so the we put the `ACKPACKET_SIZE` into the third byte.
    #[allow(clippy::cast_possible_truncation)]
    ip_header.set_total_length(u32::from_be_bytes([0, 0, ACKPACKET_SIZE as u8, 0]));
    ip_header.set_identification(0x27);
    ip_header.set_fragment_offset(0);
    ip_header.set_ttl(u32::from(IP_DEFAULT_TTL));
    ip_header.set_protocol(u32::from(IP_DEFAULT_PROTOCOL));
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
    #[allow(clippy::cast_possible_truncation)]
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
    aeth_header.set_msn(msg_seq_num.into_be().into());

    let mut nreth_header = NReth(
        &mut ip_header.0[IPV4_HEADER_SIZE + UDP_HEADER_SIZE + BTH_HEADER_SIZE + AETH_HEADER_SIZE..],
    );
    if is_nak {
        // we have checked the option before.
        #[allow(clippy::unwrap_used)]
        let last_retry_psn = last_retry_psn.unwrap().into_be();
        nreth_header.set_last_retry_psn(last_retry_psn);
    } else {
        nreth_header.set_last_retry_psn(0);
    }
    // calculate the ICRC
    let total_buf = &mut ip_header.0[..ACKPACKET_SIZE];
    let icrc = calculate_icrc(total_buf);
    total_buf[ACKPACKET_SIZE - 4..].copy_from_slice(&icrc.to_le_bytes());
}

const IPV4_HEADER_SIZE: usize = 20;
const UDP_HEADER_SIZE: usize = 8;
const BTH_HEADER_SIZE: usize = 12;
const IPV4_UDP_BTH_HEADER_SIZE: usize = IPV4_HEADER_SIZE + UDP_HEADER_SIZE + BTH_HEADER_SIZE;
const AETH_HEADER_SIZE: usize = 4;
const NRETH_HEADER_SIZE: usize = 4;
const ICRC_SIZE: usize = 4;
const ACKPACKET_SIZE: usize = IPV4_HEADER_SIZE
    + UDP_HEADER_SIZE
    + BTH_HEADER_SIZE
    + AETH_HEADER_SIZE
    + NRETH_HEADER_SIZE
    + ICRC_SIZE;
const ACKPACKET_WITHOUT_IPV4_HEADER_SIZE: usize = ACKPACKET_SIZE - IPV4_HEADER_SIZE;

const IP_DEFAULT_VERSION_AND_LEN: u8 = 0x45;
const IP_DEFAULT_TTL: u8 = 64;
const IP_DEFAULT_PROTOCOL: u8 = 17;
const RDMA_DEFAULT_PORT: u16 = 4791;

/// Calculate the RDMA packet ICRC.
///
/// the `data` passing in should include the space for the ICRC(4 bytes).
///
/// # Panic
/// We assume the `data` is large enough to hold the RDMA packet, in other words, the length should
/// contain the space for IP header, UDP header, BTH header and the ICRC.
#[allow(clippy::indexing_slicing)]
fn calculate_icrc(data: &[u8]) -> u32 {
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
    #[allow(clippy::arithmetic_side_effects)]
    hasher.update(&data[IPV4_UDP_BTH_HEADER_SIZE..data.len() - ICRC_SIZE]);
    hasher.finalize()
}

/// Calculate the checksum of the IPv4 header
///
/// The `header` should be a valid IPv4 header, and the checksum field is set to 0.
///
/// # Panic
/// The header is considered as a standard IPv4 header, so the length should be 20 bytes.
/// Otherwise it will panic.
#[allow(
    clippy::cast_possible_truncation,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects
)]
fn calculate_ipv4_checksum(header: &[u8]) -> u16 {
    let mut sum = 0u32;

    for i in (0..IPV4_HEADER_SIZE).step_by(2) {
        let word = if i + 1 < header.len() {
            (u16::from(header[i]) << 8_i32) | u16::from(header[i + 1])
        } else {
            u16::from(header[i]) << 8_i32
        };
        sum += u32::from(word);
    }

    while sum >> 16_i32 != 0 {
        sum = (sum & 0xFFFF) + (sum >> 16_i32);
    }

    !sum as u16
}

#[cfg(test)]
mod tests {
    use std::{net::Ipv4Addr, sync::Arc, thread::sleep};

    use eui48::MacAddress;
    use flume::unbounded;

    use crate::{
        buf::PacketBuf,
        device::{ToCardWorkRbDesc, ToHostWorkRbDescCommon, ToHostWorkRbDescRead},
        qp::QpContext,
        responser::{calculate_ipv4_checksum, ACKPACKET_SIZE},
        types::{Key, MemAccessTypeFlag, Msn, Pmtu, Psn, Qpn},
    };

    use super::calculate_icrc;

    use parking_lot::{Mutex, RwLock};

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
        let icrc = calculate_icrc(&packet);
        assert_eq!(icrc, u32::from_le_bytes([0xc6, 0x87, 0x22, 0x98]));
        let packet = [
            69, 0, 0, 0, 0, 0, 0, 0, 64, 17, 124, 232, 127, 0, 0, 3, 127, 0, 0, 2, 18, 183, 18,
            183, 0, 32, 0, 0, 17, 0, 0, 0, 0, 0, 0, 2, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
            0,
        ];
        let icrc = calculate_icrc(&packet);
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

    const BUFFER_SIZE: usize = 1024 * super::RDMA_ACK_BUFFER_SLOT_SIZE;
    #[test]
    fn test_desc_responser() {
        let (sender, receiver) = unbounded();
        let buffer = Box::new([0u8; BUFFER_SIZE]);
        let buffer = Box::leak(buffer);
        let ack_buffers = PacketBuf::new(buffer.as_ptr() as usize, BUFFER_SIZE, Key::new(0x1000));
        struct Dummy(Mutex<Vec<ToCardWorkRbDesc>>);
        impl super::WorkDescriptorSender for Dummy {
            fn send_work_desc(&self, desc: ToCardWorkRbDesc) -> Result<(), crate::Error> {
                self.0.lock().push(desc);
                Ok(())
            }
        }
        let dummy = std::sync::Arc::new(Dummy(Mutex::new(Vec::new())));
        let qp_table = std::sync::Arc::new(RwLock::new(std::collections::HashMap::new()));
        qp_table.write().insert(
            Qpn::new(3),
            QpContext {
                qp_type: crate::types::QpType::Rc,
                pmtu: Pmtu::Mtu4096,
                pd: crate::Pd { handle: 1 },
                qpn: Qpn::new(3),
                rq_acc_flags: MemAccessTypeFlag::IbvAccessNoFlags,
                local_ip: Ipv4Addr::LOCALHOST,
                dqp_ip: Ipv4Addr::LOCALHOST,
                dqp_mac_addr: MacAddress::new([0; 6]),
                sending_psn: Mutex::new(Psn::new(0)),
            },
        );
        let _responser =
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
        let mut v = dummy.0.lock();
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
}
