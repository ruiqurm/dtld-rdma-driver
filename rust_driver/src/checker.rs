use std::{
    cell::{RefCell, RefMut},
    collections::{BTreeMap, HashMap},
    ops::Bound,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
};

use crate::{
    buf::{PacketBuf, RDMA_ACK_BUFFER_SLOT_SIZE},
    device::{
        ToCardCtrlRbDesc, ToCardCtrlRbDescCommon, ToCardCtrlRbDescUpdateErrPsnRecoverPoint,
        ToHostWorkRbDescAck, ToHostWorkRbDescAethCode, ToHostWorkRbDescRead,
        ToHostWorkRbDescWriteOrReadResp, ToHostWorkRbDescWriteType,
    },
    op_ctx::OpCtx,
    qp::QpContext,
    responser::make_ack,
    types::{Msn, Pmtu, Psn, Qpn},
    CtrlDescriptorSender, ThreadSafeHashmap, WorkDescriptorSender,
};

use flume::{Receiver, TryRecvError};

use log::{error, info};

#[derive(Debug)]
pub(crate) struct PacketChecker {
    thread: Option<std::thread::JoinHandle<()>>,
    stop_flag: Arc<AtomicBool>,
}

pub(crate) struct PacketCheckerContext {
    pub(crate) desc_poller_channel: Receiver<PacketCheckEvent>,
    pub(crate) recv_ctx_map: RecvContextMap,
    pub(crate) qp_table: ThreadSafeHashmap<Qpn, QpContext>,
    pub(crate) user_op_ctx_map: ThreadSafeHashmap<(Qpn, Msn), OpCtx<()>>,
    pub(crate) ctrl_desc_sender: Arc<dyn CtrlDescriptorSender>,
    pub(crate) work_desc_sender: Arc<dyn WorkDescriptorSender>,
    pub(crate) ack_buffers: PacketBuf<RDMA_ACK_BUFFER_SLOT_SIZE>,
}

impl PacketChecker {
    pub(crate) fn new(mut context: PacketCheckerContext) -> Self {
        let stop_flag = Arc::new(AtomicBool::new(false));
        let thread_stop_flag = Arc::clone(&stop_flag);
        let thread = std::thread::spawn(move || {
            working_thread(&mut context, &thread_stop_flag);
        });
        Self {
            thread: Some(thread),
            stop_flag,
        }
    }
}

impl Drop for PacketChecker {
    fn drop(&mut self) {
        self.stop_flag.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            if let Err(e) = thread.join() {
                panic!("{}", format!("PacketChecker thread join failed: {e:?}"));
            }
            info!("PacketChecker thread is normally stopped");
        }
    }
}

fn working_thread(ctx: &mut PacketCheckerContext, stop_flag: &AtomicBool) {
    while !stop_flag.load(Ordering::Relaxed) {
        let result = ctx.desc_poller_channel.try_recv();
        match result {
            Err(TryRecvError::Disconnected) => {
                error!("PacketChecker is stopped due to pipe brocken");
                return;
            }
            Err(TryRecvError::Empty) => {}
            Ok(event) => {
                ctx.handle_check_event(event);
            }
        }
    }
}

impl PacketCheckerContext {
    fn handle_check_event(&mut self, event: PacketCheckEvent) {
        match event {
            PacketCheckEvent::Write(event) => {
                let qpn = event.common.dqpn;
                let expected_psn = event.common.expected_psn;
                let psn = event.psn;
                if expected_psn != psn {
                    self.enter_qp_error_status(qpn, expected_psn, psn);
                }
                let (is_normal, pmtu) = if let Some(qp) = self.qp_table.read().get(&qpn) {
                    (qp.status.load(Ordering::Acquire).is_normal(), qp.pmtu)
                } else {
                    return;
                };
                if is_normal {
                    self.handle_qp_normal(&event, pmtu);
                } else {
                    self.handle_qp_ooo(&event, pmtu);
                }
            }
            PacketCheckEvent::ReadReq(_) => {
                // convert read req directly
            }
            PacketCheckEvent::Ack(event) => {
                let code = event.code;
                let qpn = event.common.dqpn;
                let msn = event.common.msn;
                if matches!(code, ToHostWorkRbDescAethCode::Ack) {
                    self.wakeup_user_op_ctx(qpn, msn);
                }
            }
        }
    }

