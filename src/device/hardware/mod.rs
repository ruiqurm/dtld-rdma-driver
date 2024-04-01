use super::{
    DeviceAdaptor, DeviceError, ToCardCtrlRbDesc, ToCardRb, ToCardWorkRbDesc, ToHostCtrlRbDesc,
    ToHostRb, ToHostWorkRbDesc,
};
use std::sync::Arc;

#[allow(clippy::struct_field_names)]
#[derive(Debug)]
pub(crate) struct HardwareDevice {
    to_card_ctrl_rb: ToCardCtrlRb,
    to_host_ctrl_rb: ToHostCtrlRb,
    to_card_work_rb: ToCardWorkRb,
    to_host_work_rb: ToHostWorkRb,
}

#[derive(Debug, Clone)]
struct ToCardCtrlRb;
#[derive(Debug, Clone)]
struct ToHostCtrlRb;
#[derive(Debug, Clone)]
struct ToCardWorkRb;
#[derive(Debug, Clone)]
struct ToHostWorkRb;

impl HardwareDevice {
    pub(crate) fn init() -> Self {
        Self {
            to_card_ctrl_rb: ToCardCtrlRb,
            to_host_ctrl_rb: ToHostCtrlRb,
            to_card_work_rb: ToCardWorkRb,
            to_host_work_rb: ToHostWorkRb,
        }
    }
}

impl DeviceAdaptor for HardwareDevice {
    fn to_card_ctrl_rb(&self) -> Arc<dyn ToCardRb<ToCardCtrlRbDesc>> {
        Arc::new(self.to_card_ctrl_rb.clone())
    }

    fn to_host_ctrl_rb(&self) -> Arc<dyn ToHostRb<ToHostCtrlRbDesc>> {
        Arc::new(self.to_host_ctrl_rb.clone())
    }

    fn to_card_work_rb(&self) -> Arc<dyn ToCardRb<ToCardWorkRbDesc>> {
        Arc::new(self.to_card_work_rb.clone())
    }

    fn to_host_work_rb(&self) -> Arc<dyn ToHostRb<ToHostWorkRbDesc>> {
        Arc::new(self.to_host_work_rb.clone())
    }

    fn read_csr(&self, _addr: usize) -> Result<u32, DeviceError> {
        todo!()
    }

    fn write_csr(&self, _addr: usize, _data: u32) -> Result<(), DeviceError> {
        todo!()
    }

    fn get_phys_addr(&self, virt_addr: usize) -> Result<usize, DeviceError> {
        Ok(virt_addr)
    }
}

impl ToCardRb<ToCardCtrlRbDesc> for ToCardCtrlRb {
    fn push(&self, _desc: ToCardCtrlRbDesc) -> Result<(), DeviceError> {
        todo!()
    }
}

impl ToHostRb<ToHostCtrlRbDesc> for ToHostCtrlRb {
    fn pop(&self) -> Result<ToHostCtrlRbDesc, DeviceError> {
        todo!()
    }
}

impl ToHostRb<ToHostWorkRbDesc> for ToHostWorkRb {
    fn pop(&self) -> Result<ToHostWorkRbDesc, DeviceError> {
        todo!()
    }
}

impl ToCardRb<ToCardWorkRbDesc> for ToCardWorkRb {
    fn push(&self, _desc: ToCardWorkRbDesc) -> Result<(), DeviceError> {
        todo!()
    }
}
