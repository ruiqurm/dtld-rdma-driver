use std::{
    collections::HashMap,
    sync::{atomic::Ordering, Arc},
};

use derive_builder::Builder;
use flume::unbounded;
use parking_lot::{lock_api::RwLock, Mutex};

use crate::{
    buf::{PacketBuf, RDMA_ACK_BUFFER_SLOT_SIZE},
    checker::{PacketCheckEvent, PacketCheckerContext, RecvContextMap},
    device::{
        ToCardCtrlRbDesc, ToCardWorkRbDesc, ToHostWorkRbDescCommon, ToHostWorkRbDescRead,
        ToHostWorkRbDescWriteOrReadResp, ToHostWorkRbDescWriteType,
    },
    op_ctx::CtrlOpCtx,
    qp::{QpContext, QpStatus},
    types::{Key, Msn, Pmtu, Psn, QpType, Qpn},
    utils::{calculate_packet_cnt, get_first_packet_max_length},
    CtrlDescriptorSender, WorkDescriptorSender,
};
macro_rules! construct_context {
    ($context : ident,$device: ident, $qpn : ident = $qpn_val : expr) => {
        let $device = Arc::new(MockCtrlDescSender::default());
        let ctrl_desc_sender: Arc<dyn CtrlDescriptorSender> = $device.clone();
        let work_desc_sender: Arc<dyn WorkDescriptorSender> = $device.clone();
        let user_op_ctx_map = RwLock::new(HashMap::new()).into();
        let qp_table = RwLock::new(HashMap::new()).into();
        let buffer = Box::new([0u8; BUFFER_SIZE]);
        let buffer = Box::leak(buffer);
        let lkey = Key::new(0x1000);
        let ack_buffers: PacketBuf<RDMA_ACK_BUFFER_SLOT_SIZE> =
            PacketBuf::new(buffer.as_ptr() as usize, BUFFER_SIZE, lkey);

        // we don't use the channel, so we don't care if it is closed
        let (_send_channel, desc_poller_channel) = unbounded();
        let $context = PacketCheckerContext {
            desc_poller_channel,
            recv_ctx_map: RecvContextMap::default(),
            qp_table,
            user_op_ctx_map,
            ctrl_desc_sender,
            work_desc_sender,
            ack_buffers,
        };
        let $qpn = Qpn::new($qpn_val);
        $context.qp_table.write().insert(
            $qpn,
            QpContext {
                $qpn,
                ..Default::default()
            },
        );
    };
}

macro_rules! make_ref_packet_event {
    ($name : ident,$qpn:ident,$start_psn: ident = $start_psn_val :expr,
         $msn : ident = $msn_val : expr,
         addr= $addr_val:expr,
         len= $len_val:expr) => {
        let $msn = Msn::new($msn_val);
        let $start_psn = Psn::new($start_psn_val);
        let $name: PacketCheckEvent = PacketWriteBuilder::create_empty()
            .dqpn($qpn)
            .msn($msn)
            .psn($start_psn)
            .write_type(ToHostWorkRbDescWriteType::First)
            .addr($addr_val)
            .len(($len_val) as u32)
            .can_auto_ack(true)
            .build()
            .unwrap()
            .into();
    };
}

#[test]
fn test_checker_on_recv_read_request() {
    construct_context!(context, device, qpn = 0x1234);
    let event = PacketCheckEvent::ReadReq(ToHostWorkRbDescRead {
        common: ToHostWorkRbDescCommon {
            dqpn: qpn,
            msn: Msn::new(0x1234),
            ..Default::default()
        },
        ..Default::default()
    });
    context.handle_check_event(event);
    let desc = device.work_pop().expect("should get a read response");
    if let ToCardWorkRbDesc::ReadResp(desc) = *desc {
        assert_eq!(desc.common.dqpn, qpn);
        assert_eq!(desc.common.msn, Msn::new(0x1234));
    } else {
        panic!("should be a read response");
    }
}

