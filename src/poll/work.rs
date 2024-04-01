use core::panic;
use log::{debug, error};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex, RwLock},
};

use crate::{
    device::{
        ToHostRb, ToHostWorkRbDesc, ToHostWorkRbDescAck, ToHostWorkRbDescNack,
        ToHostWorkRbDescRead, ToHostWorkRbDescStatus, ToHostWorkRbDescWriteOrReadResp,
        ToHostWorkRbDescWriteType, ToHostWorkRbDescWriteWithImm,
    },
    op_ctx::WriteOpCtx,
    qp::QpContext,
    responser::{RespCommand, RespReadRespCommand},
    types::{Msn, Qpn},
    Error, RecvPktMap,
};

#[allow(clippy::module_name_repetitions)]
#[derive(Debug)]
pub(crate) struct WorkDescPoller {
    _thread: std::thread::JoinHandle<()>,
}

pub(crate) struct WorkDescPollerContext {
    pub(crate) work_rb: Arc<dyn ToHostRb<ToHostWorkRbDesc>>,
    pub(crate) recv_pkt_map: Arc<RwLock<HashMap<Msn, Arc<Mutex<RecvPktMap>>>>>,
    pub(crate) qp_table: Arc<RwLock<HashMap<Qpn, QpContext>>>,
    pub(crate) sending_queue: std::sync::mpsc::Sender<RespCommand>,
    pub(crate) write_op_ctx_map: Arc<RwLock<HashMap<Msn, WriteOpCtx>>>,
}

unsafe impl Send for WorkDescPollerContext {}

impl WorkDescPoller {
    pub(crate) fn new(ctx: WorkDescPollerContext) -> Self {
        let thread = std::thread::spawn(move || WorkDescPollerContext::poll_working_thread(&ctx));
        Self { _thread: thread }
    }
}

