//! open-rdma-driver
#![deny(
    // The following are allowed by default lints according to
    // https://doc.rust-lang.org/rustc/lints/listing/allowed-by-default.html
    absolute_paths_not_starting_with_crate,
    explicit_outlives_requirements,
    // elided_lifetimes_in_paths,  // allow anonymous lifetime
    explicit_outlives_requirements,
    keyword_idents,
    macro_use_extern_crate,
    meta_variable_misuse,
    missing_abi,
    missing_copy_implementations,
    missing_debug_implementations, 
    // must_not_suspend, unstable
    missing_docs,
    non_ascii_idents,
    // non_exhaustive_omitted_patterns, unstable
    noop_method_call,
    pointer_structural_match,
    rust_2021_incompatible_closure_captures,
    rust_2021_incompatible_or_patterns,
    rust_2021_prefixes_incompatible_syntax,
    rust_2021_prelude_collisions,
    single_use_lifetimes,
    // trivial_casts, // We allow trivial_casts for casting a pointer
    trivial_numeric_casts,
    unreachable_pub,
    // unsafe_code, // we need unsafe when managing memory
    unsafe_op_in_unsafe_fn,
    unstable_features,
    unused_extern_crates,
    unused_import_braces,
    unused_lifetimes,
    unused_qualifications,
    unused_results,
    variant_size_differences, 

    clippy::all,
    clippy::pedantic,
    clippy::cargo,

    // The followings are selected restriction lints for rust 1.57
    // clippy::as_conversions, // we allow lossless "as" conversion, it has checked by clippy::cast_possible_truncation
    clippy::clone_on_ref_ptr,
    clippy::create_dir,
    clippy::dbg_macro,
    clippy::decimal_literal_representation,
    clippy::default_numeric_fallback,
    clippy::disallowed_script_idents,
    clippy::else_if_without_else,
    clippy::exhaustive_enums,
    clippy::exhaustive_structs,
    clippy::exit,
    clippy::expect_used,
    clippy::filetype_is_file,
    clippy::float_arithmetic,
    clippy::float_cmp_const,
    clippy::get_unwrap,
    clippy::if_then_some_else_none,
    // clippy::implicit_return, it's idiomatic Rust code.
    clippy::indexing_slicing,
    clippy::inline_asm_x86_intel_syntax,
    clippy::arithmetic_side_effects,
 
    // clippy::pattern_type_mismatch, // cause some false postive and unneeded copy
    clippy::print_stderr,
    clippy::print_stdout,
    clippy::rc_buffer,
    clippy::rc_mutex,
    clippy::rest_pat_in_fully_bound_structs,
    clippy::same_name_method,
    clippy::self_named_module_files,
    // clippy::shadow_reuse, it’s a common pattern in Rust code
    // clippy::shadow_same, it’s a common pattern in Rust code
    clippy::shadow_unrelated,
    clippy::str_to_string,
    clippy::string_add,
    clippy::string_to_string,
    // clippy::todo, // TODO: We still have some unclear todos
    // clippy::unimplemented, // TODO: We still have some unimplemented functions
    clippy::unnecessary_self_imports,
    clippy::unneeded_field_pattern,
    // clippy::unreachable, // the unreachable code should unreachable otherwise it's a bug
    clippy::unwrap_in_result,
    clippy::unwrap_used, 
    clippy::use_debug,
    clippy::verbose_file_reads,
    clippy::wildcard_enum_match_arm,

    // // The followings are selected lints from 1.61.0 to 1.67.1
    clippy::as_ptr_cast_mut,
    clippy::derive_partial_eq_without_eq,
    clippy::empty_drop,
    clippy::empty_structs_with_brackets,
    clippy::format_push_string,
    clippy::iter_on_empty_collections,
    clippy::iter_on_single_items,
    clippy::large_include_file,
    clippy::manual_clamp,
    clippy::suspicious_xor_used_as_pow,
    clippy::unnecessary_safety_comment,
    clippy::unnecessary_safety_doc,
    clippy::unused_peekable,
    clippy::unused_rounding,    

    // The followings are selected restriction lints from rust 1.68.0 to 1.71.0
    // clippy::allow_attributes, still unstable
    clippy::impl_trait_in_params,
    clippy::let_underscore_untyped,
    clippy::missing_assert_message,
    clippy::multiple_unsafe_ops_per_block,
    clippy::semicolon_inside_block,
    // clippy::semicolon_outside_block, already used `semicolon_inside_block`
    clippy::tests_outside_test_module,
    // 1.71.0
    clippy::default_constructed_unit_structs,
    clippy::items_after_test_module,
    clippy::manual_next_back,
    clippy::manual_while_let_some,
    clippy::needless_bool_assign,
    clippy::non_minimal_cfg,
)]

