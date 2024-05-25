use core::panic;
use flume::Sender;
use log::{debug, error, info};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};

use crate::{
    buf::Slot,
    checker::PacketCheckEvent,
    device::{
        ToHostRb, ToHostWorkRbDesc, ToHostWorkRbDescAck, ToHostWorkRbDescNack, ToHostWorkRbDescRaw,
        ToHostWorkRbDescRead, ToHostWorkRbDescStatus, ToHostWorkRbDescWriteOrReadResp,
        ToHostWorkRbDescWriteWithImm,
    },
    nic::NicRecvNotification,
    responser::{RespCommand, RespReadRespCommand},
    Error,
};

#[allow(clippy::module_name_repetitions)]
#[derive(Debug)]
pub(crate) struct WorkDescPoller {
    thread: Option<std::thread::JoinHandle<()>>,
    stop_flag: Arc<AtomicBool>,
}

pub(crate) struct WorkDescPollerContext {
    pub(crate) work_rb: Arc<dyn ToHostRb<ToHostWorkRbDesc>>,
    pub(crate) resp_channel: Sender<RespCommand>,
    pub(crate) checker_channel: Sender<PacketCheckEvent>,
    pub(crate) nic_channel: Sender<NicRecvNotification>,
}

unsafe impl Send for WorkDescPollerContext {}

impl WorkDescPoller {
    pub(crate) fn new(ctx: WorkDescPollerContext) -> Self {
        let stop_flag = Arc::new(AtomicBool::new(false));
        let thread_stop_flag = Arc::clone(&stop_flag);
        let thread = std::thread::spawn(move || {
            WorkDescPollerContext::poll_working_thread(&ctx, &thread_stop_flag);
        });
        Self {
            thread: Some(thread),
            stop_flag,
        }
    }
}

impl WorkDescPollerContext {
    pub(crate) fn poll_working_thread(ctx: &Self, stop_flag: &AtomicBool) {
        while !stop_flag.load(Ordering::Relaxed) {
            let desc = match ctx.work_rb.pop() {
                Ok(desc) => desc,
                Err(e) => {
                    error!("WorkDescPoller is stopped due to : {:?}", e);
                    return;
                }
            };
            debug!("driver read from card RQ: {:?}", &desc);
            if !matches!(desc.status(), ToHostWorkRbDescStatus::Normal) {
                error!("desc status is {:?}", desc.status());
                continue;
            }

            let result = match desc {
                ToHostWorkRbDesc::Read(desc) => ctx.handle_work_desc_read(desc),
                ToHostWorkRbDesc::WriteOrReadResp(desc) => ctx.handle_work_desc_write(desc),
                ToHostWorkRbDesc::WriteWithImm(desc) => ctx.handle_work_desc_write_with_imm(&desc),
                ToHostWorkRbDesc::Ack(desc) => ctx.handle_work_desc_ack(desc),
                ToHostWorkRbDesc::Nack(desc) => ctx.handle_work_desc_nack(desc),
                ToHostWorkRbDesc::Raw(desc) => ctx.handle_work_desc_raw(&desc),
            };
            if let Err(reason) = result {
                error!("poll_work_rb stopped: {}", reason);
                return;
            }
        }
    }

    #[inline]
    fn handle_work_desc_read(&self, desc: ToHostWorkRbDescRead) -> Result<(), Error> {
        let command = RespCommand::ReadResponse(RespReadRespCommand { desc });
        self.resp_channel
            .send(command)
            .map_err(|_| Error::PipeBroken("work polling thread to responser"))?;
        Ok(())
    }

    #[inline]
    fn handle_work_desc_write(&self, desc: ToHostWorkRbDescWriteOrReadResp) -> Result<(), Error> {
        let msg = PacketCheckEvent::from(desc);
        self.checker_channel
            .send(msg)
            .map_err(|_| Error::PipeBroken("work polling thread to responser"))
    }

    fn handle_work_desc_write_with_imm(
        &self,
        _desc: &ToHostWorkRbDescWriteWithImm,
    ) -> Result<(), Error> {
        todo!()
    }

    #[inline]
    fn handle_work_desc_ack(&self, desc: ToHostWorkRbDescAck) -> Result<(), Error> {
        let msg = PacketCheckEvent::from(desc);
        self.checker_channel
            .send(msg)
            .map_err(|_| Error::PipeBroken("work polling thread to responser"))
    }

    #[inline]
    fn handle_work_desc_nack(&self, desc: ToHostWorkRbDescNack) -> Result<(), Error> {
        let msg = PacketCheckEvent::from(desc);
        self.checker_channel
            .send(msg)
            .map_err(|_| Error::PipeBroken("work polling thread to responser"))
    }

    #[inline]
    fn handle_work_desc_raw(&self, desc: &ToHostWorkRbDescRaw) -> Result<(), Error> {
        let slot = unsafe { Slot::from_raw_parts_mut(desc.addr as *mut u8, desc.key) };
        self.nic_channel
            .send(NicRecvNotification {
                buf: slot,
                len: desc.len,
            })
            .map_err(|_| Error::PipeBroken("work polling thread to nic thread"))
    }
}

impl Drop for WorkDescPoller {
    fn drop(&mut self) {
        self.stop_flag.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            if let Err(e) = thread.join() {
                panic!("{}", format!("WorkDescPoller thread join failed: {e:?}"));
            }
            info!("WorkDescPoller thread is normally stopped");
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::HashMap, sync::Arc, thread::sleep};

