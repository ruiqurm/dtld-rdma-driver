use eui48::MacAddress;

use crate::{
    device::{ToCardCtrlRbDesc, ToCardCtrlRbDescCommon, ToCardCtrlRbDescQpManagement},
    types::{MemAccessTypeFlag, Pmtu, Psn, Qp, QpType, Qpn},
    Device, Error, Pd,
};
use std::{
    hash::{Hash, Hasher},
    net::Ipv4Addr,
    sync::{
        atomic::{AtomicBool, Ordering},
        Mutex,
    },
};

const QP_MAX_CNT: usize = 1024;

pub struct QpContext {
    pub(crate) pd: Pd,
    pub(crate) qpn: Qpn,
    pub(crate) qp_type: QpType,
    #[allow(unused)]
    pub(crate) rq_acc_flags: MemAccessTypeFlag,
    pub(crate) pmtu: Pmtu,
    #[allow(unused)]
    pub(crate) local_ip: Ipv4Addr,
    #[allow(unused)]
    pub(crate) local_mac_addr: MacAddress,
    pub(crate) dqp_ip: Ipv4Addr,
    pub(crate) dqp_mac_addr: MacAddress,
    pub(crate) sending_psn: Mutex<Psn>,
}

impl From<&Qp> for QpContext {
    fn from(qp: &Qp) -> QpContext {
        QpContext {
            pd: qp.pd.clone(),
            qpn: qp.qpn,
            qp_type: qp.qp_type,
            rq_acc_flags: qp.rq_acc_flags,
            pmtu: qp.pmtu.clone(),
            local_ip: qp.local_ip,
            local_mac_addr: qp.local_mac,
            dqp_ip: qp.dqp_ip,
            dqp_mac_addr: qp.dqp_mac,
            sending_psn: Mutex::new(Psn::new(0)),
        }
    }
}
impl Device {
    #[allow(clippy::too_many_arguments)]
    pub fn create_qp(&self, qp: &Qp) -> Result<(), Error> {
        let mut qp_pool = self.0.qp_table.write().unwrap();
        let mut pd_pool = self.0.pd.lock().unwrap();
        let pd = &qp.pd;
        let pd_ctx = pd_pool.get_mut(pd).ok_or(Error::InvalidPd)?;

        let qpc = QpContext::from(qp);
        let op_id = self.get_ctrl_op_id();

        let desc = ToCardCtrlRbDesc::QpManagement(ToCardCtrlRbDescQpManagement {
            common: ToCardCtrlRbDescCommon { op_id },
            is_valid: true,
            qpn: qp.qpn,
            pd_hdl: qp.pd.handle,
            qp_type: qp.qp_type,
            rq_acc_flags: qp.rq_acc_flags,
            pmtu: qp.pmtu.clone(),
        });

        let ctx = self.do_ctrl_op(op_id, desc)?;

        let res = ctx.wait_result().unwrap();

        if !res {
            return Err(Error::DeviceReturnFailed);
        }

        let pd_res = pd_ctx.qp.insert(qp.qpn);
        let qp_res = qp_pool.insert(qp.qpn, qpc);

        assert!(pd_res);
        assert!(qp_res.is_none());

        Ok(())
    }

    pub fn destroy_qp(&self, qp: Qpn) -> Result<(), Error> {
        let mut qp_pool = self.0.qp_table.write().unwrap();
        let mut pd_pool = self.0.pd.lock().unwrap();

        let op_id = self.get_ctrl_op_id();

        let (pd_ctx, desc) = if let Some(qp_ctx) = qp_pool.get(&qp) {
            let pd_ctx = pd_pool.get_mut(&qp_ctx.pd).ok_or(Error::InvalidPd)?;
            let desc = ToCardCtrlRbDesc::QpManagement(ToCardCtrlRbDescQpManagement {
                common: ToCardCtrlRbDescCommon { op_id },
                is_valid: false,
                qpn: qp_ctx.qpn,
                pd_hdl: 0,
                qp_type: qp_ctx.qp_type,
                rq_acc_flags: MemAccessTypeFlag::IbvAccessNoFlags,
                pmtu: qp_ctx.pmtu.clone(),
            });
            (pd_ctx, desc)
        } else {
            return Err(Error::InvalidQpn);
        };

        let ctx = self.do_ctrl_op(op_id, desc)?;

        let res = ctx.wait_result().unwrap();

        if !res {
            return Err(Error::DeviceReturnFailed);
        }

        pd_ctx.qp.remove(&qp);
        qp_pool.remove(&qp);

        Ok(())
    }
}

impl Hash for Qp {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.qpn.hash(state);
    }
}

impl PartialEq for Qp {
    fn eq(&self, other: &Self) -> bool {
        self.qpn == other.qpn
    }
}

impl Eq for Qp {}

pub struct QpManager {
    qp_availability: Box<[AtomicBool]>,
}

impl QpManager {
    pub fn new() -> Self {
        let qp_availability: Vec<AtomicBool> =
            (0..QP_MAX_CNT).map(|_| AtomicBool::new(true)).collect();

        // by IB spec, QP0 and QP1 are reserved, so qpn should start with 2
        qp_availability[0].store(false, Ordering::Relaxed);
        qp_availability[1].store(false, Ordering::Relaxed);

        Self {
            qp_availability: qp_availability.into_boxed_slice(),
        }
    }

    pub fn alloc(&self) -> Result<Qpn, Error> {
        self
            .qp_availability
            .iter()
            .enumerate()
            .find_map(|(idx, n)| n.swap(false, Ordering::AcqRel).then_some(Qpn::new(idx as u32)))
            .ok_or_else(|| Error::NoAvailableQp)
    }

    pub fn free(&self, qpn: Qpn) {
        self.qp_availability[qpn.get() as usize].store(true, Ordering::Relaxed);
    }
}

impl Default for QpManager {
    fn default() -> Self {
        Self::new()
    }
}