    #[inline]
    fn wakeup_user_op_ctx(&self, qpn: Qpn, msn: Msn) {
        if let Some(ctx) = self.user_op_ctx_map.read().get(&(qpn, msn)) {
            if let Err(e) = ctx.set_result(()) {
                error!("Set result failed {:?}", e);
            }
        } else {
            error!("No op ctx found for {:?}", (qpn, msn));
        }
    }

    #[allow(unused_results)]
    fn handle_qp_normal(&mut self, event: &ToHostWorkRbDescWriteOrReadResp, pmtu: Pmtu) {
        let qpn = event.common.dqpn;
        let msn = event.common.msn;

        match event.write_type {
            ToHostWorkRbDescWriteType::First => {
                let ctx = RecvContext::from(event);
                self.recv_ctx_map.insert_ctx(qpn, msn, ctx, pmtu);
            }
            ToHostWorkRbDescWriteType::Last => {
                self.recv_ctx_map.remove_ctx(qpn, msn);
                if event.is_read_resp {
                    self.wakeup_user_op_ctx(qpn, msn);
                } else if !event.can_auto_ack {
                    self.send_ack(qpn, msn, event.psn);
                }
            }
            ToHostWorkRbDescWriteType::Only => {
                if event.is_read_resp {
                    self.wakeup_user_op_ctx(qpn, msn);
                }
            }
            ToHostWorkRbDescWriteType::Middle => {}
        };
    }

    fn send_ack(&self, qpn: Qpn, msn: Msn, psn: Psn) {
        let mut slot = self.ack_buffers.recycle_buf();
        if let Ok(desc) = make_ack(slot, &self.qp_table, qpn, msn, psn) {
            if let Err(e) = self.work_desc_sender.send_work_desc(desc) {
                error!("Send ack failed {:?}", e);
            }
        } else {
            error!("send ack failed");
        }
    }

    // handle qp that out-of-order
    fn handle_qp_ooo(&mut self, event: &ToHostWorkRbDescWriteOrReadResp, pmtu: Pmtu) {
        let qpn = event.common.dqpn;
        let msn = event.common.msn;
        let mut need_check_completed_or_try_recover = false;
        match event.write_type {
            ToHostWorkRbDescWriteType::First => {
                let ctx = RecvContext::new_with_recvmap(event, u32::from(&pmtu));
                self.recv_ctx_map.insert_ctx(qpn, msn, ctx, pmtu);
            }
            ToHostWorkRbDescWriteType::Middle | ToHostWorkRbDescWriteType::Last => {
                if let Some(mut ctx) = self.recv_ctx_map.get_ctx_mut(qpn, msn){
                    ctx.recv_map.as_mut().unwrap().insert(event.psn);
                    need_check_completed_or_try_recover = true;
                }
                // otherwise, we ignore this packet
            }
            ToHostWorkRbDescWriteType::Only => {
                if event.is_read_resp {
                    self.wakeup_user_op_ctx(qpn, msn);
                }
            }
        };
        if need_check_completed_or_try_recover {
            self.check_completed_and_try_recover(qpn, msn);
        }
    }

    /// Check if corresponding msn is completed and try to recover the qp status
    fn check_completed_and_try_recover(&self, qpn: Qpn, msn: Msn) {
        let mut perqp_map = self.recv_ctx_map.get_per_qp_ctx_mut(qpn).unwrap();
        let (is_read_resp, is_completed, last_psn) = if let Some(ctx) = perqp_map.map.get_mut(&msn)
        {
            let recv_map = ctx.recv_map.as_ref().unwrap();
            (
                ctx.is_read_resp,
                recv_map.is_complete(),
                recv_map.last_psn(),
            )
        } else {
            (false, false, Psn::default())
        };

        // decrease borrow
        drop(perqp_map);

        if is_completed {
            self.recv_ctx_map.remove_ctx(qpn, msn);
            if is_read_resp {
                self.wakeup_user_op_ctx(qpn, msn);
            } else {
                // we should manually send ack the packet
                self.send_ack(qpn, msn, last_psn);
            }
        }

        let perqp_map = self.recv_ctx_map.get_per_qp_ctx_mut(qpn).unwrap();
        // if there is only one recv context and it's in-order,
        // we can try to recover the qp status
        if perqp_map.map.len() <= 1 {
            let (_, ctx) = perqp_map.map.iter().next().unwrap();
            let recv_map = ctx.recv_map.as_ref().unwrap();
            if let Some(recover_psn) = recv_map.try_get_recover_psn() {
                try_recover(&self.ctrl_desc_sender, qpn, recover_psn);
            }
            if perqp_map.map.is_empty() {
                self.recv_ctx_map.remove_per_qp_ctx(qpn);
            }
        }
    }

