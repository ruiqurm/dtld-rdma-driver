use std::{
    error::Error,
    net::Ipv4Addr,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    thread::{spawn, JoinHandle},
};

use flume::{unbounded, Receiver};
use log::debug;

use crate::SchedulerStrategy;

use self::{
    logic::{BlueRDMALogic, BlueRdmaLogicError},
    net_agent::udp_agent::{UDPReceiveAgent, UDPSendAgent},
};

use super::{
    scheduler::DescriptorScheduler, DeviceAdaptor, DeviceError, ToCardCtrlRbDesc, ToCardRb,
    ToCardWorkRbDesc, ToHostCtrlRbDesc, ToHostRb, ToHostWorkRbDesc,
};

mod logic;
mod net_agent;
mod packet;
mod packet_processor;
#[cfg(test)]
pub(crate) mod tests;
mod types;

/// An software device implementation of the device.
#[allow(dead_code)]
#[derive(Debug)]
pub(crate) struct SoftwareDevice<Strat:SchedulerStrategy> {
    recv_agent: UDPReceiveAgent,
    device: Arc<BlueRDMALogic>,
    stop_flag: Arc<AtomicBool>,
    polling_thread: Option<JoinHandle<()>>,
    to_card_work_rb: ToCardWorkRb<Strat>,
    to_host_work_rb: ToHostWorkRb,
    to_host_ctrl_rb: ToHostCtrlRb,
}

#[derive(Debug, Clone)]
struct ToCardWorkRb<Strat:SchedulerStrategy>(Arc<DescriptorScheduler<Strat>>);

#[derive(Debug, Clone)]
struct ToHostWorkRb(Receiver<ToHostWorkRbDesc>);

#[derive(Debug, Clone)]
struct ToHostCtrlRb(Receiver<ToHostCtrlRbDesc>);

impl<Strat:SchedulerStrategy> SoftwareDevice<Strat> {
    /// Initializing an software device.
    pub(crate) fn new(
        addr: Ipv4Addr,
        port: u16,
        scheduler: Arc<DescriptorScheduler<Strat>>,
    ) -> Result<Self, Box<dyn Error>> {
        let send_agent = UDPSendAgent::new(addr, port)?;
        let (ctrl_sender, ctrl_receiver) = unbounded();
        let (work_sender, work_receiver) = unbounded();
        let device = Arc::new(BlueRDMALogic::new(
            Arc::new(send_agent),
            ctrl_sender,
            work_sender,
        ));
        let recv_agent = UDPReceiveAgent::new(Arc::<BlueRDMALogic>::clone(&device), addr, port)?;

        let this_scheduler = Arc::<DescriptorScheduler<Strat>>::clone(&scheduler);
        let this_device = Arc::<BlueRDMALogic>::clone(&device);

        let stop_flag = Arc::new(AtomicBool::new(false));
        let thread_stop_flag = Arc::clone(&stop_flag);

        let polling_thread = spawn(move || {
            while !thread_stop_flag.load(Ordering::Relaxed) {
                match this_scheduler.pop_batch() {
                    Ok((result, _n)) => {
                        for desc in result.into_iter().flatten() {
                            let _: Result<(), BlueRdmaLogicError> = this_device.send(desc.into_desc());
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

impl<Strat:SchedulerStrategy> Drop for SoftwareDevice<Strat> {
    fn drop(&mut self) {
        self.stop_flag.store(true, Ordering::Relaxed);
        if let Some(thread) = self.polling_thread.take() {
            if let Err(e) = thread.join() {
                panic!("{}", format!("SoftwareDevice thread join failed: {e:?}"));
            }
        }
    }
}

impl<Strat:SchedulerStrategy> DeviceAdaptor for SoftwareDevice<Strat> {
    fn to_card_ctrl_rb(&self) -> Arc<dyn ToCardRb<ToCardCtrlRbDesc>> {
        Arc::<BlueRDMALogic>::clone(&self.device)
    }

    fn to_host_ctrl_rb(&self) -> Arc<dyn ToHostRb<ToHostCtrlRbDesc>> {
        Arc::new(self.to_host_ctrl_rb.clone())
    }

    fn to_card_work_rb(&self) -> Arc<dyn ToCardRb<Box<ToCardWorkRbDesc>>> {
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

    fn use_hugepage(&self) -> bool {
        false
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
        self.0
            .recv()
            .map_err(|e| DeviceError::Device(e.to_string()))
    }
}

impl ToHostRb<ToHostWorkRbDesc> for ToHostWorkRb {
    fn pop(&self) -> Result<ToHostWorkRbDesc, DeviceError> {
        self.0
            .recv()
            .map_err(|e| DeviceError::Device(e.to_string()))
    }
}

impl<Strat:SchedulerStrategy> ToCardRb<Box<ToCardWorkRbDesc>> for ToCardWorkRb<Strat> {
    fn push(&self, desc: Box<ToCardWorkRbDesc>) -> Result<(), DeviceError> {
        debug!("driver to card SQ: {:?}", desc);
        self.0.push(desc)
    }
}