#[test]
fn test_checker_normal() {
    construct_context!(context, device, qpn = 0x1234);
    let start_psn = Psn::new(0x1234);
    let msn = Msn::new(0x1235);
    let num_of_pkt = 5;
    let packet_first: PacketCheckEvent = PacketWriteBuilder::create_empty()
        .dqpn(qpn)
        .msn(msn)
        .psn(start_psn)
        .write_type(ToHostWorkRbDescWriteType::First)
        .can_auto_ack(true)
        .build()
        .unwrap()
        .into();
    context.handle_check_event(packet_first.clone()); // should create a new qp context

    let mut packet_last = packet_first.clone();
    if let PacketCheckEvent::Write(ref mut e) = packet_last {
        e.write_type = ToHostWorkRbDescWriteType::Last;
        e.psn = start_psn.wrapping_add(num_of_pkt - 1);
        e.common.expected_psn = e.psn;
    }

    context.handle_check_event(packet_last.clone()); // should create a new qp context

    assert!(device.work_pop().is_none());

    // test can't auto ack
    let mut packet_first_can_not_ack = packet_first.clone();
    if let PacketCheckEvent::Write(ref mut e) = packet_first_can_not_ack {
        e.can_auto_ack = false;
    }
    let mut packet_last_can_not_ack = packet_last.clone();
    if let PacketCheckEvent::Write(ref mut e) = packet_last_can_not_ack {
        e.can_auto_ack = false;
    }
    context.handle_check_event(packet_first_can_not_ack);
    context.handle_check_event(packet_last_can_not_ack);
    let desc = device.work_pop().expect("should get a ack");
    if let ToCardWorkRbDesc::WriteWithImm(desc) = *desc {
        assert_eq!(desc.common.dqpn, qpn);
        assert_eq!(desc.common.msn, msn);
    } else {
        panic!("should be an ack");
    }

    // test only
    let mut packet_only = packet_first.clone();
    if let PacketCheckEvent::Write(ref mut e) = packet_only {
        e.can_auto_ack = false;
        e.write_type = ToHostWorkRbDescWriteType::Only;
    }
    context.handle_check_event(packet_only);
    let desc = device.work_pop().expect("should get a ack");
    if let ToCardWorkRbDesc::WriteWithImm(desc) = *desc {
        assert_eq!(desc.common.dqpn, qpn);
        assert_eq!(desc.common.msn, msn);
    } else {
        panic!("should be an ack");
    }
}

#[test]
fn test_checker_miss_and_then_recover() {
    construct_context!(context, device, qpn = 0x1234);

    make_ref_packet_event!(packet_ref,qpn,start_psn=0,msn=0x1235,addr=0x500u32,len=4096*5);
    let packets = generate_range_of_packet(packet_ref, Pmtu::Mtu4096);
    assert_eq!(packets.len(), 6);
    // pkt 0 : first
    // pkt 1 : continuous,ignore
    // pkt 2 : miss
    // pkt 3 : psn=3, expected_psn=2
    // pkt 4 : continuous,ignore
    // pkt 5 : last, psn=5, expected_psn=5
    // should be [0,1] [3,5]
    let mut input_packets = [packets[0].clone(), packets[3].clone(), packets[5].clone()];
    set_expected_psn(&mut input_packets[1], Psn::new(2));
    set_expected_psn(&mut input_packets[2], Psn::new(5));

    context.handle_check_event(input_packets[0].clone());
    check_qp_status(&context, qpn, QpStatus::Normal);

    context.handle_check_event(input_packets[1].clone());
    check_qp_status(&context, qpn, QpStatus::OutOfOrder);
    check_recv_ctx_flag_and_intervals(&context, qpn, msn, false, true, 2);

    context.handle_check_event(input_packets[2].clone());
    check_qp_status(&context, qpn, QpStatus::OutOfOrder);
    check_recv_ctx_flag_and_intervals(&context, qpn, msn, false, true, 2);

    for i in 0..6 {
        let mut pkt = packets[i].clone();
        set_expected_psn(&mut pkt, Psn::new(6));
        context.handle_check_event(pkt);
    }
    check_recv_ctx_exist(&context, qpn, msn, false);
    let desc = device.work_pop().expect("should get a ack");
    if let ToCardWorkRbDesc::WriteWithImm(desc) = *desc {
        assert_eq!(desc.common.dqpn, qpn);
        assert_eq!(desc.common.msn, msn);
        assert!(matches!(desc.common.qp_type, QpType::RawPacket));
    } else {
        panic!("should be an ack");
    }

    device.ctrl_pop_and_exec_handler(true);
    check_qp_status(&context, qpn, QpStatus::Normal);
}