impl WorkDescPollerContext {
    pub(crate) fn poll_working_thread(ctx: &Self) {
        loop {
            let desc = match ctx.work_rb.pop() {
                Ok(desc) => desc,
                Err(e) => {
                    error!("failed to fetch descriptor from work rb : {:?}", e);
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
                ToHostWorkRbDesc::WriteOrReadResp(desc) => ctx.handle_work_desc_write(&desc),
                ToHostWorkRbDesc::WriteWithImm(desc) => ctx.handle_work_desc_write_with_imm(&desc),
                ToHostWorkRbDesc::Ack(desc) => ctx.handle_work_desc_ack(&desc),
                ToHostWorkRbDesc::Nack(desc) => ctx.handle_work_desc_nack(&desc),
            };
            if let Err(reason) = result {
                error!("poll_work_rb stopped: {}", reason);
                return;
            }
        }
    }

    fn handle_work_desc_read(&self, desc: ToHostWorkRbDescRead) -> Result<(), Error> {
        let command = RespCommand::ReadResponse(RespReadRespCommand { desc });
        self.sending_queue
            .send(command)
            .map_err(|_| Error::PipeBroken("work polling thread to responser"))?;
        Ok(())
    }

    fn handle_work_desc_write(&self, desc: &ToHostWorkRbDescWriteOrReadResp) -> Result<(), Error> {
        let msn = desc.common.msn;

        if matches!(
            desc.write_type,
            ToHostWorkRbDescWriteType::First | ToHostWorkRbDescWriteType::Only
        ) {
            let real_payload_len = desc.len;
            let pmtu = {
                let guard = self
                    .qp_table
                    .read()
                    .map_err(|_| Error::LockPoisoned("qp table lock"))?;
                if let Some(qp_ctx) = guard.get(&desc.common.dqpn) {
                    qp_ctx.pmtu
                } else {
                    error!("{:?} not found", desc.common.dqpn.get());
                    return Ok(());
                }
            };

            let pmtu = u32::from(&pmtu);

            #[allow(clippy::cast_possible_truncation)]
            let first_pkt_len = if matches!(desc.write_type, ToHostWorkRbDescWriteType::First) {
                u64::from(pmtu) - (desc.addr & (u64::from(pmtu) - 1))
            } else {
                u64::from(real_payload_len)
            } as u32;

            let pkt_cnt = 1 + (real_payload_len - first_pkt_len).div_ceil(pmtu);
            let mut pkt_map = RecvPktMap::new(
                desc.is_read_resp,
                pkt_cnt as usize,
                desc.psn,
                desc.common.dqpn,
            );
            pkt_map.insert(desc.psn);
            let mut recv_pkt_map_guard = self
                .recv_pkt_map
                .write()
                .map_err(|_| Error::LockPoisoned("recv_pkt_map lock"))?;
            if recv_pkt_map_guard
                .insert(msn, Mutex::new(pkt_map).into())
                .is_some()
            {
                error!(
                    "msn={:?} already exists in recv_pkt_map_guard",
                    desc.common.msn
                );
            }
        } else {
            let guard = self
                .recv_pkt_map
                .read()
                .map_err(|_| Error::LockPoisoned("map of recv_pkt_map lock"))?;
            if let Some(recv_pkt_map) = guard.get(&msn) {
                let mut recv_pkt_map = recv_pkt_map
                    .lock()
                    .map_err(|_| Error::LockPoisoned("recv_pkt_map lock"))?;
                recv_pkt_map.insert(desc.psn);
            } else {
                error!("recv_pkt_map not found for {:?}", msn);
            }
        }
        Ok(())
    }

    fn handle_work_desc_write_with_imm(
        &self,
        _desc: &ToHostWorkRbDescWriteWithImm,
    ) -> Result<(), Error> {
        todo!()
    }

    fn handle_work_desc_ack(&self, desc: &ToHostWorkRbDescAck) -> Result<(), Error> {
        let guard = self
            .write_op_ctx_map
            .read()
            .map_err(|_| Error::LockPoisoned("write_op_ctx_map lock"))?;
        let key = desc.msn;
        if let Some(op_ctx) = guard.get(&key) {
            if let Err(e) = op_ctx.set_result(()) {
                error!("Set result failed {:?}", e);
            }
        } else {
            error!("receive ack, but op_ctx not found for {:?}", key);
        }

        Ok(())
    }

    // This function is still under development
    #[allow(clippy::unused_self)]
    fn handle_work_desc_nack(&self, _desc: &ToHostWorkRbDescNack) -> Result<(), Error> {
        panic!("receive a nack");
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::HashMap,
        net::Ipv4Addr,
        sync::{Arc, Mutex, RwLock},
        thread::sleep,
    };

    use eui48::MacAddress;

    use crate::{
        device::{
            DeviceError, ToHostRb, ToHostWorkRbDesc, ToHostWorkRbDescAck, ToHostWorkRbDescCommon,
            ToHostWorkRbDescRead, ToHostWorkRbDescStatus, ToHostWorkRbDescTransType,
            ToHostWorkRbDescWriteOrReadResp, ToHostWorkRbDescWriteType,
        },
        op_ctx::WriteOpCtx,
        qp::QpContext,
        responser::RespCommand,
        types::{Key, MemAccessTypeFlag, Msn, Psn, Qpn},
        Pd,
    };

    use super::WorkDescPoller;

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
            let is_empty = self.rb.lock().unwrap().is_empty();
            if is_empty {
                sleep(std::time::Duration::from_secs(10))
            }
            Ok(self.rb.lock().unwrap().pop().unwrap())
        }
    }
    #[test]
    fn test_work_desc_poller() {
        let mut input = vec![
            // test writeFirst
            ToHostWorkRbDesc::WriteOrReadResp(ToHostWorkRbDescWriteOrReadResp {
                common: ToHostWorkRbDescCommon {
                    dqpn: Qpn::new(3),
                    status: ToHostWorkRbDescStatus::Normal,
                    trans: ToHostWorkRbDescTransType::Rc,
                    pad_cnt: 0,
                    msn: Msn::default(),
                    expected_psn: Psn::new(0),
                },
                is_read_resp: false,
                addr: 0,
                len: 3192,
                key: Key::new(0),
                write_type: ToHostWorkRbDescWriteType::First,
                psn: Psn::new(0),
            }),
            // test writeMiddle
            ToHostWorkRbDesc::WriteOrReadResp(ToHostWorkRbDescWriteOrReadResp {
                common: ToHostWorkRbDescCommon {
                    dqpn: Qpn::new(3),
                    status: ToHostWorkRbDescStatus::Normal,
                    trans: ToHostWorkRbDescTransType::Rc,
                    pad_cnt: 0,
                    msn: Msn::default(),
                    expected_psn: Psn::new(1),
                },
                is_read_resp: false,
                addr: 1024,
                len: 1024,
                key: Key::new(0),
                write_type: ToHostWorkRbDescWriteType::First,
                psn: Psn::new(1),
            }),
            // test writeLast
            ToHostWorkRbDesc::WriteOrReadResp(ToHostWorkRbDescWriteOrReadResp {
                common: ToHostWorkRbDescCommon {
                    dqpn: Qpn::new(3),
                    status: ToHostWorkRbDescStatus::Normal,
                    trans: ToHostWorkRbDescTransType::Rc,
                    pad_cnt: 0,
                    msn: Msn::default(),
                    expected_psn: Psn::new(2),
                },
                is_read_resp: false,
                addr: 1024,
                len: 1024,
                key: Key::new(0),
                write_type: ToHostWorkRbDescWriteType::First,
                psn: Psn::new(2),
            }),
            // test read
            ToHostWorkRbDesc::Read(ToHostWorkRbDescRead {
                common: ToHostWorkRbDescCommon {
                    dqpn: Qpn::new(3),
                    status: ToHostWorkRbDescStatus::Normal,
                    trans: ToHostWorkRbDescTransType::Rc,
                    pad_cnt: 0,
                    msn: Msn::default(),
                    expected_psn: Psn::default(),
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
                    status: ToHostWorkRbDescStatus::Normal,
                    trans: ToHostWorkRbDescTransType::Rc,
                    pad_cnt: 0,
                    msn: Msn::default(),
                    expected_psn: Psn::default(),
                },
                value: 0,
                msn: Msn::default(),
                psn: Psn::new(2),
            }),
        ];
        input.reverse();

        let work_rb = Arc::new(MockToHostRb::new(input));
        let recv_pkt_map = Arc::new(RwLock::new(HashMap::new()));
        let qp_table = Arc::new(RwLock::new(HashMap::new()));
        qp_table.write().unwrap().insert(
            Qpn::new(3),
            QpContext {
                pd: Pd { handle: 0 },
                qpn: Qpn::new(3),
                qp_type: crate::types::QpType::Rc,
                rq_acc_flags: MemAccessTypeFlag::IbvAccessRemoteWrite,
                pmtu: crate::types::Pmtu::Mtu1024,
                local_ip: Ipv4Addr::LOCALHOST,
                local_mac_addr: MacAddress::new([0; 6]),
                dqp_ip: Ipv4Addr::LOCALHOST,
                dqp_mac_addr: MacAddress::new([0; 6]),
                sending_psn: Mutex::new(Psn::new(0)),
            },
        );
        let (sending_queue, recv_queue) = std::sync::mpsc::channel::<RespCommand>();
        let write_op_ctx_map = Arc::new(RwLock::new(HashMap::new()));
        let key = Msn::default();
        let ctx = WriteOpCtx::new_running();
        write_op_ctx_map.write().unwrap().insert(key, ctx.clone());
        let work_ctx = super::WorkDescPollerContext {
            work_rb,
            recv_pkt_map,
            qp_table,
            sending_queue,
            write_op_ctx_map,
        };
        let _poller = WorkDescPoller::new(work_ctx);
        let _ = ctx.wait();
        let item = recv_queue.recv().unwrap();
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
    }
}
