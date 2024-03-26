use crate::{
    device::{
        DeviceAdaptor, EmulatedDevice, HardwareDevice, SoftwareDevice, ToCardCtrlRbDesc,
        ToCardWorkRbDescCommon,
    },
    mr::{MrCtx, MrPgt},
    pd::PdCtx,
};
use device::{
    ToCardCtrlRbDescCommon, ToCardCtrlRbDescSetNetworkParam, ToCardCtrlRbDescSge,
    ToCardWorkRbDescBuilder,
};
use op_ctx::{CtrlOpCtx, ReadOpCtx, WriteOpCtx};
use pkt_checker::PacketChecker;
use poll::work::WorkDescPoller;
use qp::QpContext;
use recv_pkt_map::RecvPktMap;
use responser::{DescResponser, WorkDescriptorSender};
use std::{
    collections::HashMap,
    net::SocketAddr,
    sync::{
        atomic::{AtomicU16, AtomicU32, Ordering},
        Arc, Mutex, OnceLock, RwLock,
    },
    thread,
};
use thiserror::Error;
use types::{Key, MemAccessTypeFlag, Msn, Psn, Qpn, RdmaDeviceNetwork};
use utils::calculate_packet_cnt;

pub mod mr;
pub mod op_ctx;
pub mod pd;
pub mod qp;
pub mod types;

mod device;
mod pkt_checker;
mod poll;
mod recv_pkt_map;
mod responser;
mod utils;

pub use crate::{mr::Mr, pd::Pd};
pub use types::Error;

const MR_KEY_IDX_BIT_CNT: usize = 8;
const MR_TABLE_SIZE: usize = 64;
const MR_PGT_SIZE: usize = 1024;


#[derive(Clone)]
pub struct Device(Arc<DeviceInner<dyn DeviceAdaptor>>);
struct DeviceInner<D: ?Sized> {
    pd: Mutex<HashMap<Pd, PdCtx>>,
    mr_table: Mutex<[Option<MrCtx>; MR_TABLE_SIZE]>,
    qp_table: Arc<RwLock<HashMap<Qpn, QpContext>>>,
    mr_pgt: Mutex<MrPgt>,
    read_op_ctx_map: Arc<RwLock<HashMap<Msn, ReadOpCtx>>>,
    write_op_ctx_map: Arc<RwLock<HashMap<Msn, WriteOpCtx>>>,
    ctrl_op_ctx_map: RwLock<HashMap<u32, CtrlOpCtx>>,
    next_ctrl_op_id: AtomicU32,
    next_msn : AtomicU16,
    responser: OnceLock<DescResponser>,
    work_desc_poller: OnceLock<WorkDescPoller>,
    pkt_checker_thread: OnceLock<PacketChecker>,
    adaptor: D,
}

pub struct Sge {
    pub addr: u64,
    pub len: u32,
    pub key: Key,
}

impl Device {
    const MR_TABLE_EMPTY_ELEM: Option<MrCtx> = None;

    pub fn new_hardware(network: RdmaDeviceNetwork) -> Result<Self, Error> {
        let qp_table = Arc::new(RwLock::new(HashMap::new()));

        let adaptor = HardwareDevice::init().map_err(Error::Device)?;

        let inner = Arc::new(DeviceInner {
            pd: Mutex::new(HashMap::new()),
            mr_table: Mutex::new([Self::MR_TABLE_EMPTY_ELEM; MR_TABLE_SIZE]),
            qp_table,
            mr_pgt: Mutex::new(MrPgt::new()),
            read_op_ctx_map: Arc::new(RwLock::new(HashMap::new())),
            write_op_ctx_map: Arc::new(RwLock::new(HashMap::new())),
            ctrl_op_ctx_map: RwLock::new(HashMap::new()),
            next_ctrl_op_id: AtomicU32::new(0),
            next_msn: AtomicU16::new(0),
            adaptor,
            responser: OnceLock::new(),
            pkt_checker_thread: OnceLock::new(),
            work_desc_poller: OnceLock::new(),
        });

        let dev = Self(inner);
        dev.init(&network)?;

        Ok(dev)
    }

    pub fn new_software(network: RdmaDeviceNetwork) -> Result<Self, Error> {
        let qp_table = Arc::new(RwLock::new(HashMap::new()));
        let adaptor = SoftwareDevice::init().map_err(Error::Device)?;

        let inner = Arc::new(DeviceInner {
            pd: Mutex::new(HashMap::new()),
            mr_table: Mutex::new([Self::MR_TABLE_EMPTY_ELEM; MR_TABLE_SIZE]),
            qp_table,
            mr_pgt: Mutex::new(MrPgt::new()),
            read_op_ctx_map: Arc::new(RwLock::new(HashMap::new())),
            write_op_ctx_map: Arc::new(RwLock::new(HashMap::new())),
            ctrl_op_ctx_map: RwLock::new(HashMap::new()),
            next_ctrl_op_id: AtomicU32::new(0),
            next_msn: AtomicU16::new(0),
            responser: OnceLock::new(),
            work_desc_poller: OnceLock::new(),
            pkt_checker_thread: OnceLock::new(),
            adaptor,
        });

        let dev = Self(inner);
        dev.init(&network)?;

        Ok(dev)
    }