#![cfg_attr(
    test,
    allow(
        clippy::indexing_slicing,
        unused_results,
        clippy::unwrap_used,
        clippy::unwrap_in_result,
        clippy::expect_used,
        clippy::as_conversions,
        clippy::shadow_unrelated,
        clippy::arithmetic_side_effects,
        clippy::let_underscore_untyped,
        clippy::pedantic, 
        clippy::default_numeric_fallback,
    )
)]
use crate::{
    device::{
        DeviceAdaptor, EmulatedDevice, HardwareDevice, SoftwareDevice, ToCardCtrlRbDesc,
        ToCardWorkRbDescCommon,
    },
    mr::{MrCtx, MrPgt},
    pd::PdCtx,
};
use device::{
    scheduler::{round_robin::RoundRobinStrategy, DescriptorScheduler}, ToCardCtrlRbDescCommon, ToCardCtrlRbDescSetNetworkParam, ToCardCtrlRbDescSetRawPacketReceiveMeta, ToCardCtrlRbDescSge, ToCardWorkRbDesc, ToCardWorkRbDescBuilder
};
use flume::unbounded;
use mr::{ACKNOWLEDGE_BUFFER_SIZE, NIC_BUFFER_SIZE};
use nic::BasicNicDeivce;
use op_ctx::{CtrlOpCtx, ReadOpCtx, WriteOpCtx};
use pkt_checker::{PacketChecker, RecvPktMap};
use poll::{ctrl::{ControlPoller, ControlPollerContext}, work::{WorkDescPoller, WorkDescPollerContext}};
use qp::QpContext;
use responser::DescResponser;

use std::{
    collections::HashMap, net::SocketAddr, sync::{
        atomic::{AtomicU16, AtomicU32, Ordering},
        Arc, OnceLock,
    }
};
use thiserror::Error;
use types::{Key, MemAccessTypeFlag, Msn, Psn, Qpn, RdmaDeviceNetworkParam, Sge};
use utils::{calculate_packet_cnt, Buffer};
use parking_lot::{Mutex,RwLock};

/// memory region
pub mod mr;
/// op context for user to track the status of the write/read/control operation
pub mod op_ctx;
/// protection domain
pub mod pd;
/// queue pair related structs and functions
pub mod qp;
/// types exported to user
pub mod types;

/// adaptor device: hardware, software, emulated
mod device;
/// pakcet check thread: checking if the packet is received correctly
mod pkt_checker;
/// poll thread: polling the work descriptor and control descriptor
mod poll;
/// responser thread: sending the response(read resp or ack) to the device
mod responser;
/// A simple buffer allocator
mod buf;
/// basic nic functions
mod nic;
/// utility functions
mod utils;

pub use crate::{mr::Mr, pd::Pd};
pub use types::Error;
pub use utils::{HugePage,AlignedMemory};

const MR_KEY_IDX_BIT_CNT: usize = 8;
const MR_TABLE_SIZE: usize = 64;
const MR_PGT_SIZE: usize = 1024;
const DEFAULT_RMDA_PORT : u16 = 4791;

type ThreadSafeHashmap<K,V> = Arc<RwLock<HashMap<K,V>>>;

/// A user space RDMA device.
/// 
/// The device provides a general interface, like `write`, `read`, `register_mr/qp/pd`, etc.
/// 
/// The device has an adaptor, which can be hardware, software, or emulated. 
#[derive(Debug,Clone)]
pub struct Device(Arc<DeviceInner<dyn DeviceAdaptor>>);

