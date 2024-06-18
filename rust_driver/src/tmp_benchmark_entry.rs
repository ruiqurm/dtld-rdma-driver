#![allow(clippy::unwrap_used,clippy::arithmetic_side_effects,clippy::shadow_unrelated)]
use std::{sync::{
    atomic::{AtomicU32, AtomicU8},
    Arc,
}, time::Instant};

use flume::unbounded;
use parking_lot::Mutex;

use crate::{
    device::{
        scheduler::{pop_and_write_to_ringbuf, push_to_scheduler}, CsrReaderProxy, CsrWriterProxy, DeviceError, Ringbuf, ToCardWorkRbDesc, ToCardWorkRbDescBuilder, ToCardWorkRbDescCommon, ToCardWorkRbDescOpcode, ToHostRb, ToHostWorkRbDesc, ToHostWorkRbDescError
    }, types::{Key, Sge}, utils::Buffer, work_poller::WorkDescPollerContext, RoundRobinStrategy,
};

#[derive(Debug, Clone, Default)]
struct MockProxy(Arc<(AtomicU8, AtomicU8)>);

type ToHostWorkRb = Ringbuf<MockProxy, 128, 32, 4096>;

impl CsrReaderProxy for MockProxy {
    fn write_tail(&self, data: u32) -> Result<(), DeviceError> {
        self.0
             .0
            .store(data as u8, std::sync::atomic::Ordering::Release);
        Ok(())
    }
    fn read_head(&self) -> Result<u32, DeviceError> {
        let val = self.0 .1.load(std::sync::atomic::Ordering::Acquire);
        Ok(val as u32)
    }
}

impl MockProxy {
    fn write_head(&self, data: u32) -> Result<(), DeviceError> {
        self.0
             .1
            .store(data as u8, std::sync::atomic::Ordering::Release);
        Ok(())
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

/// benchmark receive packet
#[allow(clippy::unwrap_used)]
pub fn benchmark_recv_pkt() {
    // create huge page ring buf
    let to_host_work_rb_buffer = Buffer::new(4096, true).unwrap();
    let proxy = MockProxy::default();
    let to_host_work_rb = Mutex::new(ToHostWorkRb::new(proxy.clone(), to_host_work_rb_buffer));
    // prefill the memory, so that reading will not failed
    let (checker_sender, _checker_recver) = unbounded();
    let (nic_sender, _nic_recver) = unbounded();
    let recv_ctx = WorkDescPollerContext {
        work_rb: Arc::new(to_host_work_rb),
        checker_channel: checker_sender,
        nic_channel: nic_sender,
    };
    proxy.write_head(127).unwrap();
    let start = std::time::Instant::now();
    for i in 0_i32..127_i32 {
        recv_ctx.recv_a_desc().unwrap();
    }
    let elapsed = start.elapsed();
    log::info!("recv 256 packets elapsed: {:?}", elapsed);
}

#[derive(Debug, Clone, Default)]
struct MockWriteProxy(Arc<AtomicU32>);

type ToCardWorkRb = Ringbuf<MockWriteProxy, 128, 32, 4096>;
impl CsrWriterProxy for MockWriteProxy {
    fn write_head(&self, data: u32) -> Result<(), DeviceError> {
        Ok(())
    }

    fn read_tail(&self) -> Result<u32, DeviceError> {
        Ok(0)
    }
}

/// get a descriptor of specific size
pub fn get_descriptor(size: u32) -> Box<ToCardWorkRbDesc> {
    let common = ToCardWorkRbDescCommon {
        total_len: size,
        ..Default::default()
    };
    let sge = Sge {
        len: size,
        addr: 0,
        key: Key::default(),
    };
    ToCardWorkRbDescBuilder::new(ToCardWorkRbDescOpcode::Write)
        .with_common(common)
        .with_sge(sge)
        .build()
        .unwrap()
}

/// benchmark scheduler
pub fn benchmark_scheduler(descriptor_size : u32,scheduler_size:usize,batch_size:usize) {
    let strategy = RoundRobinStrategy::new();
    let buffer = Buffer::new(4096, true).unwrap();
    let ringbuf = Mutex::new(ToCardWorkRb::new(MockWriteProxy::default(), buffer));

    // warm up
    let descs_num = descriptor_size as usize / scheduler_size;
    let mut data = vec![get_descriptor(descriptor_size).clone();batch_size];
    for i in 0..descs_num{
        let desc = data.pop();
        if let Some(desc) = desc {
            push_to_scheduler(desc, &strategy, scheduler_size);
        }
        pop_and_write_to_ringbuf(&strategy, &ringbuf);
    }

    // round 1
    let mut data = vec![get_descriptor(descriptor_size).clone();batch_size];
    let start = Instant::now();
    for i in 0..descs_num{
        let desc = data.pop();
        if let Some(desc) = desc {
            push_to_scheduler(desc, &strategy, scheduler_size);
        }
        pop_and_write_to_ringbuf(&strategy, &ringbuf);
    }
    let time = start.elapsed().as_micros();
    let per_time = time / batch_size as u128;
    eprintln!("descriptor_size={},scheduler_size={},batch_size={},time={},pertime={}",descriptor_size,scheduler_size,batch_size,time,per_time);
}