#[test]
fn test_checker_out_of_order_and_miss() {
    construct_context!(context, device, qpn = 0x1234);

    // case 1
    // ID
    // [0]pkt 0 : first
    // [1]pkt 5 : last, psn=5,expected_psn=1
    // [2]pkt 3 : psn=3,expected_psn=6
    // [3]pkt 1 : psn=1,expected_psn=6
    // [4]pkt 4 : psn=4,expected_psn=6
    // should be [0-1] [3-5]
    make_ref_packet_event!(packet_ref,qpn,start_psn=0,msn=0x1235,addr=0x500u32,len=4096*5);
    let packets = generate_range_of_packet(packet_ref, Pmtu::Mtu4096);
    assert_eq!(packets.len(), 6);
    let mut input_packets = [
        packets[0].clone(),
        packets[5].clone(),
        packets[3].clone(),
        packets[1].clone(),
        packets[4].clone(),
    ];
    set_expected_psn(&mut input_packets[1], Psn::new(1));
    set_expected_psn(&mut input_packets[2], Psn::new(6));
    set_expected_psn(&mut input_packets[3], Psn::new(6));
    set_expected_psn(&mut input_packets[4], Psn::new(6));

    context.handle_check_event(input_packets[0].clone());
    check_qp_status(&context, qpn, QpStatus::Normal);

    context.handle_check_event(input_packets[1].clone());
    check_qp_status(&context, qpn, QpStatus::OutOfOrder);
    check_recv_ctx_flag_and_intervals(&context, qpn, msn, false, true, 2);

    context.handle_check_event(input_packets[2].clone());
    check_recv_ctx_flag_and_intervals(&context, qpn, msn, false, true, 3);

    context.handle_check_event(input_packets[3].clone());
    check_recv_ctx_flag_and_intervals(&context, qpn, msn, false, true, 3);

    context.handle_check_event(input_packets[4].clone());
    check_recv_ctx_flag_and_intervals(&context, qpn, msn, false, true, 2);

    for i in 0..6 {
        let mut pkt = packets[i].clone();
        set_expected_psn(&mut pkt, Psn::new(6));
        context.handle_check_event(pkt);
    }
    check_recv_ctx_exist(&context, qpn, msn, false);
    let desc = device.work_pop().expect("should get a ack");
    if let ToCardWorkRbDesc::WriteWithImm(desc) = *desc {
        assert_eq!(desc.common.dqpn, qpn);
        assert_eq!(desc.common.msn, msn);
        assert!(matches!(desc.common.qp_type, QpType::RawPacket));
    } else {
        panic!("should be an ack");
    }

    device.ctrl_pop_and_exec_handler(true);
    check_qp_status(&context, qpn, QpStatus::Normal);

    // case 2
    // start_psn=6, end_psn=12, total=7 packets
    //
    // [0]pkt 6 : first,psn=6,expected_psn=6   [6]
    // [1]pkt 8 : psn=8,expected_psn=7     [6] [8]
    // [2]pkt 7 : psn=7,expected_psn=9    [6,8] in order
    // [3]pkt 11: psn=11,expected_psn=10  [6,9] [11]
    // [4]pkt 10: psn=10,expected_psn=12  [6,11],in order

    make_ref_packet_event!(packet_ref,qpn,start_psn=6,msn=0x1236,addr=0x500u32,len=4096*6);
    let packets = generate_range_of_packet(packet_ref, Pmtu::Mtu4096);
    assert_eq!(packets.len(), 7);
    let mut input_packets = [
        packets[0].clone(),
        packets[2].clone(),
        packets[1].clone(),
        packets[5].clone(),
        packets[4].clone(),
        packets[6].clone(),
    ];
    set_expected_psn(&mut input_packets[1], Psn::new(7));
    set_expected_psn(&mut input_packets[2], Psn::new(9));
    set_expected_psn(&mut input_packets[3], Psn::new(10));
    set_expected_psn(&mut input_packets[4], Psn::new(12));

    context.handle_check_event(input_packets[0].clone());
    check_qp_status(&context, qpn, QpStatus::Normal);

    context.handle_check_event(input_packets[1].clone());
    check_qp_status(&context, qpn, QpStatus::OutOfOrder);
    check_recv_ctx_flag_and_intervals(&context, qpn, msn, false, true, 2);

    context.handle_check_event(input_packets[2].clone());
    check_recv_ctx_flag_and_intervals(&context, qpn, msn, false, false, 1);

    context.handle_check_event(input_packets[3].clone());
    check_recv_ctx_flag_and_intervals(&context, qpn, msn, false, true, 2);
    device.ctrl_pop_and_exec_handler(false);
    check_qp_status(&context, qpn, QpStatus::OutOfOrder);

    context.handle_check_event(input_packets[4].clone());
    check_recv_ctx_flag_and_intervals(&context, qpn, msn, false, false, 1);

    context.handle_check_event(input_packets[5].clone());
    check_recv_ctx_exist(&context, qpn, msn, false);
    device.ctrl_pop_and_exec_handler(true);
    check_qp_status(&context, qpn, QpStatus::Normal);

    assert!(device.work_pop().is_some())
}