#[derive(Debug)]
struct DeviceInner<D: ?Sized> {
    pd: Mutex<HashMap<Pd, PdCtx>>,
    mr_table: Mutex<[Option<MrCtx>; MR_TABLE_SIZE]>,
    qp_table: ThreadSafeHashmap<Qpn, QpContext>,
    mr_pgt: Mutex<MrPgt>,
    read_op_ctx_map: ThreadSafeHashmap<Msn, ReadOpCtx>,
    write_op_ctx_map: ThreadSafeHashmap<Msn, WriteOpCtx>,
    ctrl_op_ctx_map: ThreadSafeHashmap<u32, CtrlOpCtx>,
    next_ctrl_op_id: AtomicU32,
    next_msn: AtomicU16,
    responser: OnceLock<DescResponser>,
    work_desc_poller: OnceLock<WorkDescPoller>,
    pkt_checker_thread: OnceLock<PacketChecker>,
    ctrl_desc_poller : OnceLock<ControlPoller>,
    local_network : RdmaDeviceNetworkParam,
    nic_device : OnceLock<BasicNicDeivce>,
    buffer_keeper : Mutex<Vec<Buffer>>,
    adaptor: D,
}

impl Device {
    const MR_TABLE_EMPTY_ELEM: Option<MrCtx> = None;
    /// # Errors
    ///
    /// Will return `Err` if the device failed to create the `adaptor` or the device failed to init.
    pub fn new_hardware(network: &RdmaDeviceNetworkParam,device_name : String) -> Result<Self, Error> {
        let qp_table = Arc::new(RwLock::new(HashMap::new()));
        let round_robin = Arc::new(RoundRobinStrategy::new());
        let scheduler = Arc::new(DescriptorScheduler::new(round_robin));
        let adaptor = HardwareDevice::init(device_name,scheduler).map_err(|e| Error::Device(Box::new(e)))?;

        let inner = Arc::new(DeviceInner {
            pd: Mutex::new(HashMap::new()),
            mr_table: Mutex::new([Self::MR_TABLE_EMPTY_ELEM; MR_TABLE_SIZE]),
            qp_table,
            mr_pgt: Mutex::new(MrPgt::new()),
            read_op_ctx_map: Arc::new(RwLock::new(HashMap::new())),
            write_op_ctx_map: Arc::new(RwLock::new(HashMap::new())),
            ctrl_op_ctx_map: Arc::new(RwLock::new(HashMap::new())),
            next_ctrl_op_id: AtomicU32::new(0),
            next_msn: AtomicU16::new(0),
            adaptor,
            responser: OnceLock::new(),
            pkt_checker_thread: OnceLock::new(),
            work_desc_poller: OnceLock::new(),
            ctrl_desc_poller : OnceLock::new(),
            nic_device : OnceLock::new(),
            buffer_keeper : Vec::new().into(),
            local_network : *network,
        });

        let dev = Self(inner);
        dev.init(true)?;

        Ok(dev)
    }

    /// # Errors
    ///
    /// Will return `Err` if the device failed to create the `adaptor` or the device failed to init.
    pub fn new_software(network: &RdmaDeviceNetworkParam) -> Result<Self, Error> {
        let qp_table = Arc::new(RwLock::new(HashMap::new()));
        let adaptor = SoftwareDevice::init(network.ipaddr,DEFAULT_RMDA_PORT).map_err(Error::Device)?;

        let inner = Arc::new(DeviceInner {
            pd: Mutex::new(HashMap::new()),
            mr_table: Mutex::new([Self::MR_TABLE_EMPTY_ELEM; MR_TABLE_SIZE]),
            qp_table,
            mr_pgt: Mutex::new(MrPgt::new()),
            read_op_ctx_map: Arc::new(RwLock::new(HashMap::new())),
            write_op_ctx_map: Arc::new(RwLock::new(HashMap::new())),
            ctrl_op_ctx_map: Arc::new(RwLock::new(HashMap::new())),
            next_ctrl_op_id: AtomicU32::new(0),
            next_msn: AtomicU16::new(0),
            responser: OnceLock::new(),
            work_desc_poller: OnceLock::new(),
            pkt_checker_thread: OnceLock::new(),
            ctrl_desc_poller : OnceLock::new(),
            nic_device : OnceLock::new(),
            buffer_keeper : Vec::new().into(),
            local_network : *network,
            adaptor,
        });

        let dev = Self(inner);
        dev.init(false)?;

        Ok(dev)
    }

