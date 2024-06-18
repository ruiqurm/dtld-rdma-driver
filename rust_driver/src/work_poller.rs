use core::panic;
use flume::Sender;
use log::{debug, error, info};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};

use crate::{
    buf::Slot,
    checker::PacketCheckEvent,
    device::{
        DeviceError, ToHostRb, ToHostWorkRbDesc, ToHostWorkRbDescRaw, ToHostWorkRbDescStatus, ToHostWorkRbDescWriteWithImm
    },
    nic::NicRecvNotification,
    Error,
};

#[allow(clippy::module_name_repetitions)]
#[derive(Debug)]
pub(crate) struct WorkDescPoller {
    thread: Option<std::thread::JoinHandle<()>>,
    stop_flag: Arc<AtomicBool>,
}

pub(crate) struct WorkDescPollerContext {
    pub(crate) work_rb: Arc<dyn ToHostRb<ToHostWorkRbDesc>>,
    pub(crate) checker_channel: Sender<PacketCheckEvent>,
    pub(crate) nic_channel: Sender<NicRecvNotification>,
}

unsafe impl Send for WorkDescPollerContext {}

impl WorkDescPoller {
    pub(crate) fn new(ctx: WorkDescPollerContext) -> Self {
        let stop_flag = Arc::new(AtomicBool::new(false));
        let thread_stop_flag = Arc::clone(&stop_flag);
        let thread = std::thread::spawn(move || {
            WorkDescPollerContext::poll_working_thread(&ctx, &thread_stop_flag);
        });
        Self {
            thread: Some(thread),
            stop_flag,
        }
    }
}

impl WorkDescPollerContext {
    pub(crate) fn poll_working_thread(ctx: &Self, stop_flag: &AtomicBool) {
        while !stop_flag.load(Ordering::Relaxed) {
            if ctx.recv_a_desc().is_err(){
                break;
            }
        }
    }

    pub(crate) fn recv_a_desc(&self) -> Result<(), Error> {
        let desc = match self.work_rb.pop() {
            Ok(desc) => desc,
            Err(DeviceError::ParseDesc(e)) => {
                error!("parse descriptor failed : {:?}", e);
                return Ok(());
            }
            Err(e)=>{
                error!("WorkDescPoller is stopped due to : {:?}", e);
                return Ok(());
            }
        };
        debug!("driver read from card RQ: {:?}", &desc);
        if !matches!(desc.status(), ToHostWorkRbDescStatus::Normal) {
            error!("desc status is {:?}", desc.status());
            return Ok(());
        }

        let result = match desc {
            ToHostWorkRbDesc::Read(desc) => self.handle_work_desc_to_checker(desc),
            ToHostWorkRbDesc::WriteOrReadResp(desc) => self.handle_work_desc_to_checker(desc),
            ToHostWorkRbDesc::WriteWithImm(desc) => self.handle_work_desc_write_with_imm(&desc),
            ToHostWorkRbDesc::Ack(desc) => self.handle_work_desc_to_checker(desc),
            ToHostWorkRbDesc::Raw(desc) => self.handle_work_desc_raw(&desc),
        };
        if let Err(reason) = result {
            error!("poll_work_rb stopped: {}", reason);
            return Err(reason);
        }
        Ok(())
    }

    #[inline]
    fn handle_work_desc_to_checker<T>(&self, desc: T) -> Result<(), Error>
    where
        PacketCheckEvent: From<T>,
    {
        let msg = PacketCheckEvent::from(desc);
        self.checker_channel
            .send(msg)
            .map_err(|_| Error::PipeBroken("work polling thread to responser"))
    }

    fn handle_work_desc_write_with_imm(
        &self,
        _desc: &ToHostWorkRbDescWriteWithImm,
    ) -> Result<(), Error> {
        todo!()
    }

    #[inline]
    fn handle_work_desc_raw(&self, desc: &ToHostWorkRbDescRaw) -> Result<(), Error> {
        let slot = unsafe { Slot::from_raw_parts_mut(desc.addr as *mut u8, desc.key) };
        self.nic_channel
            .send(NicRecvNotification {
                buf: slot,
                len: desc.len,
            })
            .map_err(|_| Error::PipeBroken("work polling thread to nic thread"))
    }
}

impl Drop for WorkDescPoller {
    fn drop(&mut self) {
        self.stop_flag.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            if let Err(e) = thread.join() {
                panic!("{}", format!("WorkDescPoller thread join failed: {e:?}"));
            }
            info!("WorkDescPoller thread is normally stopped");
        }
    }
}