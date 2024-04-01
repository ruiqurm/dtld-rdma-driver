use log::{debug, error};

use self::rpc_cli::{
    RpcClient, ToCardCtrlRbCsrProxy, ToCardWorkRbCsrProxy, ToHostCtrlRbCsrProxy,
    ToHostWorkRbCsrProxy,
};
use super::{
    constants, ringbuf::Ringbuf, DeviceAdaptor, DeviceError, ToCardCtrlRbDesc, ToCardRb,
    ToCardWorkRbDesc, ToHostCtrlRbDesc, ToHostRb, ToHostWorkRbDesc,
};
use std::{
    net::SocketAddr,
    sync::{Arc, Mutex},
    thread::spawn,
};

#[cfg(feature = "scheduler")]
use super::scheduler::DescriptorScheduler;

mod rpc_cli;

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

/// An emulated device implementation of the device.
#[derive(Debug)]
pub(crate) struct EmulatedDevice {
    // FIXME: Temporarily ,we use Mutex to make the Rb imuumtable as well as thread safe
    to_card_ctrl_rb: Mutex<ToCardCtrlRb>,
    #[cfg(not(feature = "scheduler"))]
    to_card_work_rb: Mutex<ToCardWorkRb>,
    to_host_ctrl_rb: Mutex<ToHostCtrlRb>,
    to_host_work_rb: Mutex<ToHostWorkRb>,
    heap_mem_start_addr: usize,
    rpc_cli: RpcClient,
    #[cfg(feature = "scheduler")]
    scheduler: Arc<DescriptorScheduler>,
}