#[test]
fn test_checker_receive_out_of_order_first() {
    construct_context!(context, device, qpn = 0x1234);

    // case 1
    // [0] psn = 5, middl,expected_psn = 0
    make_ref_packet_event!(packet_ref,qpn,start_psn=5,msn=0x1235,addr=0x500u32,len=4096*5);

    let mut pkt = packet_ref.clone();
    set_expected_psn(&mut pkt, Psn::new(0));
    set_write_type(&mut pkt, ToHostWorkRbDescWriteType::Middle);
    context.handle_check_event(pkt);
    check_qp_status(&context, qpn, QpStatus::OutOfOrder);

    // case 2
    // [1] psn=5, first,expected_psn=0
}

#[test]
fn test_checker_miss_packet_recover_and_miss_again() {}

#[test]
fn test_checker_miss_multiple_transaction_and_then_recover() {}

#[test]
fn test_checker_recover_failed() {}

#[derive(Debug, Default)]
struct MockCtrlDescSender {
    ctrl_queue: Mutex<Vec<(ToCardCtrlRbDesc, CtrlOpCtx)>>,
    work_queue: Mutex<Vec<Box<ToCardWorkRbDesc>>>,
}
impl MockCtrlDescSender {
    fn ctrl_pop_and_exec_handler(&self, is_succ: bool) {
        if let Some((_, ctx)) = self.ctrl_queue.lock().pop() {
            if let Some(handler) = ctx.take_handler() {
                handler(is_succ);
            }
        }
    }

    fn work_pop(&self) -> Option<Box<ToCardWorkRbDesc>> {
        self.work_queue.lock().pop()
    }
}
impl CtrlDescriptorSender for MockCtrlDescSender {
    fn send_ctrl_desc(&self, desc: ToCardCtrlRbDesc) -> Result<CtrlOpCtx, crate::Error> {
        let ctx = CtrlOpCtx::new_running();
        self.ctrl_queue.lock().push((desc, ctx.clone()));
        Ok(ctx)
    }
}

impl WorkDescriptorSender for MockCtrlDescSender {
    fn send_work_desc(&self, desc: Box<ToCardWorkRbDesc>) -> Result<(), crate::Error> {
        self.work_queue.lock().push(desc);
        Ok(())
    }
}
const BUFFER_SIZE: usize = 1024 * RDMA_ACK_BUFFER_SLOT_SIZE;

#[derive(Builder)]
struct PacketWrite {
    dqpn: Qpn,
    msn: Msn,
    #[builder(setter(into, strip_option), default)]
    expected_psn: Option<Psn>,
    psn: Psn,
    write_type: ToHostWorkRbDescWriteType,
    #[builder(setter(into, strip_option), default)]
    is_read_resp: Option<bool>,
    #[builder(setter(into, strip_option), default)]
    can_auto_ack: Option<bool>,
    #[builder(setter(into, strip_option), default)]
    addr: Option<u64>,
    #[builder(setter(into, strip_option), default)]
    len: Option<u32>,
}

