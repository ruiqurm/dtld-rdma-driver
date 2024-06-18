use crate::{utils::Buffer, SchedulerStrategy};
use log::debug;
use parking_lot::Mutex;

use self::{
    csr_cli::{
        CsrClient, ToCardCtrlRbCsrProxy, ToCardWorkRbCsrProxy, ToHostCtrlRbCsrProxy,
        ToHostWorkRbCsrProxy,
    },
    phys_addr_resolver::PhysAddrResolver,
};

use super::{
    constants,
    ringbuf::Ringbuf,
    scheduler::DescriptorScheduler,
    DeviceAdaptor, DeviceError, ToCardCtrlRbDesc, ToCardRb, ToCardWorkRbDesc, ToHostCtrlRbDesc,
    ToHostRb, ToHostWorkRbDesc, ToHostWorkRbDescError,
};
use std::{path::Path, sync::Arc};

mod csr_cli;
mod phys_addr_resolver;

type ToCardCtrlRb = Ringbuf<
    ToCardCtrlRbCsrProxy,
    { constants::RINGBUF_DEPTH },
    { constants::RINGBUF_ELEM_SIZE },
    { constants::RINGBUF_PAGE_SIZE },
>;

type ToHostCtrlRb = Ringbuf<
    ToHostCtrlRbCsrProxy,
    { constants::RINGBUF_DEPTH },
    { constants::RINGBUF_ELEM_SIZE },
    { constants::RINGBUF_PAGE_SIZE },
>;

type ToCardWorkRb = Ringbuf<
    ToCardWorkRbCsrProxy,
    { constants::RINGBUF_DEPTH },
    { constants::RINGBUF_ELEM_SIZE },
    { constants::RINGBUF_PAGE_SIZE },
>;

type ToHostWorkRb = Ringbuf<
    ToHostWorkRbCsrProxy,
    { constants::RINGBUF_DEPTH },
    { constants::RINGBUF_ELEM_SIZE },
    { constants::RINGBUF_PAGE_SIZE },
>;

#[derive(Debug, Clone)]
pub(crate) struct HardwareDevice<Strat: SchedulerStrategy>(Arc<HardwareDeviceInner<Strat>>);

#[allow(clippy::struct_field_names)]
#[derive(Debug)]
pub(crate) struct HardwareDeviceInner<Strat: SchedulerStrategy> {
    to_card_ctrl_rb: Arc<Mutex<ToCardCtrlRb>>,
    to_host_ctrl_rb: Arc<Mutex<ToHostCtrlRb>>,
    to_host_work_rb: Arc<Mutex<ToHostWorkRb>>,
    csr_cli: CsrClient,
    scheduler: Arc<DescriptorScheduler<Strat>>,
    phys_addr_resolver: PhysAddrResolver,
}

