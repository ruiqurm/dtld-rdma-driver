use log::{debug, error};

use self::{
    csr_cli::{
        CsrClient, ToCardCtrlRbCsrProxy, ToCardWorkRbCsrProxy, ToHostCtrlRbCsrProxy,
        ToHostWorkRbCsrProxy,
    },
    phys_addr_resolver::PhysAddrResolver,
};

use super::{
    constants, ringbuf::Ringbuf, scheduler::DescriptorScheduler, DeviceAdaptor, DeviceError,
    ToCardCtrlRbDesc, ToCardRb, ToCardWorkRbDesc, ToHostCtrlRbDesc, ToHostRb, ToHostWorkRbDesc,
    ToHostWorkRbDescError,
};
use std::{
    path::Path,
    sync::{Arc, Mutex},
    thread::spawn,
};

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

#[allow(clippy::struct_field_names)]
#[derive(Debug)]
pub(crate) struct HardwareDevice {
    to_card_ctrl_rb: Mutex<ToCardCtrlRb>,
    to_host_ctrl_rb: Mutex<ToHostCtrlRb>,
    to_host_work_rb: Mutex<ToHostWorkRb>,
    csr_cli: CsrClient,
    scheduler: Arc<DescriptorScheduler>,
    phys_addr_resolver: PhysAddrResolver,
}

impl HardwareDevice {
    #[allow(clippy::cast_possible_truncation)]
    pub(crate) fn init<P: AsRef<Path>>(
        device_name: P,
        scheduler: Arc<DescriptorScheduler>,
    ) -> Result<Arc<Self>, DeviceError> {
        let csr_cli =
            CsrClient::new(device_name).map_err(|e| DeviceError::Device(e.to_string()))?;
        let (to_card_ctrl_rb, to_card_ctrl_rb_addr) =
            ToCardCtrlRb::new(ToCardCtrlRbCsrProxy::new(csr_cli.clone()));
        let (to_host_ctrl_rb, to_host_ctrl_rb_addr) =
            ToHostCtrlRb::new(ToHostCtrlRbCsrProxy::new(csr_cli.clone()));
        let (to_card_work_rb, to_card_work_rb_addr) =
            ToCardWorkRb::new(ToCardWorkRbCsrProxy::new(csr_cli.clone()));
        let (to_host_work_rb, to_host_work_rb_addr) =
            ToHostWorkRb::new(ToHostWorkRbCsrProxy::new(csr_cli.clone()));
        let phys_addr_resolver =
            PhysAddrResolver::new().map_err(|e| DeviceError::Device(e.to_string()))?;
        let dev = Arc::new(Self {
            to_card_ctrl_rb: Mutex::new(to_card_ctrl_rb),
            to_host_ctrl_rb: Mutex::new(to_host_ctrl_rb),
            to_host_work_rb: Mutex::new(to_host_work_rb),
            csr_cli,
            scheduler: Arc::<DescriptorScheduler>::clone(&scheduler),
            phys_addr_resolver,
        });

        let pa_of_to_card_ctrl_rb_addr = dev.get_phys_addr(to_card_ctrl_rb_addr)?;
        dev.csr_cli.write_csr(
            constants::CSR_ADDR_CMD_REQ_QUEUE_ADDR_LOW,
            (pa_of_to_card_ctrl_rb_addr & 0xFFFF_FFFF) as u32,
        )?;
        dev.csr_cli.write_csr(
            constants::CSR_ADDR_CMD_REQ_QUEUE_ADDR_HIGH,
            (pa_of_to_card_ctrl_rb_addr >> 32) as u32,
        )?;

        let pa_of_to_host_ctrl_rb_addr = dev.get_phys_addr(to_host_ctrl_rb_addr)?;
        dev.csr_cli.write_csr(
            constants::CSR_ADDR_CMD_RESP_QUEUE_ADDR_LOW,
            (pa_of_to_host_ctrl_rb_addr & 0xFFFF_FFFF) as u32,
        )?;
        dev.csr_cli.write_csr(
            constants::CSR_ADDR_CMD_RESP_QUEUE_ADDR_HIGH,
            (pa_of_to_host_ctrl_rb_addr >> 32) as u32,
        )?;

        let pa_of_to_card_work_rb_addr = dev.get_phys_addr(to_card_work_rb_addr)?;
        dev.csr_cli.write_csr(
            constants::CSR_ADDR_SEND_QUEUE_ADDR_LOW,
            (pa_of_to_card_work_rb_addr & 0xFFFF_FFFF) as u32,
        )?;
        dev.csr_cli.write_csr(
            constants::CSR_ADDR_SEND_QUEUE_ADDR_HIGH,
            (pa_of_to_card_work_rb_addr >> 32) as u32,
        )?;

        let pa_of_to_host_work_rb_addr = dev.get_phys_addr(to_host_work_rb_addr)?;
        dev.csr_cli.write_csr(
            constants::CSR_ADDR_META_REPORT_QUEUE_ADDR_LOW,
            (pa_of_to_host_work_rb_addr & 0xFFFF_FFFF) as u32,
        )?;
        dev.csr_cli.write_csr(
            constants::CSR_ADDR_META_REPORT_QUEUE_ADDR_HIGH,
            (pa_of_to_host_work_rb_addr >> 32) as u32,
        )?;

        #[cfg(feature = "scheduler")]
        {
            let _: std::thread::JoinHandle<_> = spawn(move || {
                let rb = Mutex::new(to_card_work_rb);
                loop {
                    match scheduler.pop() {
                        Ok(result) => {
                            if let Some(desc) = result {
                                if let Err(e) = push_to_card_work_rb_desc(&rb, &desc) {
                                    error!("push to to_card_work_rb failed: {:?}", e);
                                }
                            }
                        }
                        Err(e) => {
                            error!("scheduler pop failed:{:?}", e);
                        }
                    }
                }
            });
        }

        Ok(dev)
    }
}

