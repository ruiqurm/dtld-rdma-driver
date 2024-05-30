use std::{
    collections::{BTreeMap, HashMap},
    ops::Bound,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
};

use crate::{
    device::{
        ToHostWorkRbDescAck, ToHostWorkRbDescNack, ToHostWorkRbDescWriteOrReadResp,
        ToHostWorkRbDescWriteType,
    },
    op_ctx::OpCtx,
    responser::RespCommand,
    types::{Msn, Psn, Qpn},
    Error, ThreadSafeHashmap,
};

use flume::{Receiver, Sender, TryRecvError};

use log::{error, info};

#[derive(Debug)]
pub(crate) struct PacketChecker {
    thread: Option<std::thread::JoinHandle<()>>,
    stop_flag: Arc<AtomicBool>,
}

enum PacketCheckEventType {
    First,
    Middle,
    Last,
    Only,
    Ack,
    Nack,
}

pub(crate) struct PacketCheckEvent {
    pub(crate) qpn: Qpn,
    pub(crate) msn: Msn,
    pub(crate) psn: Psn,
    type_: PacketCheckEventType,
    expected_psn: Psn,
    pub(crate) is_read_resp: bool,
}

impl Default for PacketCheckEvent {
    fn default() -> Self {
        Self {
            qpn: Qpn::default(),
            msn: Msn::default(),
            psn: Psn::default(),
            type_: PacketCheckEventType::Only,
            expected_psn: Psn::default(),
            is_read_resp: false,
        }
    }
}

pub(crate) struct PacketCheckerContext {
    pub(crate) resp_channel: Sender<RespCommand>,
    pub(crate) desc_poller_channel: Receiver<PacketCheckEvent>,
    pub(crate) recv_ctx_map: HashMap<(Qpn, Msn), RecvContext>,
    pub(crate) user_op_ctx_map: ThreadSafeHashmap<(Qpn, Msn), OpCtx<()>>,
}