impl<Strat: SchedulerStrategy> HardwareDevice<Strat> {
    #[allow(clippy::cast_possible_truncation)]
    pub(crate) fn new<P: AsRef<Path>>(
        device_path: P,
        strategy: Strat,
        scheduler_size:usize
    ) -> Result<Self, DeviceError> {
        let csr_cli =
            CsrClient::new(device_path).map_err(|e| DeviceError::Device(e.to_string()))?;

        let to_card_ctrl_rb_buffer = Buffer::new(constants::RINGBUF_PAGE_SIZE, true)
            .map_err(|e| DeviceError::Device(e.to_string()))?;
        let to_card_ctrl_rb_addr = to_card_ctrl_rb_buffer.as_ptr() as usize;
        let to_card_ctrl_rb = ToCardCtrlRb::new(
            ToCardCtrlRbCsrProxy::new(csr_cli.clone()),
            to_card_ctrl_rb_buffer,
        );

        let to_host_ctrl_rb = Buffer::new(constants::RINGBUF_PAGE_SIZE, true)
            .map_err(|e| DeviceError::Device(e.to_string()))?;
        let to_host_ctrl_rb_addr = to_host_ctrl_rb.as_ptr() as usize;
        let to_host_ctrl_rb =
            ToHostCtrlRb::new(ToHostCtrlRbCsrProxy::new(csr_cli.clone()), to_host_ctrl_rb);

        let to_card_work_rb_buffer = Buffer::new(constants::RINGBUF_PAGE_SIZE, true)
            .map_err(|e| DeviceError::Device(e.to_string()))?;
        let to_card_work_rb_addr = to_card_work_rb_buffer.as_ptr() as usize;
        let to_card_work_rb = ToCardWorkRb::new(
            ToCardWorkRbCsrProxy::new(csr_cli.clone()),
            to_card_work_rb_buffer,
        );

        let to_host_work_rb_buffer = Buffer::new(constants::RINGBUF_PAGE_SIZE, true)
            .map_err(|e| DeviceError::Device(e.to_string()))?;
        let to_host_work_rb_addr = to_host_work_rb_buffer.as_ptr() as usize;
        let to_host_work_rb = ToHostWorkRb::new(
            ToHostWorkRbCsrProxy::new(csr_cli.clone()),
            to_host_work_rb_buffer,
        );

        let phys_addr_resolver =
            PhysAddrResolver::new().map_err(|e| DeviceError::Device(e.to_string()))?;
        let scheduler = Arc::new(DescriptorScheduler::new(
            strategy,
            Mutex::new(to_card_work_rb),
            scheduler_size
        ));
        let dev = Self(Arc::new(HardwareDeviceInner {
            to_card_ctrl_rb: Mutex::new(to_card_ctrl_rb).into(),
            to_host_ctrl_rb: Mutex::new(to_host_ctrl_rb).into(),
            to_host_work_rb: Mutex::new(to_host_work_rb).into(),
            csr_cli,
            scheduler: Arc::<DescriptorScheduler<Strat>>::clone(&scheduler),
            phys_addr_resolver,
        }));

        let pa_of_to_card_ctrl_rb_addr = dev.get_phys_addr(to_card_ctrl_rb_addr)?;
        dev.0.csr_cli.write_csr(
            constants::CSR_ADDR_CMD_REQ_QUEUE_ADDR_LOW,
            (pa_of_to_card_ctrl_rb_addr & 0xFFFF_FFFF) as u32,
        )?;
        dev.0.csr_cli.write_csr(
            constants::CSR_ADDR_CMD_REQ_QUEUE_ADDR_HIGH,
            (pa_of_to_card_ctrl_rb_addr >> 32) as u32,
        )?;

        let pa_of_to_host_ctrl_rb_addr = dev.get_phys_addr(to_host_ctrl_rb_addr)?;
        dev.0.csr_cli.write_csr(
            constants::CSR_ADDR_CMD_RESP_QUEUE_ADDR_LOW,
            (pa_of_to_host_ctrl_rb_addr & 0xFFFF_FFFF) as u32,
        )?;
        dev.0.csr_cli.write_csr(
            constants::CSR_ADDR_CMD_RESP_QUEUE_ADDR_HIGH,
            (pa_of_to_host_ctrl_rb_addr >> 32) as u32,
        )?;

        let pa_of_to_card_work_rb_addr = dev.get_phys_addr(to_card_work_rb_addr)?;
        dev.0.csr_cli.write_csr(
            constants::CSR_ADDR_SEND_QUEUE_ADDR_LOW,
            (pa_of_to_card_work_rb_addr & 0xFFFF_FFFF) as u32,
        )?;
        dev.0.csr_cli.write_csr(
            constants::CSR_ADDR_SEND_QUEUE_ADDR_HIGH,
            (pa_of_to_card_work_rb_addr >> 32) as u32,
        )?;

        let pa_of_to_host_work_rb_addr = dev.get_phys_addr(to_host_work_rb_addr)?;
        dev.0.csr_cli.write_csr(
            constants::CSR_ADDR_META_REPORT_QUEUE_ADDR_LOW,
            (pa_of_to_host_work_rb_addr & 0xFFFF_FFFF) as u32,
        )?;
        dev.0.csr_cli.write_csr(
            constants::CSR_ADDR_META_REPORT_QUEUE_ADDR_HIGH,
            (pa_of_to_host_work_rb_addr >> 32) as u32,
        )?;
        Ok(dev)
    }
}

