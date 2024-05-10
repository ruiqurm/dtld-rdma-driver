use std::{
    collections::{HashMap, LinkedList},
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc::Sender,
        Arc,
    },
};

use crate::{
    op_ctx::ReadOpCtx,
    recv_pkt_map::RecvPktMap,
    responser::{RespAckCommand, RespCommand},
    types::{Msn, Psn},
    Error,
};

use parking_lot::{Mutex, RwLock};

use log::{error, info};

#[derive(Debug)]
pub(crate) struct PacketChecker {
    thread: Option<std::thread::JoinHandle<()>>,
    stop_flag: Arc<AtomicBool>,
}

impl PacketChecker {
    pub(crate) fn new(
        send_queue: Sender<RespCommand>,
        recv_pkt_map: Arc<RwLock<HashMap<Msn, Arc<Mutex<RecvPktMap>>>>>,
        read_op_ctx_map: Arc<RwLock<HashMap<Msn, ReadOpCtx>>>,
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
                error!("Failed to join the WorkDescPoller thread: {:?}", e);
            }
        }
    }
}

struct PacketCheckerContext {
    send_queue: Sender<RespCommand>,
    recv_pkt_map: Arc<RwLock<HashMap<Msn, Arc<Mutex<RecvPktMap>>>>>,
    read_op_ctx_map: Arc<RwLock<HashMap<Msn, ReadOpCtx>>>,
}

impl PacketCheckerContext {
    fn working_thread(ctx: &Self, stop_flag: &AtomicBool) {
        while !stop_flag.load(Ordering::Relaxed) {
            if let Err(e) = ctx.check_pkt_map() {
                error!("PacketChecker stopped: {:?}", e);
                break;
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
            let (is_complete, is_read_resp, is_out_of_order, dqpn, end_psn) = {
                let guard = map.lock();
                (
                    guard.is_complete(),
                    guard.is_read_resp(),
                    guard.is_out_of_order(),
                    guard.dqpn(),
                    guard.end_psn(),
                )
            };
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
                panic!("send nack command")
            } else {
                // everthing is fine, do nothing
            }
        }

        // remove the completed recv_pkt_map
        if !remove_list.is_empty() {
            let mut guard = self.recv_pkt_map.write();
            for dqpn in &remove_list {
                let _: Option<Arc<Mutex<RecvPktMap>>> = guard.remove(dqpn);
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::HashMap,
        sync::{mpsc, Arc},
        thread::sleep,
        time::Duration,
    };

    use crate::{
        op_ctx::ReadOpCtx,
        recv_pkt_map::RecvPktMap,
        types::{Msn, Psn, Qpn},
    };

    use super::PacketChecker;

    use parking_lot::{Mutex, RwLock};

    #[test]
    fn test_packet_checker() {
        let (send_queue, recv_queue) = mpsc::channel();
        let recv_pkt_map = Arc::new(RwLock::new(HashMap::<Msn, Arc<Mutex<RecvPktMap>>>::new()));
        let read_op_ctx_map = Arc::new(RwLock::new(HashMap::<Msn, ReadOpCtx>::new()));
        let _packet_checker = PacketChecker::new(
            send_queue,
            Arc::<RwLock<HashMap<Msn, Arc<Mutex<RecvPktMap>>>>>::clone(&recv_pkt_map),
            Arc::<RwLock<HashMap<Msn, ReadOpCtx>>>::clone(&read_op_ctx_map),
        );
        let key = Msn::new(1);
        recv_pkt_map.write().insert(
            key,
            Mutex::new(RecvPktMap::new(false, 2, Psn::new(1), Qpn::new(3))).into(),
        );
        recv_pkt_map
            .write()
            .get(&key)
            .unwrap()
            .lock()
            .insert(Psn::new(1));
        sleep(Duration::from_millis(1));
        assert!(matches!(
            recv_queue.try_recv(),
            Err(mpsc::TryRecvError::Empty)
        ));
        recv_pkt_map
            .write()
            .get(&key)
            .unwrap()
            .lock()
            .insert(Psn::new(2));
        sleep(Duration::from_millis(10));
        assert!(recv_queue.try_recv().is_ok());
    }
}