    // store the error status in qp
    fn enter_qp_error_status(&mut self, qpn: Qpn, expected_psn: Psn, recved_psn: Psn) {
        if let Some(qp) = self.qp_table.read().get(&qpn) {
            // set flag
            qp.status
                .store(crate::qp::QpStatus::OutOfOrder, Ordering::Release);
        };

        // create context for all msn
        if let Some(mut per_qp_map) = self.recv_ctx_map.get_per_qp_ctx_mut(qpn) {
            let pmtu = u32::from(&per_qp_map.pmtu);

            // we know that if we are previous in the normal status,
            // we should have only one recv context
            assert_eq!(per_qp_map.map.len(), 1);

            // the expected_psn is the psn that we should receive **next**
            let start_psn = expected_psn.wrapping_sub(1);
            for (_, ctx) in per_qp_map.map.iter_mut() {
                ctx.create_map_on_psn(start_psn, recved_psn, pmtu);
            }
        };
    }
}

fn try_recover(ctrl_desc_sender: &Arc<dyn CtrlDescriptorSender>, qpn: Qpn, recover_psn: Psn) {
    let desc =
        ToCardCtrlRbDesc::UpdateErrorPsnRecoverPoint(ToCardCtrlRbDescUpdateErrPsnRecoverPoint {
            common: ToCardCtrlRbDescCommon::default(),
            qpn,
            recover_psn,
        });
    if ctrl_desc_sender.send_ctrl_desc(desc).is_err() {
        error!("Send recover desc failed");
    }
}

/// The RecvContextMap is a map from (Qpn, Msn) to RecvContext
#[derive(Default)]
pub(crate) struct RecvContextMap(RefCell<HashMap<Qpn, PerQpContextMap>>);

struct PerQpContextMap {
    pmtu: Pmtu,
    map: BTreeMap<Msn, RecvContext>,
}

impl PerQpContextMap {
    fn new(pmtu: Pmtu) -> Self {
        Self {
            pmtu,
            map: BTreeMap::new(),
        }
    }
}

impl RecvContextMap {
    pub(crate) fn new() -> Self {
        Self(HashMap::new().into())
    }

    fn insert_ctx(&self, qpn: Qpn, msn: Msn, ctx: RecvContext, qp_pmtu: Pmtu) {
        // the `per qp context` might leak here?
        let mut inner = self.0.borrow_mut();
        let per_qp_map = inner.entry(qpn).or_insert(PerQpContextMap::new(qp_pmtu));
        if per_qp_map.map.insert(msn, ctx).is_some() {
            log::error!("create duplicate msn({:?}) record for qpn={:?}", msn, qpn);
        }
    }

    fn remove_ctx(&self, qpn: Qpn, msn: Msn) {
        let mut inner = self.0.borrow_mut();
        if let Some(per_qp_map) = inner.get_mut(&qpn) {
            let _dont_care = per_qp_map.map.remove(&msn);
        } else {
            log::error!("No recv ctx found for qpn={:?},msn={:?}", qpn, msn);
        }
    }

    fn remove_per_qp_ctx(&self, qpn: Qpn) {
        let mut inner = self.0.borrow_mut();
        let _dont_care = inner.remove(&qpn);
    }

    fn get_ctx_mut(&self, qpn: Qpn, msn: Msn) -> Option<RefMut<RecvContext>> {
        let should_ret_none = {
            let inner = self.0.borrow();
            if inner.contains_key(&qpn) {
                inner.get(&qpn).unwrap().map.contains_key(&msn)
            } else {
                false
            }
        };
        if !should_ret_none {
            None
        } else {
            Some(RefMut::map(self.0.borrow_mut(), |inner| {
                inner.get_mut(&qpn).unwrap().map.get_mut(&msn).unwrap()
            }))
        }
    }