    /// # Errors
    ///
    /// Will return `Err` if the device failed to create the `adaptor` or the device failed to init.
    pub fn new_emulated(
        rpc_server_addr: SocketAddr,
        heap_mem_start_addr: usize,
        network: &RdmaDeviceNetworkParam,
    ) -> Result<Self, Error> {
        let qp_table = Arc::new(RwLock::new(HashMap::new()));
        #[cfg(feature = "scheduler")]
        let adaptor = {
            let round_robin = Arc::new(RoundRobinStrategy::new());
            let scheduler = Arc::new(DescriptorScheduler::new(round_robin));
            EmulatedDevice::init(rpc_server_addr, heap_mem_start_addr,scheduler).map_err(|e| Error::Device(Box::new(e)))?
        };
        #[cfg(not(feature = "scheduler"))]
        let adaptor =
            EmulatedDevice::init(rpc_server_addr, heap_mem_start_addr).map_err(|e| Error::Device(Box::new(e)))?;

        let inner = Arc::new(DeviceInner {
            pd: Mutex::new(HashMap::new()),
            mr_table: Mutex::new([Self::MR_TABLE_EMPTY_ELEM; MR_TABLE_SIZE]),
            qp_table,
            mr_pgt: Mutex::new(MrPgt::new()),
            read_op_ctx_map: Arc::new(RwLock::new(HashMap::new())),
            write_op_ctx_map: Arc::new(RwLock::new(HashMap::new())),
            ctrl_op_ctx_map: Arc::new(RwLock::new(HashMap::new())),
            next_ctrl_op_id: AtomicU32::new(0),
            next_msn: AtomicU16::new(0),
            responser: OnceLock::new(),
            work_desc_poller: OnceLock::new(),
            pkt_checker_thread: OnceLock::new(),
            ctrl_desc_poller : OnceLock::new(),
            local_network : *network,
            nic_device : OnceLock::new(),
            buffer_keeper : Vec::new().into(),
            adaptor,
        });

        let dev = Self(inner);

        dev.init(false)?;

        Ok(dev)
    }

    /// RDMA write operation
    /// 
    /// # Errors
    ///
    /// Will return `Err` if:
    /// * lock poisoned
    /// * failed to create a descriptor
    /// * failed to send a descriptor
    /// * failed to create a operation context
    pub fn write(
        &self,
        dqpn: Qpn,
        raddr: u64,
        rkey: Key,
        flags: MemAccessTypeFlag,
        sge0: Sge
    ) -> Result<WriteOpCtx, Error> {
        let msn = self.get_msn();
        let common = {
            let total_len = sge0.len;
            let qp_guard = self.0.qp_table.read();
            let qp = qp_guard.get(&dqpn).ok_or(Error::Invalid(format!("Qpn :{dqpn:?}")))?;
            let mut common = ToCardWorkRbDescCommon {
                total_len,
                raddr,
                rkey,
                dqp_ip: qp.dqp_ip,
                dqpn: qp.qpn,
                mac_addr: qp.dqp_mac_addr,
                pmtu: qp.pmtu,
                flags,
                qp_type: qp.qp_type,
                psn: Psn::default(),
                msn,
            };
            let packet_cnt = calculate_packet_cnt(qp.pmtu, raddr, total_len);
            let first_pkt_psn = {
                let mut send_psn = qp.sending_psn.lock();
                let first_pkt_psn = *send_psn;
                *send_psn = send_psn.wrapping_add(packet_cnt);
                first_pkt_psn
            };
            common.psn = first_pkt_psn;
            common
        };

        let desc = ToCardWorkRbDescBuilder::new_write()
            .with_common(common)
            .with_sge(sge0)
            .build()?;
        self.send_work_desc(desc)?;

        let ctx = WriteOpCtx::new_running();

        self.0
            .write_op_ctx_map
            .write()
            .insert(msn, ctx.clone()).map_or_else(||Ok(()),|_|Err(Error::CreateOpCtxFailed))?;
        Ok(ctx)
    }