impl EmulatedDevice {
    /// Initializing an emulated device.
    /// This function needs to be synchronized.
    ///
    /// Here we allow the `cast_possible_truncation` lint because before every transcation
    /// we used an "AND" mask to perform truncation.
    #[allow(clippy::cast_possible_truncation)]
    pub(crate) fn init(
        rpc_server_addr: SocketAddr,
        heap_mem_start_addr: usize,
        #[cfg(feature = "scheduler")] scheduler: Arc<DescriptorScheduler>,
    ) -> Result<Arc<Self>, DeviceError> {
        let rpc_cli =
            RpcClient::new(rpc_server_addr).map_err(|e| DeviceError::Device(e.to_string()))?;

        let (to_card_ctrl_rb, to_card_ctrl_rb_addr) =
            ToCardCtrlRb::new(ToCardCtrlRbCsrProxy::new(rpc_cli.clone()));
        let (to_host_ctrl_rb, to_host_ctrl_rb_addr) =
            ToHostCtrlRb::new(ToHostCtrlRbCsrProxy::new(rpc_cli.clone()));
        let (to_card_work_rb, to_card_work_rb_addr) =
            ToCardWorkRb::new(ToCardWorkRbCsrProxy::new(rpc_cli.clone()));
        let (to_host_work_rb, to_host_work_rb_addr) =
            ToHostWorkRb::new(ToHostWorkRbCsrProxy::new(rpc_cli.clone()));

        let dev = Arc::new(Self {
            to_card_ctrl_rb: Mutex::new(to_card_ctrl_rb),
            to_host_ctrl_rb: Mutex::new(to_host_ctrl_rb),
            to_host_work_rb: Mutex::new(to_host_work_rb),
            heap_mem_start_addr,
            rpc_cli,
            #[cfg(feature = "scheduler")]
            scheduler: Arc::<DescriptorScheduler>::clone(&scheduler),
            #[cfg(not(feature = "scheduler"))]
            to_card_work_rb: Mutex::new(to_card_work_rb),
        });

        let pa_of_to_card_ctrl_rb_addr = dev.get_phys_addr(to_card_ctrl_rb_addr)?;
        dev.rpc_cli.write_csr(
            constants::CSR_ADDR_CMD_REQ_QUEUE_ADDR_LOW,
            (pa_of_to_card_ctrl_rb_addr & 0xFFFF_FFFF) as u32,
        )?;
        dev.rpc_cli.write_csr(
            constants::CSR_ADDR_CMD_REQ_QUEUE_ADDR_HIGH,
            (pa_of_to_card_ctrl_rb_addr >> 32) as u32,
        )?;

        let pa_of_to_host_ctrl_rb_addr = dev.get_phys_addr(to_host_ctrl_rb_addr)?;
        dev.rpc_cli.write_csr(
            constants::CSR_ADDR_CMD_RESP_QUEUE_ADDR_LOW,
            (pa_of_to_host_ctrl_rb_addr & 0xFFFF_FFFF) as u32,
        )?;
        dev.rpc_cli.write_csr(
            constants::CSR_ADDR_CMD_RESP_QUEUE_ADDR_HIGH,
            (pa_of_to_host_ctrl_rb_addr >> 32) as u32,
        )?;

        let pa_of_to_card_work_rb_addr = dev.get_phys_addr(to_card_work_rb_addr)?;
        dev.rpc_cli.write_csr(
            constants::CSR_ADDR_SEND_QUEUE_ADDR_LOW,
            (pa_of_to_card_work_rb_addr & 0xFFFF_FFFF) as u32,
        )?;
        dev.rpc_cli.write_csr(
            constants::CSR_ADDR_SEND_QUEUE_ADDR_HIGH,
            (pa_of_to_card_work_rb_addr >> 32) as u32,
        )?;

        let pa_of_to_host_work_rb_addr = dev.get_phys_addr(to_host_work_rb_addr)?;
        dev.rpc_cli.write_csr(
            constants::CSR_ADDR_META_REPORT_QUEUE_ADDR_LOW,
            (pa_of_to_host_work_rb_addr & 0xFFFF_FFFF) as u32,
        )?;
        dev.rpc_cli.write_csr(
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

impl DeviceAdaptor for Arc<EmulatedDevice> {
    fn to_card_ctrl_rb(&self) -> Arc<dyn ToCardRb<ToCardCtrlRbDesc>> {
        Arc::<EmulatedDevice>::clone(self)
    }

    fn to_host_ctrl_rb(&self) -> Arc<dyn ToHostRb<ToHostCtrlRbDesc>> {
        Arc::<EmulatedDevice>::clone(self)
    }

    fn to_card_work_rb(&self) -> Arc<dyn ToCardRb<ToCardWorkRbDesc>> {
        #[cfg(feature = "scheduler")]
        {
            Arc::<DescriptorScheduler>::clone(&self.scheduler)
        }
        #[cfg(not(feature = "scheduler"))]
        {
            Arc::<EmulatedDevice>::clone(self)
        }
    }

    fn to_host_work_rb(&self) -> Arc<dyn ToHostRb<ToHostWorkRbDesc>> {
        Arc::<EmulatedDevice>::clone(self)
    }

    fn read_csr(&self, addr: usize) -> Result<u32, DeviceError> {
        self.rpc_cli.read_csr(addr)
    }

    fn write_csr(&self, addr: usize, data: u32) -> Result<(), DeviceError> {
        self.rpc_cli.write_csr(addr, data)
    }

    fn get_phys_addr(&self, virt_addr: usize) -> Result<usize, DeviceError> {
        Ok(virt_addr - self.heap_mem_start_addr)
    }
}

impl ToCardRb<ToCardCtrlRbDesc> for EmulatedDevice {
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

impl ToHostRb<ToHostCtrlRbDesc> for EmulatedDevice {
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

#[cfg(not(feature = "scheduler"))]
impl ToCardRb<ToCardWorkRbDesc> for EmulatedDevice {
    fn push(&self, desc: ToCardWorkRbDesc) -> Result<(), DeviceError> {
        // TODO: the card might not be able to handle "part of the desc"
        // So me might need to ensure we have enough space to write the whole desc before writing
        push_to_card_work_rb_desc(&self.to_card_work_rb, desc)
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

impl ToHostRb<ToHostWorkRbDesc> for EmulatedDevice {
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
                Err(incomplete_desc) => {
                    let next_mem = reader.next().ok_or(DeviceError::Device(
                        "Failed to read from ringbuf".to_owned(),
                    ))?;
                    read_res = incomplete_desc.read(next_mem);
                    match read_res {
                        Ok(desc) => break Ok(desc),
                        Err(_) => {
                            todo!();
                        }
                    }
                }
            }
        }
    }
}
