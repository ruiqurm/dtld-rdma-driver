use std::{
    error::Error,
    net::Ipv4Addr,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    thread::{ spawn, JoinHandle},
};

use flume::{unbounded, Receiver};
use log::debug;

use self::{
    logic::{BlueRDMALogic, BlueRdmaLogicError},
    net_agent::udp_agent::{UDPReceiveAgent, UDPSendAgent},
};

use super::{
    scheduler::{round_robin::RoundRobinStrategy, DescriptorScheduler},
    DeviceAdaptor, DeviceError, ToCardCtrlRbDesc, ToCardRb, ToCardWorkRbDesc, ToHostCtrlRbDesc,
    ToHostRb, ToHostWorkRbDesc,
};

mod logic;
mod net_agent;
mod packet;
mod packet_processor;
#[cfg(test)]
pub(crate) mod tests;
mod types;

/// An software device implementation of the device.
///
/// # Examples:
/// ```
/// let device = SoftwareDevice::init().unwrap();
/// let ctrl_rb = device.to_card_ctrl_rb();
// // ctrl_rb.push(desc) // create mr or qp
/// let data_send_rb = device.to_card_work_rb();
/// // data_rb.push(desc) // send data
/// let data_recv_rb = device.to_host_work_rb();
/// // data_recv_rb.pop() // recv data
/// ```
#[allow(dead_code)]
#[derive(Debug)]
pub(crate) struct SoftwareDevice {
    recv_agent: UDPReceiveAgent,
    device: Arc<BlueRDMALogic>,
    stop_flag: Arc<AtomicBool>,
    polling_thread: Option<JoinHandle<()>>,
    to_card_work_rb: ToCardWorkRb,
    to_host_work_rb: ToHostWorkRb,
    to_host_ctrl_rb: ToHostCtrlRb,
}

#[derive(Debug, Clone)]
struct ToCardWorkRb(Arc<DescriptorScheduler>);

#[derive(Debug, Clone)]
struct ToHostWorkRb(Receiver<ToHostWorkRbDesc>);

#[derive(Debug, Clone)]
struct ToHostCtrlRb(Receiver<ToHostCtrlRbDesc>);

impl SoftwareDevice {
    /// Initializing an software device.
    pub(crate) fn init(addr: Ipv4Addr, port: u16) -> Result<Self, Box<dyn Error>> {
        let send_agent = UDPSendAgent::new(addr, port)?;
        let (ctrl_sender, ctrl_receiver) = unbounded();
        let (work_sender, work_receiver) = unbounded();
        let device = Arc::new(BlueRDMALogic::new(
            Arc::new(send_agent),
            ctrl_sender,
            work_sender,
        ));
        // The strategy is a global singleton, so we leak it
        let round_robin = Arc::new(RoundRobinStrategy::new());
        let scheduler = DescriptorScheduler::new(round_robin);
        let scheduler = Arc::new(scheduler);
        let recv_agent = UDPReceiveAgent::new(Arc::<BlueRDMALogic>::clone(&device), addr, port)?;

        let this_scheduler = Arc::<DescriptorScheduler>::clone(&scheduler);
        let this_device = Arc::<BlueRDMALogic>::clone(&device);

        let stop_flag = Arc::new(AtomicBool::new(false));
        let thread_stop_flag = Arc::clone(&stop_flag);

        let polling_thread = spawn(move || {
            while !thread_stop_flag.load(Ordering::Relaxed) {
                match this_scheduler.pop() {
                    Ok(result) => {
                        if let Some(to_card_ctrl_rb_desc) = result {
                            let _: Result<(), BlueRdmaLogicError> =
                                this_device.send(to_card_ctrl_rb_desc);
                        }
                    }
                    Err(e) => {
                        log::error!("polling scheduler thread error: {:?}", e);
                    }
                }
            }
        });
        let to_card_work_rb = ToCardWorkRb(scheduler);
        Ok(Self {
            recv_agent,
            polling_thread: Some(polling_thread),
            device,
            to_card_work_rb,
            to_host_work_rb: ToHostWorkRb(work_receiver),
            to_host_ctrl_rb: ToHostCtrlRb(ctrl_receiver),
            stop_flag,
        })
    }
}

impl Drop for SoftwareDevice {
    fn drop(&mut self) {
        self.stop_flag.store(true, Ordering::Relaxed);
        if let Some(thread) = self.polling_thread.take() {
            if let Err(e) = thread.join() {
                panic!("{}", format!("SoftwareDevice thread join failed: {e:?}"));
            }
        }
    }
}

impl DeviceAdaptor for SoftwareDevice {
    fn to_card_ctrl_rb(&self) -> Arc<dyn ToCardRb<ToCardCtrlRbDesc>> {
        Arc::<BlueRDMALogic>::clone(&self.device)
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

impl ToCardRb<ToCardCtrlRbDesc> for BlueRDMALogic {
    fn push(&self, desc: ToCardCtrlRbDesc) -> Result<(), DeviceError> {
        self.update(desc)
            .map_err(|e| DeviceError::Device(e.to_string()))?;
        Ok(())
    }
}

impl ToHostRb<ToHostCtrlRbDesc> for ToHostCtrlRb {
    fn pop(&self) -> Result<ToHostCtrlRbDesc, DeviceError> {
        self.0.recv().map_err(|e|DeviceError::Device(e.to_string()))
    }
}

impl ToHostRb<ToHostWorkRbDesc> for ToHostWorkRb {
    fn pop(&self) -> Result<ToHostWorkRbDesc, DeviceError> {
        self.0.recv().map_err(|e|DeviceError::Device(e.to_string()))
    }
}

impl ToCardRb<ToCardWorkRbDesc> for ToCardWorkRb {
    fn push(&self, desc: ToCardWorkRbDesc) -> Result<(), DeviceError> {
        debug!("driver to card SQ: {:?}", desc);
        self.0.push(desc)
    }
}