    fn get_per_qp_ctx_mut(&self, qpn: Qpn) -> Option<RefMut<PerQpContextMap>> {
        let should_ret_none = {
            let inner = self.0.borrow();
            inner.contains_key(&qpn)
        };
        if !should_ret_none {
            None
        } else {
            Some(RefMut::map(self.0.borrow_mut(), |inner| {
                inner.get_mut(&qpn).unwrap()
            }))
        }
    }
}

#[inline]
fn get_pkt_length(real_payload_len: u32, addr: u64, pmtu: u32) -> u32 {
    let first_pkt_len = u64::from(pmtu) - (addr & (u64::from(pmtu) - 1));
    (1 + (u64::from(real_payload_len) - first_pkt_len).div_ceil(u64::from(pmtu))) as u32
}

struct RecvContext {
    is_read_resp: bool,
    start_addr: u64,
    len_in_bytes: u32,
    start_psn: Psn,
    recv_map: Option<Box<SlidingWindow>>,
}

impl RecvContext {
    pub(crate) fn new_with_recvmap(event: &ToHostWorkRbDescWriteOrReadResp, pmtu: u32) -> Self {
        let pkt_len = get_pkt_length(event.len, event.addr, pmtu);
        let mut map = Box::new(SlidingWindow::new(event.psn, pkt_len));
        map.insert(event.psn);
        Self {
            is_read_resp: event.is_read_resp,
            start_addr: event.addr,
            len_in_bytes: event.len,
            start_psn: event.psn,
            recv_map: Some(map),
        }
    }
    pub(crate) fn create_map_on_psn(&mut self, last_psn: Psn, recved_psn: Psn, pmtu: u32) {
        let pkt_len = get_pkt_length(self.len_in_bytes, self.start_addr, pmtu);
        let packet_remain = pkt_len - last_psn.wrapping_abs(self.start_psn) + 1;
        let mut map = Box::new(SlidingWindow::new(last_psn, packet_remain));
        map.insert(last_psn);
        map.insert(recved_psn);
        self.recv_map = Some(map);
    }
}

impl From<&ToHostWorkRbDescWriteOrReadResp> for RecvContext {
    fn from(event: &ToHostWorkRbDescWriteOrReadResp) -> Self {
        RecvContext {
            is_read_resp: event.is_read_resp,
            start_addr: event.addr,
            len_in_bytes: event.len,
            start_psn: event.psn,
            recv_map: None,
        }
    }
}

#[derive(Debug)]
struct SlidingWindow {
    intervals: BTreeMap<u32, u32>,
    start_psn: Psn,
    recent_abs_psn: u32,
    recent_rel_psn: Psn,
    num_of_packets: u32,
}

impl SlidingWindow {
    const MAX_WINDOW_SIZE: u32 = 1 << 23_i32;

    pub(crate) fn new(start: Psn, num_of_packets: u32) -> Self {
        Self {
            intervals: BTreeMap::new(),
            start_psn: start,
            recent_rel_psn: start,
            recent_abs_psn: 0,
            num_of_packets,
        }
    }