impl From<PacketWrite> for PacketCheckEvent {
    fn from(value: PacketWrite) -> Self {
        PacketCheckEvent::Write(ToHostWorkRbDescWriteOrReadResp {
            common: ToHostWorkRbDescCommon {
                dqpn: value.dqpn,
                msn: value.msn,
                expected_psn: value.expected_psn.unwrap_or(value.psn),
                ..Default::default()
            },
            psn: value.psn,
            write_type: value.write_type,
            can_auto_ack: value.can_auto_ack.unwrap_or(false),
            addr: value.addr.unwrap_or(0),
            len: value.len.unwrap_or(0),
            is_read_resp: value.is_read_resp.unwrap_or(false),
        })
    }
}

fn set_write_type(pkt: &mut PacketCheckEvent, write_type: ToHostWorkRbDescWriteType) {
    update(pkt, |desc| {
        desc.write_type = write_type.clone();
    });
}

fn set_expected_psn(pkt: &mut PacketCheckEvent, psn: Psn) {
    update(pkt, |desc| {
        desc.common.expected_psn = psn;
    });
}

fn update<F: Fn(&mut ToHostWorkRbDescWriteOrReadResp)>(pkt: &mut PacketCheckEvent, f: F) {
    if let PacketCheckEvent::Write(ref mut e) = pkt {
        f(e)
    }
}

/// Generate a range of packets
///
/// if set_start is true, the first packet will be set as First
/// if set_end is true, the last packet will be set as Last
fn generate_range_of_packet(ref_pkt: PacketCheckEvent, pmtu: Pmtu) -> Vec<PacketCheckEvent> {
    let pkt = ref_pkt.clone();
    let (start_psn, start_addr, length) = if let PacketCheckEvent::Write(ref e) = pkt {
        (e.psn, e.addr, e.len)
    } else {
        panic!("pkt should be a write event")
    };
    let count = calculate_packet_cnt(pmtu, start_addr, length);
    let mut results = Vec::with_capacity(count as usize);
    for i in 0..count {
        let mut pkt = pkt.clone();
        update(&mut pkt, |desc| {
            if i == 0 {
                desc.write_type = ToHostWorkRbDescWriteType::First;
                desc.addr = start_addr;
                desc.len = length
            } else if i == count - 1 {
                desc.write_type = ToHostWorkRbDescWriteType::Last;
                desc.addr =
                    start_addr - (start_addr % u64::from(&pmtu)) + i as u64 * u64::from(&pmtu);
                let first_pkt_length = get_first_packet_max_length(start_addr, u32::from(&pmtu));
                desc.len = (length - first_pkt_length) % u32::from(&pmtu);
            } else {
                desc.write_type = ToHostWorkRbDescWriteType::Middle;
                desc.addr =
                    start_addr - (start_addr % u64::from(&pmtu)) + i as u64 * u64::from(&pmtu);
                desc.len = u32::from(&pmtu);
            }
            desc.psn = start_psn.wrapping_add(i);
            desc.common.expected_psn = desc.psn;
        });
        results.push(pkt);
    }
    results
    // advance psn, addr, change length
}

fn check_qp_status(ctx: &PacketCheckerContext, qpn: Qpn, status: QpStatus) {
    let qp_status = ctx
        .qp_table
        .read()
        .get(&qpn)
        .unwrap()
        .status
        .load(Ordering::Acquire);
    assert_eq!(qp_status, status);
}

fn check_recv_ctx_exist(ctx: &PacketCheckerContext, qpn: Qpn, msn: Msn, is_exist: bool) {
    assert_eq!(ctx.recv_ctx_map.get_ctx_mut(qpn, msn).is_some(), is_exist)
}
fn check_recv_ctx_flag_and_intervals(
    ctx: &PacketCheckerContext,
    qpn: Qpn,
    msn: Msn,
    is_complete: bool,
    is_out_of_order: bool,
    intervals: usize,
) {
    let ctx = ctx.recv_ctx_map.get_ctx_mut(qpn, msn).unwrap();
    let wnd = ctx.window().unwrap();
    println!("{:?}", wnd);
    assert_eq!(wnd.is_complete(), is_complete);
    assert_eq!(wnd.is_out_of_order(), is_out_of_order);
    assert_eq!(wnd.get_intervals_len(), intervals);
}