    pub fn new_emulated(
        rpc_server_addr: SocketAddr,
        heap_mem_start_addr: usize,
        network: &RdmaDeviceNetwork,
    ) -> Result<Self, Error> {
        let qp_table = Arc::new(RwLock::new(HashMap::new()));
        let adaptor =
            EmulatedDevice::init(rpc_server_addr, heap_mem_start_addr).map_err(Error::Device)?;

        let inner = Arc::new(DeviceInner {
            pd: Mutex::new(HashMap::new()),
            mr_table: Mutex::new([Self::MR_TABLE_EMPTY_ELEM; MR_TABLE_SIZE]),
            qp_table,
            mr_pgt: Mutex::new(MrPgt::new()),
            read_op_ctx_map: Arc::new(RwLock::new(HashMap::new())),
            write_op_ctx_map: Arc::new(RwLock::new(HashMap::new())),
            ctrl_op_ctx_map: RwLock::new(HashMap::new()),
            next_ctrl_op_id: AtomicU32::new(0),
            next_msn: AtomicU16::new(0),
            responser: OnceLock::new(),
            work_desc_poller: OnceLock::new(),
            pkt_checker_thread: OnceLock::new(),
            adaptor,
        });

        let dev = Self(inner);

        dev.init(network)?;

        Ok(dev)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn write(
        &self,
        dqpn: &Qpn,
        raddr: u64,
        rkey: Key,
        flags: MemAccessTypeFlag,
        sge0: Sge,
        sge1: Option<Sge>,
        sge2: Option<Sge>,
        sge3: Option<Sge>,
    ) -> Result<WriteOpCtx, Error> {
        let msn = self.get_msn();
        let common = {
            let total_len = sge0.len
                + sge1.as_ref().map_or(0, |sge| sge.len)
                + sge2.as_ref().map_or(0, |sge| sge.len)
                + sge3.as_ref().map_or(0, |sge| sge.len);
            let qp_guard = self.0.qp_table.read().unwrap();
            let qp = qp_guard.get(dqpn).ok_or(Error::InvalidQpn)?;
            let mut common = ToCardWorkRbDescCommon {
                total_len,
                raddr,
                rkey,
                dqp_ip: qp.dqp_ip,
                dqpn: qp.qpn,
                mac_addr: qp.dqp_mac_addr,
                pmtu: qp.pmtu.clone(),
                flags,
                qp_type: qp.qp_type,
                psn: Psn::default(),
                msn,
            };
            let packet_cnt = calculate_packet_cnt(qp.pmtu.clone(), raddr, total_len);
            let first_pkt_psn = {
                let mut send_psn = qp.sending_psn.lock().unwrap();
                let first_pkt_psn = *send_psn;
                *send_psn = send_psn.wrapping_add(packet_cnt);
                first_pkt_psn
            };
            common.psn = first_pkt_psn;
            common
        };

        let builder = ToCardWorkRbDescBuilder::new_write()
            .with_common(common)
            .with_sge(sge0)
            .with_option_sge(sge1)
            .with_option_sge(sge2)
            .with_option_sge(sge3);

        self.send_work_desc(builder)?;

        let ctx = WriteOpCtx::new_running();

        self.0
            .write_op_ctx_map
            .write()
            .unwrap()
            .insert(msn, ctx.clone());
        Ok(ctx)
    }

    pub fn read(
        &self,
        dqpn: Qpn,
        raddr: u64,
        rkey: Key,
        flags: MemAccessTypeFlag,
        sge: Sge,
    ) -> Result<ReadOpCtx, Error> {
        let msn = self.get_msn();
        let common = {
            let total_len = sge.len;
            let qp_guard = self.0.qp_table.read().unwrap();
            let qp = qp_guard.get(&dqpn).ok_or(Error::InvalidQpn)?;
            let mut common = ToCardWorkRbDescCommon {
                total_len,
                raddr,
                rkey,
                dqp_ip: qp.dqp_ip,
                dqpn: qp.qpn,
                mac_addr: qp.dqp_mac_addr,
                pmtu: qp.pmtu.clone(),
                flags,
                qp_type: qp.qp_type,
                psn: Psn::default(),
                msn,
            };
            let first_pkt_psn = {
                let mut send_psn = qp.sending_psn.lock().unwrap();
                let first_pkt_psn = *send_psn;
                *send_psn = send_psn.wrapping_add(1);
                first_pkt_psn
            };
            common.psn = first_pkt_psn;
            common
        };

        let builder = ToCardWorkRbDescBuilder::new_read()
            .with_common(common)
            .with_sge(sge);
        self.send_work_desc(builder)?;

        let ctx = WriteOpCtx::new_running();
        self.0
            .read_op_ctx_map
            .write()
            .unwrap()
            .insert(msn, ctx.clone());

        Ok(ctx)
    }

    fn do_ctrl_op(&self, id: u32, desc: ToCardCtrlRbDesc) -> Result<CtrlOpCtx, Error> {
        // save operation context for unparking
        let ctrl_ctx = {
            let mut ctx = self.0.ctrl_op_ctx_map.write().unwrap();
            let ctrl_ctx = CtrlOpCtx::new_running();

            let old = ctx.insert(id, ctrl_ctx.clone());

            assert!(old.is_none());
            ctrl_ctx
        };

        // send desc to device
        self.0
            .adaptor
            .to_card_ctrl_rb()
            .push(desc)
            .map_err(|_| Error::DeviceBusy)?;

        Ok(ctrl_ctx)
    }

    fn get_ctrl_op_id(&self) -> u32 {
        self.0.next_ctrl_op_id.fetch_add(1, Ordering::AcqRel)
    }

    fn get_msn(&self) -> Msn {
        Msn::new(self.0.next_msn.fetch_add(1, Ordering::AcqRel))
    }

    fn init(&self, network: &RdmaDeviceNetwork) -> Result<(), Error> {
        let (send_queue, rece_queue) = std::sync::mpsc::channel();
        let dev_for_poll_ctrl_rb = self.clone();
        let recv_pkt_map = Arc::new(Mutex::new(HashMap::new()));

        // enable ctrl desc poller module
        thread::spawn(move || dev_for_poll_ctrl_rb.poll_ctrl_rb());

        // enable responser module
        let ack_buf = self.init_ack_buf()?;
        let responser = DescResponser::new(
            Arc::new(self.clone()),
            rece_queue,
            ack_buf,
            self.0.qp_table.clone(),
        );
        if self.0.responser.set(responser).is_err() {
            panic!("responser has been set");
        }
        // enable work desc poller module.
        let work_desc_poller = WorkDescPoller::new(
            self.0.adaptor.to_host_work_rb(),
            recv_pkt_map.clone(),
            self.0.qp_table.clone(),
            send_queue.clone(),
            self.0.write_op_ctx_map.clone(),
        );
        if self.0.work_desc_poller.set(work_desc_poller).is_err() {
            panic!("work_desc_poller has been set");
        }

        // enable packet checker module
        let pkt_checker_thread =
            PacketChecker::new(send_queue, recv_pkt_map, self.0.read_op_ctx_map.clone());
        if self.0.pkt_checker_thread.set(pkt_checker_thread).is_err() {
            panic!("pkt_checker_thread has been set");
        }

        // set card network
        self.set_network(network)?;

        Ok(())
    }

    fn set_network(&self, network: &RdmaDeviceNetwork) -> Result<(), Error> {
        let op_id = self.get_ctrl_op_id();
        let desc = ToCardCtrlRbDesc::SetNetworkParam(ToCardCtrlRbDescSetNetworkParam {
            common: ToCardCtrlRbDescCommon { op_id },
            gateway: network.gateway,
            netmask: network.netmask,
            ipaddr: network.ipaddr,
            macaddr: network.macaddr,
        });
        let ctx = self.do_ctrl_op(op_id, desc)?;
        let is_success = ctx.wait_result().expect("set network param failed");
        if !is_success {
            return Err(Error::SetNetworkParamFailed);
        };
        Ok(())
    }
}

impl From<Sge> for ToCardCtrlRbDescSge {
    fn from(sge: Sge) -> Self {
        Self {
            addr: sge.addr,
            len: sge.len,
            key: sge.key,
        }
    }
}

impl WorkDescriptorSender for Device {
    fn send_work_desc(&self, desc_builder: ToCardWorkRbDescBuilder) -> Result<(), Error> {
        let desc = desc_builder.build()?;
        self.0
            .adaptor
            .to_card_work_rb()
            .push(desc)
            .map_err(|_| Error::DeviceBusy)?;
        Ok(())
    }
}

#[derive(Debug, Error)]
pub enum PoisonErrorWrapper {
    #[error("Remote qp map lock poisoned")]
    RemoteQpMap,
    #[error("local qp map lock poisoned")]
    LocalQpMap,
    #[error("read op map lock poisoned")]
    ReadOpCtxMap,
    #[error("write op map lock poisoned")]
    WriteOpCtxMap,
    #[error("ctrl map lock poisoned")]
    CtrlOpCtxMap,
}