    use crate::{
        checker::PacketCheckEvent,
        device::{
            DeviceError, ToHostRb, ToHostWorkRbDesc, ToHostWorkRbDescAck, ToHostWorkRbDescCommon,
            ToHostWorkRbDescRaw, ToHostWorkRbDescRead, ToHostWorkRbDescStatus,
            ToHostWorkRbDescTransType, ToHostWorkRbDescWriteOrReadResp, ToHostWorkRbDescWriteType,
        },
        op_ctx::WriteOpCtx,
        qp::QpContext,
        responser::RespCommand,
        types::{Key, Msn, Psn, Qpn},
    };

    use super::WorkDescPoller;

    use parking_lot::{Mutex, RwLock};

    struct MockToHostRb {
        rb: Mutex<Vec<ToHostWorkRbDesc>>,
    }
    impl MockToHostRb {
        fn new(v: Vec<ToHostWorkRbDesc>) -> Self {
            MockToHostRb { rb: Mutex::new(v) }
        }
    }
    impl ToHostRb<ToHostWorkRbDesc> for MockToHostRb {
        fn pop(&self) -> Result<ToHostWorkRbDesc, DeviceError> {
            let is_empty = self.rb.lock().is_empty();
            if is_empty {
                sleep(std::time::Duration::from_secs(1));
                return Ok(ToHostWorkRbDesc::Raw(ToHostWorkRbDescRaw::default()));
            }
            Ok(self.rb.lock().pop().unwrap())
        }
    }
    #[test]
    fn test_work_desc_poller() {
        let mut input = vec![
            // test writeFirst
            ToHostWorkRbDesc::WriteOrReadResp(ToHostWorkRbDescWriteOrReadResp {
                common: ToHostWorkRbDescCommon {
                    dqpn: Qpn::new(3),
                    ..Default::default()
                },
                len: 4096,
                write_type: ToHostWorkRbDescWriteType::First,
                psn: Psn::new(0),
                ..Default::default()
            }),
            // test writeMiddle
            ToHostWorkRbDesc::WriteOrReadResp(ToHostWorkRbDescWriteOrReadResp {
                common: ToHostWorkRbDescCommon {
                    dqpn: Qpn::new(3),
                    expected_psn: Psn::new(1),
                    ..Default::default()
                },
                is_read_resp: false,
                addr: 1024,
                len: 1024,
                write_type: ToHostWorkRbDescWriteType::First,
                psn: Psn::new(1),
            }),
            // test writeLast
            ToHostWorkRbDesc::WriteOrReadResp(ToHostWorkRbDescWriteOrReadResp {
                common: ToHostWorkRbDescCommon {
                    dqpn: Qpn::new(3),
                    expected_psn: Psn::new(2),
                    ..Default::default()
                },
                is_read_resp: false,
                addr: 1024,
                len: 1024,
                write_type: ToHostWorkRbDescWriteType::First,
                psn: Psn::new(2),
            }),
            // test read
            ToHostWorkRbDesc::Read(ToHostWorkRbDescRead {
                common: ToHostWorkRbDescCommon {
                    dqpn: Qpn::new(3),
                    ..Default::default()
                },
                len: 2048,
                laddr: 0,
                lkey: Key::new(0),
                raddr: 0,
                rkey: Key::new(0),
            }),
            ToHostWorkRbDesc::Ack(ToHostWorkRbDescAck {
                common: ToHostWorkRbDescCommon {
                    dqpn: Qpn::new(3),
                    ..Default::default()
                },
                value: 0,
                msn: Msn::default(),
                psn: Psn::new(2),
            }),
            ToHostWorkRbDesc::Raw(ToHostWorkRbDescRaw {
                common: ToHostWorkRbDescCommon {
                    dqpn: Qpn::new(3),
                    ..Default::default()
                },
                addr: 0xa00_0000_0000,
                len: 4096,
                key: Default::default(),
            }),
        ];

        input.reverse();

        let work_rb = Arc::new(MockToHostRb::new(input));
        let qp_table = Arc::new(RwLock::new(HashMap::new()));
        qp_table.write().insert(
            Qpn::new(3),
            QpContext {
                qpn: Qpn::new(3),
                ..Default::default()
            },
        );
        let (resp_channel, resp_recv_queue) = flume::unbounded();
        let (checker_channel, checker_recv_queue) = flume::unbounded();
        let (notification_send_queue, notification_recv_queue) = flume::unbounded();

        let work_ctx = super::WorkDescPollerContext {
            work_rb,
            resp_channel,
            checker_channel,
            nic_channel: notification_send_queue,
        };
        let _poller = WorkDescPoller::new(work_ctx);
        assert_eq!(checker_recv_queue.recv().unwrap().psn.get(), 0);
        assert_eq!(checker_recv_queue.recv().unwrap().psn.get(), 1);
        assert_eq!(checker_recv_queue.recv().unwrap().psn.get(), 2);
        let item = resp_recv_queue.recv().unwrap();
        match item {
            RespCommand::ReadResponse(res) => {
                assert_eq!(res.desc.len, 2048);
                assert_eq!(res.desc.laddr, 0);
                assert_eq!(res.desc.lkey, Key::new(0));
                assert_eq!(res.desc.raddr, 0);
                assert_eq!(res.desc.rkey, Key::new(0));
            }
            _ => panic!("unexpected item"),
        }
        let item = checker_recv_queue.recv().unwrap();
        assert_eq!(item.psn.get(), 2);
        let buf = &mut notification_recv_queue.recv().unwrap().buf;
        assert_eq!(buf.as_mut_slice().as_mut_ptr() as usize, 0xa00_0000_0000);
        assert_eq!(buf.as_mut_slice().len(), 4096);
    }
}
