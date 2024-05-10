use std::{
    collections::LinkedList,
    mem,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
};

use crate::{
    op_ctx::ReadOpCtx,
    responser::{RespAckCommand, RespCommand},
    types::{Msn, Psn, Qpn},
    Error, ThreadSafeHashmap,
};

use flume::Sender;
use parking_lot::Mutex;

use log::{error, info};

#[derive(Debug)]
pub(crate) struct PacketChecker {
    thread: Option<std::thread::JoinHandle<()>>,
    stop_flag: Arc<AtomicBool>,
}

impl PacketChecker {
    pub(crate) fn new(
        send_queue: Sender<RespCommand>,
        recv_pkt_map: ThreadSafeHashmap<Msn, Arc<RecvPktMap>>,
        read_op_ctx_map: ThreadSafeHashmap<Msn, ReadOpCtx>,
    ) -> Self {
        let ctx = PacketCheckerContext {
            send_queue,
            recv_pkt_map,
            read_op_ctx_map,
        };
        let stop_flag = Arc::new(AtomicBool::new(false));
        let thread_stop_flag = Arc::clone(&stop_flag);
        let thread = std::thread::spawn(move || {
            PacketCheckerContext::working_thread(&ctx, &thread_stop_flag);
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

struct PacketCheckerContext {
    send_queue: Sender<RespCommand>,
    recv_pkt_map: ThreadSafeHashmap<Msn, Arc<RecvPktMap>>,
    read_op_ctx_map: ThreadSafeHashmap<Msn, ReadOpCtx>,
}

impl PacketCheckerContext {
    fn working_thread(ctx: &Self, stop_flag: &AtomicBool) {
        while !stop_flag.load(Ordering::Relaxed) {
            if let Err(e) = ctx.check_pkt_map() {
                error!("PacketChecker is stopped due to: {:?}", e);
                return;
            }
        }
    }
    fn check_pkt_map(&self) -> Result<(), Error> {
        let mut remove_list = LinkedList::new();
        let iter_maps = {
            let guard = self.recv_pkt_map.read();
            guard
                .iter()
                .map(|(k, v)| (*k, Arc::clone(v)))
                .collect::<Vec<_>>()
        };
        for (msn, map) in iter_maps {
            let dqpn = map.dqpn();
            let end_psn = map.end_psn();
            let is_read_resp = map.is_read_resp;
            let (is_complete, is_out_of_order) = map.check_status();

            // send ack
            if is_complete {
                info!("Complete: {:?}", &msn);
                if !is_read_resp {
                    // If we are not in read response, we should send ack
                    let command =
                        RespCommand::Acknowledge(RespAckCommand::new_ack(dqpn, msn, end_psn));
                    self.send_queue
                        .send(command)
                        .map_err(|_| Error::PipeBroken("packet checker send queue"))?;
                } else if let Some(ctx) = self.read_op_ctx_map.read().get(&msn) {
                    if let Err(e) = ctx.set_result(()) {
                        error!("Set result failed {:?}", e);
                    }
                } else {
                    error!("No read op ctx found for {:?}", msn);
                }
                remove_list.push_back(msn);
            } else if is_out_of_order {
                // TODO: what should we put in NACK packet?
                let command = RespCommand::Acknowledge(RespAckCommand::new_nack(
                    dqpn,
                    Msn::default(),
                    end_psn,
                    Psn::default(),
                ));
                self.send_queue
                    .send(command)
                    .map_err(|_| Error::PipeBroken("packet checker send queue"))?;
                // In the previous discussion, we agreed that the first version does not need to implement NACK,
                // and we will panic when NACK needs to be sent.
                // FIXME:
                panic!("send nack command")
            } else {
                // everthing is fine, do nothing
            }
        }

        // remove the completed recv_pkt_map
        if !remove_list.is_empty() {
            let mut guard = self.recv_pkt_map.write();
            for dqpn in &remove_list {
                let _: Option<Arc<RecvPktMap>> = guard.remove(dqpn);
            }
        }
        Ok(())
    }
}

#[derive(Debug)]
pub(crate) struct RecvPktMap {
    inner: Mutex<RecvPktMapInner>,
    is_read_resp: bool,
    start_psn: Psn,
    end_psn: Psn,
    dqpn: Qpn,
}

#[derive(Debug)]
pub(crate) struct RecvPktMapInner {
    stage_0: Box<[u64]>,
    stage_0_last_chunk: u64,
    stage_1: Box<[u64]>,
    stage_1_last_chunk: u64,
    stage_2: Box<[u64]>,
    stage_2_last_chunk: u64,
    last_pkt_psn: Psn,
    is_out_of_order: bool,
}

impl RecvPktMap {
    const FULL_CHUNK_DIV_BIT_SHIFT_CNT: u32 = 64usize.ilog2();
    const LAST_CHUNK_MOD_MASK: usize = mem::size_of::<u64>() * 8 - 1;

    #[allow(clippy::arithmetic_side_effects)]
    pub(crate) fn new(is_read_resp: bool, pkt_cnt: usize, start_psn: Psn, dqpn: Qpn) -> Self {
        let create_stage = |len| {
            // used-bit count in the last u64, len % 64
            let rem = len & Self::LAST_CHUNK_MOD_MASK;
            // number of u64, ceil(len / 64)
            let len = (len >> Self::FULL_CHUNK_DIV_BIT_SHIFT_CNT) + usize::from(rem != 0);
            // last u64, lower `rem` bits are 1, higher bits are 0. if `rem == 0``, all bits are 1
            let last_chunk = ((1u64 << rem) - 1) | u64::from(rem != 0).wrapping_sub(1);

            (vec![0; len].into_boxed_slice(), last_chunk)
        };

        let (stage_0, stage_0_last_chunk) = create_stage(pkt_cnt);
        let (stage_1, stage_1_last_chunk) = create_stage(stage_0.len());
        let (stage_2, stage_2_last_chunk) = create_stage(stage_1.len());
        // pkt_cnt is guaranteed to be less than 2^24, so the cast is safe
        #[allow(clippy::cast_possible_truncation)]
        Self {
            is_read_resp,
            start_psn,
            end_psn: start_psn.wrapping_add(pkt_cnt as u32 - 1),
            inner: RecvPktMapInner {
                stage_0,
                stage_0_last_chunk,
                stage_1,
                stage_1_last_chunk,
                stage_2,
                stage_2_last_chunk,
                last_pkt_psn: start_psn.wrapping_sub(1),
                is_out_of_order: false,
            }
            .into(),
            dqpn,
        }
    }

    #[allow(clippy::indexing_slicing)] // we will refactor this structure later
    pub(crate) fn insert(&self, new_psn: Psn) {
        let psn = (new_psn.wrapping_abs(self.start_psn)) as usize;
        let stage_0_idx = psn >> Self::FULL_CHUNK_DIV_BIT_SHIFT_CNT; // which u64 in stage 0
        let stage_0_rem = psn & Self::LAST_CHUNK_MOD_MASK; // bit position in u64
        let stage_0_bit = 1 << stage_0_rem; // bit mask
        let mut guard = self.inner.lock();
        guard.stage_0[stage_0_idx] |= stage_0_bit; // set bit in stage 0

        let is_stage_0_last_chunk = stage_0_idx == guard.stage_0.len().wrapping_sub(1); // is the bit in the last u64 in stage 0
        let stage_0_chunk_expected =
            u64::from(is_stage_0_last_chunk).wrapping_sub(1) | guard.stage_0_last_chunk; // expected bit mask of the target u64 in stage 0
        let is_stage_0_chunk_complete = guard.stage_0[stage_0_idx] == stage_0_chunk_expected; // is the target u64 in stage 0 full

        let stage_1_idx = stage_0_idx >> Self::FULL_CHUNK_DIV_BIT_SHIFT_CNT; // which u64 in stage 1
        let stage_1_rem = stage_0_idx & Self::LAST_CHUNK_MOD_MASK; // bit position in u64
        let stage_1_bit = u64::from(is_stage_0_chunk_complete) << stage_1_rem; // bit mask
        guard.stage_1[stage_1_idx] |= stage_1_bit; // set bit in stage 1

        let is_stage_1_last_chunk = stage_1_idx == guard.stage_1.len().wrapping_sub(1); // is the bit in the last u64 in stage 1
        let stage_1_chunk_expected =
            u64::from(is_stage_1_last_chunk).wrapping_sub(1) | guard.stage_1_last_chunk; // expected bit mask of the target u64 in stage 1
        let is_stage_1_chunk_complete = guard.stage_1[stage_1_idx] == stage_1_chunk_expected; // is the target u64 in stage 1 full

        let stage_2_idx = stage_1_idx >> Self::FULL_CHUNK_DIV_BIT_SHIFT_CNT; // which u64 in stage 2
        let stage_2_rem = stage_1_idx & Self::LAST_CHUNK_MOD_MASK; // bit position in u64
        let stage_2_bit = u64::from(is_stage_1_chunk_complete) << stage_2_rem; // bit mask
        guard.stage_2[stage_2_idx] |= stage_2_bit; // set bit in stage 2
        if guard.last_pkt_psn.wrapping_add(1) != new_psn {
            guard.is_out_of_order = true;
        }
        guard.last_pkt_psn = new_psn;
    }

    /// check if is complete and is out of order
    pub(crate) fn check_status(&self) -> (bool, bool) {
        let guard = self.inner.lock();
        let is_out_of_order = guard.is_out_of_order;
        let is_complete = guard
            .stage_2
            .iter()
            .enumerate()
            .fold(true, |acc, (idx, &bits)| {
                let is_last_chunk = idx == guard.stage_2.len().wrapping_sub(1);
                let chunk_expected =
                    u64::from(is_last_chunk).wrapping_sub(1) | guard.stage_2_last_chunk;
                let is_chunk_complete = bits == chunk_expected;
                acc && is_chunk_complete
            });
        (is_complete, is_out_of_order)
    }

    pub(crate) fn dqpn(&self) -> Qpn {
        self.dqpn
    }

    pub(crate) fn end_psn(&self) -> Psn {
        self.end_psn
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::HashMap, sync::Arc, thread::sleep, time::Duration};

    use crate::{
        op_ctx::ReadOpCtx,
        pkt_checker::RecvPktMap,
        types::{Msn, Psn, Qpn},
    };

    use super::PacketChecker;

    use flume::{unbounded, TryRecvError};
    use parking_lot::RwLock;

    #[test]
    fn test_packet_checker() {
        let (send_queue, recv_queue) = unbounded();
        let recv_pkt_map = Arc::new(RwLock::new(HashMap::<Msn, Arc<RecvPktMap>>::new()));
        let read_op_ctx_map = Arc::new(RwLock::new(HashMap::<Msn, ReadOpCtx>::new()));
        let _packet_checker = PacketChecker::new(
            send_queue,
            Arc::<RwLock<HashMap<Msn, Arc<RecvPktMap>>>>::clone(&recv_pkt_map),
            Arc::<RwLock<HashMap<Msn, ReadOpCtx>>>::clone(&read_op_ctx_map),
        );
        let key = Msn::new(1);
        recv_pkt_map.write().insert(
            key,
            RecvPktMap::new(false, 2, Psn::new(1), Qpn::new(3)).into(),
        );
        recv_pkt_map
            .write()
            .get(&key)
            .unwrap()
            .insert(Psn::new(1));
        sleep(Duration::from_millis(1));
        assert!(matches!(recv_queue.try_recv(), Err(TryRecvError::Empty)));
        recv_pkt_map
            .write()
            .get(&key)
            .unwrap()
            .insert(Psn::new(2));
        sleep(Duration::from_millis(10));
        assert!(recv_queue.try_recv().is_ok());
    }
}