    /// RDMA read operation
    /// 
    /// # Errors
    ///
    /// Will return `Err` if:
    /// * lock poisoned
    /// * failed to create a read descriptor
    /// * failed to send a read descriptor
    /// * failed to create a operation context
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
            let qp_guard = self.0.qp_table.read();
            let qp = qp_guard.get(&dqpn).ok_or(Error::Invalid(format!("Qpn :{dqpn:?}")))?;
            let mut common = ToCardWorkRbDescCommon {
                total_len,
                raddr,
                rkey,
                dqp_ip: qp.dqp_ip,
                dqpn: qp.qpn,
                mac_addr: qp.dqp_mac_addr,
                pmtu: qp.pmtu,
                flags,
                qp_type: qp.qp_type,
                psn: Psn::default(),
                msn,
            };
            let first_pkt_psn = {
                let mut send_psn = qp.sending_psn.lock();
                let first_pkt_psn = *send_psn;
                *send_psn = send_psn.wrapping_add(1);
                first_pkt_psn
            };
            common.psn = first_pkt_psn;
            common
        };

        let desc = ToCardWorkRbDescBuilder::new_read()
            .with_common(common)
            .with_sge(sge)
            .build()?;
        self.send_work_desc(desc)?;

        let ctx = WriteOpCtx::new_running();
        self.0
            .read_op_ctx_map
            .write()
            .insert(msn, ctx.clone()).map_or_else(||Ok(()),|_|Err(Error::CreateOpCtxFailed))?;

        Ok(ctx)
    }

    fn do_ctrl_op(&self, id: u32, desc: ToCardCtrlRbDesc) -> Result<CtrlOpCtx, Error> {
        // save operation context for unparking
        let ctrl_ctx = {
            let mut ctx = self.0.ctrl_op_ctx_map.write();
            let ctrl_ctx = CtrlOpCtx::new_running();

            if ctx.insert(id, ctrl_ctx.clone()).is_some() {
                return Err(Error::CreateOpCtxFailed);
            }

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

    fn init(&self,use_huge_page : bool) -> Result<(), Error> {
        let (send_queue, rece_queue) = unbounded();
        let recv_pkt_map = Arc::new(RwLock::new(HashMap::new()));

        // enable ctrl desc poller module
        let ctrl_thread_ctx = ControlPollerContext{
            to_host_ctrl_rb: self.0.adaptor.to_host_ctrl_rb(),
            ctrl_op_ctx_map: Arc::<RwLock<HashMap<u32, CtrlOpCtx>>>::clone(&self.0.ctrl_op_ctx_map)
        };
        let ctrl_desc_poller = ControlPoller::new(ctrl_thread_ctx);
        self.0.ctrl_desc_poller.set(ctrl_desc_poller).map_err(|_|Error::DoubleInit("ctrl_desc_poller has been set".to_owned()))?;

        // enable responser module
        let mut buf = Buffer::new(ACKNOWLEDGE_BUFFER_SIZE, use_huge_page).map_err(|e| Error::ResourceNoAvailable(format!("hugepage {e}")))?;
        let ack_buf = self.init_buf(&mut buf,ACKNOWLEDGE_BUFFER_SIZE)?;
        self.0.buffer_keeper.lock().push(buf);

        let responser = DescResponser::new(
            Arc::new(self.clone()),
            rece_queue,
            ack_buf,
            Arc::<RwLock<HashMap<Qpn, QpContext>>>::clone(&self.0.qp_table),
        );
        self.0.responser.set(responser).map_err(|_|Error::DoubleInit("responser has been set".to_owned()))?;
        self.prepare_nic_recv_buf(use_huge_page)?;

        let (nic_notify_send_queue, _nic_notify_recv_queue) = unbounded();

        // enable work desc poller module.
        let work_desc_poller_ctx = WorkDescPollerContext{
            work_rb : self.0.adaptor.to_host_work_rb(),
            recv_pkt_map : Arc::<RwLock<HashMap<Msn, Arc<RecvPktMap>>>>::clone(&recv_pkt_map),
            qp_table : Arc::<RwLock<HashMap<Qpn, QpContext>>>::clone(&self.0.qp_table),
            sending_queue : send_queue.clone(),
            write_op_ctx_map : Arc::<RwLock<HashMap<Msn, WriteOpCtx>>>::clone(&self.0.write_op_ctx_map),
            nic_notification_queue : nic_notify_send_queue,
        };
        let work_desc_poller = WorkDescPoller::new(work_desc_poller_ctx);
        self.0.work_desc_poller.set(work_desc_poller).map_err(|_|Error::DoubleInit("work descriptor poller has been set".to_owned()))?;

        // create nic recv and send device

        // let tx_buf = self.init_nic_buf()?;
        // self.0.nic_device.set(BasicNicDeivce::new(self.clone(), tx_buf, nic_notify_recv_queue)).map_err(|_|Error::DoubleInit("work descriptor poller has been set".to_owned()))?;

        // enable packet checker module
        let pkt_checker_thread = PacketChecker::new(
            send_queue,
            recv_pkt_map,
            Arc::<RwLock<HashMap<Msn, ReadOpCtx>>>::clone(&self.0.read_op_ctx_map),
        );
        self.0.pkt_checker_thread.set(pkt_checker_thread).map_err(|_|Error::DoubleInit("packet checker has been set".to_owned()))?;

        // set card network
        self.set_network(&self.0.local_network)?;

        Ok(())
    }

    #[allow(clippy::unwrap_in_result,clippy::unwrap_used)]
    fn prepare_nic_recv_buf(&self,use_huge_page:bool) -> Result<(), Error> {
        // configure basic nic recv buffer
        let op_id = self.get_ctrl_op_id();
        let mut buf = Buffer::new(NIC_BUFFER_SIZE, use_huge_page).map_err(|e| Error::ResourceNoAvailable(format!("hugepage {e}")))?;
        let recv_buf = self.init_buf(&mut buf,NIC_BUFFER_SIZE)?;
        self.0.buffer_keeper.lock().push(buf);

        let (start_va,lkey) = recv_buf.get_register_params();
        let set_raw_desc = ToCardCtrlRbDesc::SetRawPacketReceiveMeta(ToCardCtrlRbDescSetRawPacketReceiveMeta {
            common: ToCardCtrlRbDescCommon { op_id },
            base_write_addr : start_va as u64,
            key: lkey,
        });
        let set_raw_ctx = self.do_ctrl_op(op_id, set_raw_desc)?;
        let is_set_raw_success = set_raw_ctx.wait_result()?.ok_or_else(||Error::SetCtxResultFailed)?;
        if !is_set_raw_success {
            return Err(Error::DeviceReturnFailed("Network param"));
        };
        Ok(())
    }

    fn set_network(&self, network: &RdmaDeviceNetworkParam) -> Result<(), Error> {
        let op_id = self.get_ctrl_op_id();
        let desc = ToCardCtrlRbDesc::SetNetworkParam(ToCardCtrlRbDescSetNetworkParam {
            common: ToCardCtrlRbDescCommon { op_id },
            gateway: network.gateway,
            netmask: network.netmask,
            ipaddr: network.ipaddr,
            macaddr: network.macaddr,
        });
        let ctx = self.do_ctrl_op(op_id, desc)?;
        let is_success = ctx.wait_result()?.ok_or_else(||Error::SetCtxResultFailed)?;
        if !is_success {
            return Err(Error::DeviceReturnFailed("Network param"));
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

/// A interface that allows `DescResponser` to push the work descriptor to the device
pub(crate) trait WorkDescriptorSender: Send + Sync {
    fn send_work_desc(&self, desc_builder: ToCardWorkRbDesc) -> Result<(), Error>;
}

impl WorkDescriptorSender for Device {
    fn send_work_desc(&self, desc: ToCardWorkRbDesc) -> Result<(), Error> {
        self.0
            .adaptor
            .to_card_work_rb()
            .push(desc)
            .map_err(|_| Error::DeviceBusy)?;
        Ok(())
    }
}