impl<Strat: SchedulerStrategy> DeviceAdaptor for HardwareDevice<Strat> {
    fn to_card_ctrl_rb(&self) -> Arc<dyn ToCardRb<ToCardCtrlRbDesc>> {
        Arc::<Mutex<ToCardCtrlRb>>::clone(&self.0.to_card_ctrl_rb)
    }

    fn to_host_ctrl_rb(&self) -> Arc<dyn ToHostRb<ToHostCtrlRbDesc>> {
        Arc::<Mutex<ToHostCtrlRb>>::clone(&self.0.to_host_ctrl_rb)
    }

    fn to_card_work_rb(&self) -> Arc<dyn ToCardRb<Box<ToCardWorkRbDesc>>> {
        Arc::<DescriptorScheduler<Strat>>::clone(&self.0.scheduler)
    }

    fn to_host_work_rb(&self) -> Arc<dyn ToHostRb<ToHostWorkRbDesc>> {
        Arc::<Mutex<ToHostWorkRb>>::clone(&self.0.to_host_work_rb)
    }

    fn get_phys_addr(&self, virt_addr: usize) -> Result<usize, DeviceError> {
        self.0
            .phys_addr_resolver
            .query(virt_addr)
            .ok_or_else(|| DeviceError::Device(format!("Addr {virt_addr} not found")))
    }

    fn read_csr(&self, addr: usize) -> Result<u32, DeviceError> {
        self.0.csr_cli.read_csr(addr)
    }

    fn write_csr(&self, addr: usize, data: u32) -> Result<(), DeviceError> {
        self.0.csr_cli.write_csr(addr, data)
    }

    fn use_hugepage(&self) -> bool {
        true
    }
}

#[allow(clippy::unwrap_used,clippy::unwrap_in_result)]
impl ToCardRb<ToCardCtrlRbDesc> for Mutex<ToCardCtrlRb> {
    fn push(&self, desc: ToCardCtrlRbDesc) -> Result<(), DeviceError> {
        let mut guard = self.lock();
        let mut writer = guard.write();

        let mem = writer.next().unwrap(); // If blockly write desc fail, it should panic
        debug!("{:?}", &desc);
        desc.write(mem);

        Ok(())
    }
}

impl ToHostRb<ToHostCtrlRbDesc> for Mutex<ToHostCtrlRb> {
    fn pop(&self) -> Result<ToHostCtrlRbDesc, DeviceError> {
        let mut guard = self.lock();
        let mut reader = guard.read()?;
        let mem = reader.next().ok_or(DeviceError::Device(
            "Failed to read from ringbuf".to_owned(),
        ))?;
        let desc = ToHostCtrlRbDesc::read(mem)?;
        debug!("{:?}", &desc);
        Ok(desc)
    }
}


impl ToHostRb<ToHostWorkRbDesc> for Mutex<ToHostWorkRb> {
    fn pop(&self) -> Result<ToHostWorkRbDesc, DeviceError> {
        let mut guard = self.lock();
        let mut reader = guard.read()?;

        let mem = reader.next().ok_or(DeviceError::Device(
            "Failed to read from ringbuf".to_owned(),
        ))?;
        let mut read_res = ToHostWorkRbDesc::read(mem);

        loop {
            match read_res {
                Ok(desc) => break Ok(desc),
                Err(ToHostWorkRbDescError::DeviceError(e)) => {
                    return Err(e);
                }
                Err(ToHostWorkRbDescError::Incomplete(incomplete_desc)) => {
                    let next_mem = reader.next().ok_or(DeviceError::Device(
                        "Failed to read from ringbuf".to_owned(),
                    ))?;
                    read_res = incomplete_desc.read(next_mem);
                }
            }
        }
    }
}