impl PacketChecker {
    pub(crate) fn new(mut context: PacketCheckerContext) -> Self {
        let stop_flag = Arc::new(AtomicBool::new(false));
        let thread_stop_flag = Arc::clone(&stop_flag);
        let thread = std::thread::spawn(move || {
            PacketCheckerContext::working_thread(&mut context, &thread_stop_flag);
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

impl PacketCheckerContext {
    fn working_thread(ctx: &mut Self, stop_flag: &AtomicBool) {
        while !stop_flag.load(Ordering::Relaxed) {
            if let Err(e) = ctx.handle_check_packet_event() {
                error!("PacketChecker is stopped due to: {:?}", e);
                return;
            }
        }
    }

    fn handle_check_packet_event(&mut self) -> Result<(), Error> {
        loop {
            let result = self.desc_poller_channel.try_recv();
            match result {
                Err(TryRecvError::Disconnected) => {
                    return Err(Error::PipeBroken("packet checker recv queue"));
                }
                Err(TryRecvError::Empty) => return Ok(()),
                Ok(event) => self.handle_qp_normal(event),
            }
        }
    }

    #[inline]
    fn wakeup_user_op_ctx(&self, event: &PacketCheckEvent) {
        if let Some(ctx) = self.user_op_ctx_map.read().get(&(event.qpn, event.msn)) {
            if let Err(e) = ctx.set_result(()) {
                error!("Set result failed {:?}", e);
            }
        } else {
            error!("No read op ctx found for {:?}", (event.qpn, event.msn));
        }
    }

    #[allow(unused_results)]
    fn handle_qp_normal(&mut self, event: PacketCheckEvent) {
        match event.type_ {
            PacketCheckEventType::First => {
                if self
                    .recv_ctx_map
                    .insert((event.qpn, event.msn), RecvContext::from(event))
                    .is_some()
                {
                    log::error!("Receive same record more than once");
                }
            }
            PacketCheckEventType::Last => {
                if self.recv_ctx_map.remove(&(event.qpn, event.msn)).is_none() {
                    log::error!("No recv ctx found for {:?}", (event.qpn, event.msn));
                }
                if event.is_read_resp {
                    self.wakeup_user_op_ctx(&event);
                }
            }
            PacketCheckEventType::Only => {
                if event.is_read_resp {
                    self.wakeup_user_op_ctx(&event);
                }
            }
            PacketCheckEventType::Ack => {
                self.wakeup_user_op_ctx(&event);
            }
            PacketCheckEventType::Nack | PacketCheckEventType::Middle => {}
        };
    }
    // fn check_pkt_map(&self) -> Result<(), Error> {
    //     let mut remove_list = LinkedList::new();
    //     let iter_maps = {
    //         let guard = self.recv_ctx_map.read();
    //         guard
    //             .iter()
    //             .map(|(k, v)| (*k, Arc::clone(v)))
    //             .collect::<Vec<_>>()
    //     };
    //     for (msn, map) in iter_maps {
    //         let dqpn = map.dqpn();
    //         let end_psn = map.end_psn();
    //         let is_read_resp = map.is_read_resp;
    //         let (is_complete, is_out_of_order) = map.check_status();

    //         // send ack
    //         if is_complete {
    //             info!("Complete: {:?}", &msn);
    //             if !is_read_resp {
    //                 // If we are not in read response, we should send ack
    //                 let command =
    //                     RespCommand::Acknowledge(RespAckCommand::new_ack(dqpn, msn, end_psn));
    //                 self    resp_channel
    //                     .send(command)
    //                     .map_err(|_| Error::PipeBroken("packet checker send queue"))?;
    //             } else if let Some(ctx) = self.read_op_ctx_map.read().get(&msn) {
    //                 if let Err(e) = ctx.set_result(()) {
    //                     error!("Set result failed {:?}", e);
    //                 }
    //             } else {
    //                 error!("No read op ctx found for {:?}", msn);
    //             }
    //             remove_list.push_back(msn);
    //         } else if is_out_of_order {
    //             // TODO: what should we put in NACK packet?
    //             let command = RespCommand::Acknowledge(RespAckCommand::new_nack(
    //                 dqpn,
    //                 Msn::default(),
    //                 end_psn,
    //                 Psn::default(),
    //             ));
    //             self    resp_channel
    //                 .send(command)
    //                 .map_err(|_| Error::PipeBroken("packet checker send queue"))?;
    //             // In the previous discussion, we agreed that the first version does not need to implement NACK,
    //             // and we will panic when NACK needs to be sent.
    //             // FIXME:
    //             panic!("send nack command")
    //         } else {
    //             // everthing is fine, do nothing
    //         }
    //     }

    //     // remove the completed recv_ctx_map
    //     if !remove_list.is_empty() {
    //         let mut guard = self.recv_ctx_map.write();
    //         for dqpn in &remove_list {
    //             let _: Option<Arc<RecvPktMap>> = guard.remove(dqpn);
    //         }
    //     }
    //     Ok(())
    // }
}

pub(crate) struct RecvContext {
    dqpn: Qpn,
    msn: Msn,
    is_read_resp: bool,
    timeout: u128,
}

impl From<PacketCheckEvent> for RecvContext {
    fn from(event: PacketCheckEvent) -> Self {
        Self {
            dqpn: event.qpn,
            msn: event.msn,
            is_read_resp: event.is_read_resp,
            timeout: 0,
        }
    }
}

#[derive(Debug)]
struct SlidingWindow {
    intervals: BTreeMap<u32, u32>,
    recent_abs_psn: u32,
    recent_rel_psn: Psn,
    num_of_packets: u32,
}

impl SlidingWindow {
    const MAX_WINDOW_SIZE: u32 = 1 << 23_i32;

    pub(crate) fn new(start: Psn, num_of_packets: u32) -> Self {
        Self {
            intervals: BTreeMap::new(),
            recent_rel_psn: start,
            recent_abs_psn: 0,
            num_of_packets,
        }
    }

    #[allow(clippy::arithmetic_side_effects,clippy::unwrap_used)]
    pub(crate) fn insert(&mut self, psn: Psn) {
        let diff = psn.wrapping_sub(self.recent_rel_psn.get()).get();
        if diff >= Self::MAX_WINDOW_SIZE{
            return;
        }
        let abs_psn = self.recent_abs_psn.wrapping_add(diff);

        if self.intervals.is_empty() {
            let _: Option<u32> = self.intervals.insert(abs_psn, abs_psn);
            return;
        }

        let mut merge_left = None;
        let mut merge_right = None;

        if let Some((left_start,left_end)) = self
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

        if let Some((right_start,right_end)) = self
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
                let _: Option<u32> = self.intervals.insert(left_start,  right_end);
            }
            (Some((left_start, _)), None) => {
                let _: Option<u32> = self.intervals.remove(&left_start);
                let _: Option<u32> = self.intervals.insert(left_start,abs_psn);
            }
            (None, Some((right_start, right_end))) => {
                let _: Option<u32> = self.intervals.remove(&right_start);
                let _: Option<u32> = self.intervals.insert(abs_psn,  right_end);
            }
            (None, None) => {
                let _: Option<u32> = self.intervals.insert(abs_psn, abs_psn);
            }
        }
        let (_start,end) = self.intervals.first_key_value().unwrap(); // safe to unwrap
        if *end > self.recent_abs_psn{
            self.recent_abs_psn = *end;
            self.recent_rel_psn = psn;
        }
    }

    #[allow(clippy::arithmetic_side_effect,clippy::arithmetic_side_effects)]
    pub(crate) fn is_complete(&self) -> bool {
        if !(self.intervals.is_empty() && self.intervals.len() == 1){
            return false;
        }
        let (start,end) = self.intervals.first_key_value().unwrap_or((&0,&0));
        *end == self.num_of_packets - 1 && *start == 0
    }

    pub(crate) fn is_out_of_order(&self) -> bool {
        !self.is_complete() && self.intervals.len() > 1
    }
}

// impl RecvPktMap {
//     const FULL_CHUNK_DIV_BIT_SHIFT_CNT: u32 = 64usize.ilog2();
//     const LAST_CHUNK_MOD_MASK: usize = mem::size_of::<u32>() * 8 - 1;

//     #[allow(clippy::arithmetic_side_effects)]
//     pub(crate) fn new(is_read_resp: bool, pkt_cnt: usize, start_psn: Psn, dqpn: Qpn) -> Self {
//         let create_stage = |len| {
//             // used-bit count in the last u64, len % 64
//             let rem = len & Self::LAST_CHUNK_MOD_MASK;
//             // number of u64, ceil(len / 64)
//             let len = (len >> Self::FULL_CHUNK_DIV_BIT_SHIFT_CNT) + usize::from(rem != 0);
//             // last u64, lower `rem` bits are 1, higher bits are 0. if `rem == 0``, all bits are 1
//             let last_chunk = ((1u64 << rem) - 1) | u64::from(rem != 0).wrapping_sub_primitive(1);

//             (vec![0; len].into_boxed_slice(), last_chunk)
//         };

//         let (stage_0, stage_0_last_chunk) = create_stage(pkt_cnt);
//         let (stage_1, stage_1_last_chunk) = create_stage(stage_0.len());
//         let (stage_2, stage_2_last_chunk) = create_stage(stage_1.len());
//         // pkt_cnt is guaranteed to be less than 2^24, so the cast is safe
//         #[allow(clippy::cast_possible_truncation)]
//         Self {
//             is_read_resp,
//             start_psn,
//             end_psn: start_psn.wrapping_add(pkt_cnt as u32 - 1),
//             inner: RecvPktMapInner {
//                 stage_0,
//                 stage_0_last_chunk,
//                 stage_1,
//                 stage_1_last_chunk,
//                 stage_2,
//                 stage_2_last_chunk,
//                 last_pkt_psn: start_psn.wrapping_sub_primitive(1),
//                 is_out_of_order: false,
//             }
//             .into(),
//             dqpn,
//         }
//     }

//     #[allow(clippy::indexing_slicing)] // we will refactor this structure later
//     pub(crate) fn insert(&self, new_psn: Psn) {
//         let psn = (new_psn.wrapping_abs(self.start_psn)) as usize;
//         let stage_0_idx = psn >> Self::FULL_CHUNK_DIV_BIT_SHIFT_CNT; // which u64 in stage 0
//         let stage_0_rem = psn & Self::LAST_CHUNK_MOD_MASK; // bit position in u64
//         let stage_0_bit = 1 << stage_0_rem; // bit mask
//         let mut guard = self.inner.lock();
//         guard.stage_0[stage_0_idx] |= stage_0_bit; // set bit in stage 0

//         let is_stage_0_last_chunk = stage_0_idx == guard.stage_0.len().wrapping_sub_primitive(1); // is the bit in the last u64 in stage 0
//         let stage_0_chunk_expected =
//             u64::from(is_stage_0_last_chunk).wrapping_sub_primitive(1) | guard.stage_0_last_chunk; // expected bit mask of the target u64 in stage 0
//         let is_stage_0_chunk_complete = guard.stage_0[stage_0_idx] == stage_0_chunk_expected; // is the target u64 in stage 0 full

//         let stage_1_idx = stage_0_idx >> Self::FULL_CHUNK_DIV_BIT_SHIFT_CNT; // which u64 in stage 1
//         let stage_1_rem = stage_0_idx & Self::LAST_CHUNK_MOD_MASK; // bit position in u64
//         let stage_1_bit = u64::from(is_stage_0_chunk_complete) << stage_1_rem; // bit mask
//         guard.stage_1[stage_1_idx] |= stage_1_bit; // set bit in stage 1

//         let is_stage_1_last_chunk = stage_1_idx == guard.stage_1.len().wrapping_sub_primitive(1); // is the bit in the last u64 in stage 1
//         let stage_1_chunk_expected =
//             u64::from(is_stage_1_last_chunk).wrapping_sub_primitive(1) | guard.stage_1_last_chunk; // expected bit mask of the target u64 in stage 1
//         let is_stage_1_chunk_complete = guard.stage_1[stage_1_idx] == stage_1_chunk_expected; // is the target u64 in stage 1 full

//         let stage_2_idx = stage_1_idx >> Self::FULL_CHUNK_DIV_BIT_SHIFT_CNT; // which u64 in stage 2
//         let stage_2_rem = stage_1_idx & Self::LAST_CHUNK_MOD_MASK; // bit position in u64
//         let stage_2_bit = u64::from(is_stage_1_chunk_complete) << stage_2_rem; // bit mask
//         guard.stage_2[stage_2_idx] |= stage_2_bit; // set bit in stage 2
//         if guard.last_pkt_psn.wrapping_add(1) != new_psn {
//             guard.is_out_of_order = true;
//         }
//         guard.last_pkt_psn = new_psn;
//     }

//     /// check if is complete and is out of order
//     pub(crate) fn check_status(&self) -> (bool, bool) {
//         let guard = self.inner.lock();
//         let is_out_of_order = guard.is_out_of_order;
//         let is_complete = guard
//             .stage_2
//             .iter()
//             .enumerate()
//             .fold(true, |acc, (idx, &bits)| {
//                 let is_last_chunk = idx == guard.stage_2.len().wrapping_sub_primitive(1);
//                 let chunk_expected =
//                     u64::from(is_last_chunk).wrapping_sub_primitive(1) | guard.stage_2_last_chunk;
//                 let is_chunk_complete = bits == chunk_expected;
//                 acc && is_chunk_complete
//             });
//         (is_complete, is_out_of_order)
//     }

//     pub(crate) fn dqpn(&self) -> Qpn {
//         self.dqpn
//     }

//     pub(crate) fn end_psn(&self) -> Psn {
//         self.end_psn
//     }
// }

impl From<ToHostWorkRbDescWriteType> for PacketCheckEventType {
    fn from(value: ToHostWorkRbDescWriteType) -> Self {
        match value {
            ToHostWorkRbDescWriteType::First => Self::First,
            ToHostWorkRbDescWriteType::Middle => Self::Middle,
            ToHostWorkRbDescWriteType::Last => Self::Last,
            ToHostWorkRbDescWriteType::Only => Self::Only,
        }
    }
}

impl From<ToHostWorkRbDescWriteOrReadResp> for PacketCheckEvent {
    fn from(desc: ToHostWorkRbDescWriteOrReadResp) -> Self {
        Self {
            qpn: desc.common.dqpn,
            msn: desc.common.msn,
            psn: desc.psn,
            type_: PacketCheckEventType::from(desc.write_type),
            expected_psn: desc.common.expected_psn,
            is_read_resp: desc.is_read_resp,
        }
    }
}

impl From<ToHostWorkRbDescAck> for PacketCheckEvent {
    fn from(desc: ToHostWorkRbDescAck) -> Self {
        Self {
            qpn: desc.common.dqpn,
            msn: desc.common.msn,
            psn: desc.psn,
            type_: PacketCheckEventType::Ack,
            expected_psn: desc.common.expected_psn,
            is_read_resp: false,
        }
    }
}

impl From<ToHostWorkRbDescNack> for PacketCheckEvent {
    fn from(desc: ToHostWorkRbDescNack) -> Self {
        Self {
            qpn: desc.common.dqpn,
            msn: desc.common.msn,
            psn: desc.lost_psn.start,
            type_: PacketCheckEventType::Nack,
            expected_psn: desc.lost_psn.end,
            is_read_resp: false,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::HashMap, sync::Arc, thread::sleep, time::Duration};

    use crate::{
        checker::PacketCheckerContext,
        op_ctx::OpCtx,
        types::{Msn, Psn, Qpn},
    };

    use super::{PacketCheckEvent, PacketCheckEventType, PacketChecker};

    use flume::unbounded;
    use parking_lot::RwLock;

    #[test]
    fn test_packet_checker() {
        let (resp_sender, _resp_receiver) = unbounded();
        let (desc_poller_sender, desc_poller_receiver) = unbounded();
        let user_op_ctx_map = Arc::new(RwLock::new(HashMap::<(Qpn, Msn), OpCtx<()>>::new()));
        let context = PacketCheckerContext {
            resp_channel: resp_sender,
            desc_poller_channel: desc_poller_receiver,
            recv_ctx_map: HashMap::new(),
            user_op_ctx_map: Arc::<RwLock<HashMap<(Qpn, Msn), OpCtx<()>>>>::clone(&user_op_ctx_map),
        };
        let _checker = PacketChecker::new(context);
        user_op_ctx_map
            .write()
            .insert((Qpn::new(2), Msn::default()), OpCtx::new_running());
        desc_poller_sender
            .send(PacketCheckEvent {
                qpn: Qpn::new(2),
                psn: Psn::new(0),
                type_: PacketCheckEventType::First,
                expected_psn: Psn::new(0),
                is_read_resp: true,
                ..Default::default()
            })
            .unwrap();
        desc_poller_sender
            .send(PacketCheckEvent {
                qpn: Qpn::new(2),
                psn: Psn::new(3),
                type_: PacketCheckEventType::Last,
                expected_psn: Psn::new(3),
                is_read_resp: true,
                ..Default::default()
            })
            .unwrap();
        sleep(Duration::from_millis(1));
        let guard = user_op_ctx_map.read();
        let ctx = guard.get(&(Qpn::new(2), Msn::default())).unwrap();
        assert!(ctx.get_result().is_some());
    }
}