    #[allow(clippy::arithmetic_side_effects, clippy::unwrap_used)]
    pub(crate) fn insert(&mut self, psn: Psn) {
        if self.is_complete() {
            return;
        }

        let diff = psn.wrapping_sub(self.recent_rel_psn.get()).get();
        if diff >= Self::MAX_WINDOW_SIZE {
            return;
        }
        let abs_psn = self.recent_abs_psn.wrapping_add(diff);

        if self.intervals.is_empty() {
            let _: Option<u32> = self.intervals.insert(abs_psn, abs_psn);
            return;
        }
        let mut merge_left = None;
        let mut merge_right = None;

        if let Some((left_start, left_end)) = self
            .intervals
            .range((Bound::Unbounded, Bound::Included(abs_psn)))
            .next_back()
        {
            if abs_psn >= *left_start && abs_psn <= *left_end {
                return; // exist
            }

            if left_end + 1 == abs_psn {
                merge_left = Some((*left_start, *left_end));
            }
        }

        if let Some((right_start, right_end)) = self
            .intervals
            .range((Bound::Included(abs_psn), Bound::Unbounded))
            .next()
        {
            if abs_psn >= *right_start && abs_psn <= *right_end {
                return; // exist
            }

            if right_start - 1 == abs_psn {
                merge_right = Some((*right_start, *right_end));
            }
        }

        match (merge_left, merge_right) {
            (Some((left_start, _)), Some((right_start, right_end))) => {
                let _: Option<u32> = self.intervals.remove(&left_start);
                let _: Option<u32> = self.intervals.remove(&right_start);
                let _: Option<u32> = self.intervals.insert(left_start, right_end);
            }
            (Some((left_start, _)), None) => {
                let _: Option<u32> = self.intervals.remove(&left_start);
                let _: Option<u32> = self.intervals.insert(left_start, abs_psn);
            }
            (None, Some((right_start, right_end))) => {
                let _: Option<u32> = self.intervals.remove(&right_start);
                let _: Option<u32> = self.intervals.insert(abs_psn, right_end);
            }
            (None, None) => {
                let _: Option<u32> = self.intervals.insert(abs_psn, abs_psn);
            }
        }
        let (_start, end) = self.intervals.first_key_value().unwrap(); // safe to unwrap
        if *end > self.recent_abs_psn {
            self.recent_abs_psn = abs_psn;
            self.recent_rel_psn = psn;
        }
    }

    #[allow(clippy::arithmetic_side_effects)]
    pub(crate) fn is_complete(&self) -> bool {
        if self.intervals.is_empty() {
            return false;
        }
        let (start, end) = self.intervals.first_key_value().unwrap_or((&0, &0));
        *end == self.num_of_packets - 1 && *start == 0
    }

    pub(crate) fn try_get_recover_psn(&self) -> Option<Psn> {
        if !self.is_out_of_order() {
            let offset_of_next_expected = self.intervals.get(&0).unwrap() + 1;
            Some(self.start_psn.wrapping_add(offset_of_next_expected))
        } else {
            None
        }
    }

    pub(crate) fn last_psn(&self) -> Psn {
        self.start_psn.wrapping_add(self.num_of_packets - 1)
    }

    fn is_out_of_order(&self) -> bool {
        !self.is_complete() && self.intervals.len() > 1
    }
}

pub(crate) enum PacketCheckEvent {
    Write(ToHostWorkRbDescWriteOrReadResp),
    Ack(ToHostWorkRbDescAck),
    ReadReq(ToHostWorkRbDescRead),
}

impl From<ToHostWorkRbDescWriteOrReadResp> for PacketCheckEvent {
    fn from(desc: ToHostWorkRbDescWriteOrReadResp) -> Self {
        Self::Write(desc)
    }
}

impl From<ToHostWorkRbDescAck> for PacketCheckEvent {
    fn from(desc: ToHostWorkRbDescAck) -> Self {
        Self::Ack(desc)
    }
}

impl From<ToHostWorkRbDescRead> for PacketCheckEvent {
    fn from(desc: ToHostWorkRbDescRead) -> Self {
        Self::ReadReq(desc)
    }
}

impl Default for PacketCheckEvent {
    fn default() -> Self {
        Self::Write(ToHostWorkRbDescWriteOrReadResp::default())
    }
}

#[cfg(test)]
mod tests {

    use crate::types::Psn;

