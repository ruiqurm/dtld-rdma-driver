use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    thread::sleep,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use flume::Receiver;

use crate::{
    device::ToCardWorkRbDesc,
    types::{Msn, Qpn},
    Device, WorkDescriptorSender,
};

const RETRY_TIMEOUT_IN_MS: u128 = 10000; // 10s
const MAX_RETRY: u32 = 3;
const CHECKING_INTERVAL_IN_MS: u64 = 1000; // 1s

struct RetryContext {
    descriptor: ToCardWorkRbDesc,
    retry_counter: u32,
    next_timeout: u128,
}

// Main thread will send a retry record to retry monitor
pub(crate) struct RetryRecord {
    descriptor: ToCardWorkRbDesc,
    qpn: Qpn,
    msn: Msn,
}

pub(crate) struct RetryCancel {
    qpn: Qpn,
    msn: Msn,
}

pub(crate) enum RetryEvent {
    Retry(RetryRecord),
    Cancel(RetryCancel),
}

pub(crate) struct RetryMonitorContext {
    map: HashMap<(Qpn, Msn), RetryContext>,
    receiver: Receiver<RetryEvent>,
    device: Device,
}

/// get current time in ms
#[allow(clippy::unwrap_used)]
fn get_current_time() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis()
}

pub(crate) struct RetryMonitor {
    stop_flag: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl RetryMonitor {
    pub(crate) fn new(device: Device, receiver: Receiver<RetryEvent>) -> Self {
        let stop_flag = Arc::new(AtomicBool::new(false));
        let stop_flag_clone = Arc::<AtomicBool>::clone(&stop_flag);
        let mut context = RetryMonitorContext {
            map: HashMap::new(),
            receiver,
            device,
        };
        let thread = std::thread::spawn(move || {
            retry_monitor_working_thread(&stop_flag_clone, &mut context);
        });
        Self {
            stop_flag,
            thread: Some(thread),
        }
    }
}

impl RetryMonitorContext {
    fn check_receive(&mut self) {
        while let Ok(record) = self.receiver.try_recv() {
            match record {
                RetryEvent::Retry(record) => self.handle_retry(record),
                RetryEvent::Cancel(cancel) => self.handle_cancel(&cancel),
            }
        }
    }

    #[allow(clippy::arithmetic_side_effects)]
    fn handle_retry(&mut self, record: RetryRecord) {
        let key = (record.qpn, record.msn);
        let ctx = RetryContext {
            descriptor: record.descriptor,
            retry_counter: MAX_RETRY,
            next_timeout: get_current_time() + RETRY_TIMEOUT_IN_MS,
        };
        if self.map.insert(key, ctx).is_some() {
            // receive same record more than once
            log::warn!("Receive same retry record more than once");
        }
    }

    fn handle_cancel(&mut self, cancel: &RetryCancel) {
        let key = (cancel.qpn, cancel.msn);
        if self.map.remove(&key).is_none() {
            // remove failed
            log::warn!("Remove retry record failed.");
        }
    }

    #[allow(clippy::arithmetic_side_effects)]
    fn check_timeout(&mut self) {
        // self.device.0.write_op_ctx_map.read();
        let now = get_current_time();
        for (key, ctx) in self.map.iter_mut() {
            if ctx.next_timeout <= now {
                if ctx.retry_counter > 0 {
                    ctx.retry_counter -= 1;
                    ctx.next_timeout = now + RETRY_TIMEOUT_IN_MS;
                    if self.device.send_work_desc(ctx.descriptor.clone()).is_err() {
                        log::error!("Retry send work descriptor failed")
                    }
                } else {
                    // Encounter max retry, remove it and tell user the error
                    let mut guard = self.device.0.write_op_ctx_map.write();
                    if let Some(write_op_ctx) = guard.remove(&key.1) {
                        write_op_ctx.set_error("exceed max retry count");
                    } else {
                        log::warn!("Remove retry record failed: Can not find {key:?}");
                    }
                }
            }
        }
    }
}

fn retry_monitor_working_thread(stop_flag: &AtomicBool, monitor: &mut RetryMonitorContext) {
    while !stop_flag.load(Ordering::Relaxed) {
        monitor.check_receive();
        monitor.check_timeout();
        // sleep for an interval
        sleep(Duration::from_millis(CHECKING_INTERVAL_IN_MS));
    }
}