impl DeviceAdaptor for Arc<HardwareDevice> {
    fn to_card_ctrl_rb(&self) -> Arc<dyn ToCardRb<ToCardCtrlRbDesc>> {
        Arc::<HardwareDevice>::clone(self)
    }

    fn to_host_ctrl_rb(&self) -> Arc<dyn ToHostRb<ToHostCtrlRbDesc>> {
        Arc::<HardwareDevice>::clone(self)
    }

    fn to_card_work_rb(&self) -> Arc<dyn ToCardRb<ToCardWorkRbDesc>> {
        Arc::<DescriptorScheduler>::clone(&self.scheduler)
    }

    fn to_host_work_rb(&self) -> Arc<dyn ToHostRb<ToHostWorkRbDesc>> {
        Arc::<HardwareDevice>::clone(self)
    }

    fn get_phys_addr(&self, virt_addr: usize) -> Result<usize, DeviceError> {
        self.phys_addr_resolver
            .query(virt_addr)
            .ok_or_else(|| DeviceError::Device(format!("Addr {virt_addr} not found")))
    }

    fn read_csr(&self, addr: usize) -> Result<u32, DeviceError> {
        self.csr_cli.read_csr(addr)
    }

    fn write_csr(&self, addr: usize, data: u32) -> Result<(), DeviceError> {
        self.csr_cli.write_csr(addr, data)
    }
}

impl ToCardRb<ToCardCtrlRbDesc> for HardwareDevice {
    fn push(&self, desc: ToCardCtrlRbDesc) -> Result<(), DeviceError> {
        let mut guard = self
            .to_card_ctrl_rb
            .lock()
            .map_err(|e| DeviceError::LockPoisoned(e.to_string()))?;
        let mut writer = guard.write()?;

        let mem = writer.next().ok_or(DeviceError::Overflow)?;
        debug!("{:?}", &desc);
        desc.write(mem);

        Ok(())
    }
}

impl ToHostRb<ToHostCtrlRbDesc> for HardwareDevice {
    fn pop(&self) -> Result<ToHostCtrlRbDesc, DeviceError> {
        let mut guard = self
            .to_host_ctrl_rb
            .lock()
            .map_err(|e| DeviceError::LockPoisoned(e.to_string()))?;
        let mut reader = guard.read()?;
        let mem = reader.next().ok_or(DeviceError::Device(
            "Failed to read from ringbuf".to_owned(),
        ))?;
        let desc = ToHostCtrlRbDesc::read(mem)?;
        debug!("{:?}", &desc);
        Ok(desc)
    }
}

fn push_to_card_work_rb_desc(
    rb: &Mutex<ToCardWorkRb>,
    desc: &ToCardWorkRbDesc,
) -> Result<(), DeviceError> {
    debug!("driver send to card SQ: {:?}", &desc);
    let mut guard = rb
        .lock()
        .map_err(|e| DeviceError::LockPoisoned(e.to_string()))?;
    let desc_cnt = desc.serialized_desc_cnt();
    let mut writer = guard.write()?;
    desc.write_0(writer.next().ok_or(DeviceError::Overflow)?);
    desc.write_1(writer.next().ok_or(DeviceError::Overflow)?);
    desc.write_2(writer.next().ok_or(DeviceError::Overflow)?);

    if desc_cnt == 4 {
        desc.write_3(writer.next().ok_or(DeviceError::Overflow)?);
    }

    Ok(())
}

impl ToHostRb<ToHostWorkRbDesc> for HardwareDevice {
    fn pop(&self) -> Result<ToHostWorkRbDesc, DeviceError> {
        let mut guard = self
            .to_host_work_rb
            .lock()
            .map_err(|e| DeviceError::LockPoisoned(e.to_string()))?;
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