    #[test]
    fn test_sliding_window() {
        let start = 0;
        let n = 10;

        // test miss one
        for miss in 1..n - 1 {
            let mut window = super::SlidingWindow::new(Psn::new(start), n);
            for i in 0..n {
                if i != miss {
                    window.insert(Psn::new(i));
                }
            }
            assert!(!window.is_complete());
            assert!(window.is_out_of_order());
            window.insert(Psn::new(miss));
            assert!(window.is_complete(), "miss={}", miss);
            assert!(!window.is_out_of_order());
        }
        // inseert same psn
        {
            let mut window = super::SlidingWindow::new(Psn::new(start), n);
            for _ in 0..n {
                window.insert(Psn::new(0));
            }
            assert!(!window.is_complete());
        }

        // test miss multiple,except one
        for mod_num in [2u32, 3, 4] {
            let mut window = super::SlidingWindow::new(Psn::new(start), n);
            window.insert(Psn::new(0));
            for i in 1..n {
                if i % mod_num != 0 {
                    window.insert(Psn::new(i));
                }
            }
            assert!(!window.is_complete());
            assert!(window.is_out_of_order());
            for i in 1..n {
                if i % mod_num == 0 {
                    window.insert(Psn::new(i));
                }
            }
            assert!(window.is_complete());
        }

        // test miss multiple,except one
        for mod_num in [2u32, 3, 4] {
            let mut window = super::SlidingWindow::new(Psn::new(start), n);
            window.insert(Psn::new(0));
            for i in 1..n {
                if i % mod_num != 0 {
                    window.insert(Psn::new(i));
                }
            }
            assert!(!window.is_complete());
            assert!(window.is_out_of_order());
            for i in 1..n {
                if i % mod_num == 0 {
                    window.insert(Psn::new(i));
                }
            }
            assert!(window.is_complete());
        }
        // test reverse miss multiple,except one
        for mod_num in [2u32, 3, 4] {
            let mut window = super::SlidingWindow::new(Psn::new(start), n);
            window.insert(Psn::new(0));
            for i in 1..n {
                if i % mod_num != 0 {
                    window.insert(Psn::new(i));
                }
            }
            assert!(!window.is_complete());
            assert!(window.is_out_of_order());
            for i in (0..n).rev() {
                if i % mod_num == 0 {
                    window.insert(Psn::new(i));
                }
            }
            assert!(window.is_complete());
        }
        // test cross border
        let n = 20;
        {
            let base = Psn::new(Psn::MAX_VALUE - 10);
            let mut window = super::SlidingWindow::new(base, n);
            for i in 0..n - 1 {
                let next = base.wrapping_add(i);
                window.insert(next);
                assert!(!window.is_complete());
            }
            window.insert(base.wrapping_add(n - 1));
            assert!(window.is_complete());
        }

        // test outside range
        {
            let mut window = super::SlidingWindow::new(Psn::new(start), n);
            window.insert(Psn::new(start));
            assert_eq!(window.intervals.len(), 1);
            window.insert(Psn::new(Psn::MAX_VALUE)); // not in range
            assert_eq!(window.intervals.len(), 1);
        }
    }
    // #[test]
    // fn test_packet_checker() {
    //     let (resp_sender, _resp_receiver) = unbounded();
    //     let (desc_poller_sender, desc_poller_receiver) = unbounded();
    //     let user_op_ctx_map = Arc::new(RwLock::new(HashMap::<(Qpn, Msn), OpCtx<()>>::new()));
    //     let context = PacketCheckerContext {
    //         resp_channel: resp_sender,
    //         desc_poller_channel: desc_poller_receiver,
    //         recv_ctx_map: HashMap::new(),
    //         user_op_ctx_map: Arc::<RwLock<HashMap<(Qpn, Msn), OpCtx<()>>>>::clone(&user_op_ctx_map),
    //     };
    //     let _checker = PacketChecker::new(context);
    //     user_op_ctx_map
    //         .write()
    //         .insert((Qpn::new(2), Msn::default()), OpCtx::new_running());
    //     desc_poller_sender
    //         .send(PacketCheckEvent {
    //             qpn: Qpn::new(2),
    //             psn: Psn::new(0),
    //             type_: PacketCheckEventType::First,
    //             expected_psn: Psn::new(0),
    //             is_read_resp: true,
    //             ..Default::default()
    //         })
    //         .unwrap();
    //     desc_poller_sender
    //         .send(PacketCheckEvent {
    //             qpn: Qpn::new(2),
    //             psn: Psn::new(3),
    //             type_: PacketCheckEventType::Last,
    //             expected_psn: Psn::new(3),
    //             is_read_resp: true,
    //             ..Default::default()
    //         })
    //         .unwrap();
    //     sleep(Duration::from_millis(1));
    //     let guard = user_op_ctx_map.read();
    //     let ctx = guard.get(&(Qpn::new(2), Msn::default())).unwrap();
    //     assert!(ctx.get_result().is_some());
    // }
}
