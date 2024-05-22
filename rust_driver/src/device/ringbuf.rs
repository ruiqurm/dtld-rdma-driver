use std::sync::{Mutex, MutexGuard};

use crate::utils::Buffer;

use super::DeviceError;

pub(super) trait CsrWriterProxy {
    fn write_head(&self, data: u32) -> Result<(), DeviceError>;
    fn read_tail(&self) -> Result<u32, DeviceError>;
}

pub(super) trait CsrReaderProxy {
    fn write_tail(&self, data: u32) -> Result<(), DeviceError>;
    fn read_head(&self) -> Result<u32, DeviceError>;
}

/// The Ringbuf is a circular buffer used comunicate between the host and the card.
#[derive(Debug)]
pub(super) struct Ringbuf<T, const DEPTH: usize, const ELEM_SIZE: usize, const PAGE_SIZE: usize> {
    buf: Mutex<Buffer>,
    head: usize,
    tail: usize,
    proxy: T,
}

pub(super) struct RingbufWriter<
    'a,
    'proxy,
    T: CsrWriterProxy,
    const DEPTH: usize,
    const ELEM_SIZE: usize,
    const PAGE_SIZE: usize,
> {
    buf: MutexGuard<'a, Buffer>,
    head: &'a mut usize,
    tail: &'a mut usize,
    written_cnt: usize,
    proxy: &'proxy T,
}

pub(super) struct RingbufReader<
    'a,
    'proxy,
    T: CsrReaderProxy,
    const DEPTH: usize,
    const ELEM_SIZE: usize,
    const PAGE_SIZE: usize,
> {
    buf: MutexGuard<'a, Buffer>,
    head: &'a mut usize,
    tail: &'a mut usize,
    read_cnt: usize,
    proxy: &'proxy T,
}

const fn _is_power_of_2(v: usize) -> bool {
    (v & (v.wrapping_sub(1))) == 0
}

impl<T, const DEPTH: usize, const ELEM_SIZE: usize, const PAGE_SIZE: usize>
    Ringbuf<T, DEPTH, ELEM_SIZE, PAGE_SIZE>
{   
    // We use the highest bit as the guard bit. See the method `is_full` for detail
    const PTR_IDX_MASK: usize = DEPTH*2 - 1;
    const PTR_IDX_GUARD_MASK: usize = DEPTH;
    const PTR_IDX_VALID_MASK: usize = DEPTH - 1;

    const _IS_DEPTH_POWER_OF_2: () = assert!(_is_power_of_2(DEPTH), "invalid ringbuf depth");
    const _IS_ELEM_SIZE_POWER_OF_2: () = assert!(_is_power_of_2(ELEM_SIZE), "invalid element size");
    const _IS_RINGBUF_SIZE_VALID: () =
        assert!(DEPTH * ELEM_SIZE >= PAGE_SIZE, "invalid ringbuf size");

    /// Return (ringbuf, ringbuf virtual memory address)
    ///
    #[allow(clippy::indexing_slicing,clippy::arithmetic_side_effects,clippy::unwrap_used)] // we have allocate additional space in advance to avoid overflow
    pub(super) fn new(proxy: T, buffer : Buffer) -> Self {
        assert!(buffer.as_ptr() as usize % PAGE_SIZE == 0,"buffer should be aligned to PAGE_SIZE");
        Self {
            buf : Mutex::new(buffer),
            head: 0,
            tail: 0,
            proxy,
        }
    }

    #[allow(clippy::arithmetic_side_effects)]
    pub(crate) fn is_full(head: usize, tail: usize) -> bool {
        // Since the highest bit stands for two times of the DEPTH in bineary, if the head and tail have different highest bit and the rest bits are the same, 
        // it means the ringbuf is full. 
        // In hardware we use like `(head.idx == tail.idx) && (head.guard != tail.guard)`
        let head_guard = head & Self::PTR_IDX_GUARD_MASK;
        let tail_guard = tail & Self::PTR_IDX_GUARD_MASK;
        let head_low = head & Self::PTR_IDX_VALID_MASK;
        let tail_low = tail & Self::PTR_IDX_VALID_MASK;
        (head_guard != tail_guard) && (head_low == tail_low)
    }

    pub(crate) fn is_empty(head: usize, tail: usize) -> bool {
        head == tail
    }

    #[allow(clippy::arithmetic_side_effects)]
    pub(crate) fn wrapping_add(cur: usize, cnt: usize) -> usize {
        (cur + cnt) & Self::PTR_IDX_MASK
    }
}

impl<T: CsrWriterProxy, const DEPTH: usize, const ELEM_SIZE: usize, const PAGE_SIZE: usize>
    Ringbuf<T, DEPTH, ELEM_SIZE, PAGE_SIZE>
{
    /// Get space for writing `desc_cnt` descriptors to the ring buffer.
    pub(super) fn write(
        &mut self,
    ) -> Result<RingbufWriter<'_, '_, T, DEPTH, ELEM_SIZE, PAGE_SIZE>, DeviceError> {
        Ok(RingbufWriter {
            buf: self
                .buf
                .lock()
                .map_err(|_| DeviceError::LockPoisoned("read_op_ctx_map lock".to_owned()))?,
            head: &mut self.head,
            tail: &mut self.tail,
            written_cnt: 0,
            proxy: &self.proxy,
        })
    }
}

impl<T: CsrReaderProxy, const DEPTH: usize, const ELEM_SIZE: usize, const PAGE_SIZE: usize>
    Ringbuf<T, DEPTH, ELEM_SIZE, PAGE_SIZE>
{
    /// Prepare to read some descriptors from the ring buffer.
    pub(super) fn read(
        &mut self,
    ) -> Result<RingbufReader<'_, '_, T, DEPTH, ELEM_SIZE, PAGE_SIZE>, DeviceError> {
        Ok(RingbufReader {
            buf: self
                .buf
                .lock()
                .map_err(|_| DeviceError::LockPoisoned("buf lock".to_owned()))?,
            head: &mut self.head,
            tail: &mut self.tail,
            read_cnt: 0,
            proxy: &self.proxy,
        })
    }
}

impl<T: CsrWriterProxy, const DEPTH: usize, const ELEM_SIZE: usize, const PAGE_SIZE: usize>
    RingbufWriter<'_, '_, T, DEPTH, ELEM_SIZE, PAGE_SIZE>
{
    fn advance(&mut self) -> Result<(), DeviceError> {
        let head = *self.head;
        let new_head =
            Ringbuf::<T, DEPTH, ELEM_SIZE, PAGE_SIZE>::wrapping_add(head, self.written_cnt);
        *self.head = new_head;
        // since the head is got by wrapping_add, it's safe to cast to u32
        #[allow(clippy::cast_possible_truncation)]
        self.proxy.write_head(new_head as u32)?;
        self.written_cnt = 0;
        Ok(())
    }
}

impl<'a, T: CsrWriterProxy, const DEPTH: usize, const ELEM_SIZE: usize, const PAGE_SIZE: usize>
    Iterator for RingbufWriter<'a, '_, T, DEPTH, ELEM_SIZE, PAGE_SIZE>
{
    type Item = &'a mut [u8];

    #[allow(clippy::arithmetic_side_effects)] // We add comments on every arithmetic operation
    fn next(&mut self) -> Option<Self::Item> {
        let idx = (*self.head + self.written_cnt) 
            & Ringbuf::<T, DEPTH, ELEM_SIZE, PAGE_SIZE>::PTR_IDX_VALID_MASK; // head < DEPTH and written_cnt < DEPTH, so the result is < 2 * DEPTH

        // currently, we not allow the writer to overflow
        // Instead, we wait and polling.
        if Ringbuf::<T, DEPTH, ELEM_SIZE, PAGE_SIZE>::is_full(idx, *self.tail) {
            // write back first
            let _: Result<(), DeviceError> = self.advance();
            loop {
                let new_tail = self.proxy.read_tail();
                match new_tail {
                    Ok(new_tail) => {
                        if !Ringbuf::<T, DEPTH, ELEM_SIZE, PAGE_SIZE>::is_full(
                            idx,
                            new_tail as usize,
                        ) {
                            *self.tail = new_tail as usize;
                            break;
                        }
                    }
                    Err(e) => {
                        log::error!("failed to read tail pointer: {:?}", e);
                        return None;
                    }
                }
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
        }
        let offset = idx * ELEM_SIZE; // ELEM_SIZE=32Byte, so offset < DEPTH * ELEM_SIZE, which will never flow
        let ptr = unsafe { self.buf.as_mut_ptr().add(offset) };

        self.written_cnt += 1; // written_cnt < DEPTH

        Some(unsafe { std::slice::from_raw_parts_mut(ptr, ELEM_SIZE) })
    }
}

/// Drop the writer to update the head pointer.
impl<T: CsrWriterProxy, const DEPTH: usize, const ELEM_SIZE: usize, const PAGE_SIZE: usize> Drop
    for RingbufWriter<'_, '_, T, DEPTH, ELEM_SIZE, PAGE_SIZE>
{
    fn drop(&mut self) {
        let _: Result<(), DeviceError> = self.advance();
    }
}

impl<'a, T: CsrReaderProxy, const DEPTH: usize, const ELEM_SIZE: usize, const PAGE_SIZE: usize>
    Iterator for RingbufReader<'a, '_, T, DEPTH, ELEM_SIZE, PAGE_SIZE>
{
    type Item = &'a [u8];

    #[allow(clippy::arithmetic_side_effects)]
    fn next(&mut self) -> Option<Self::Item> {
        let idx =
            (*self.tail + self.read_cnt) & Ringbuf::<T, DEPTH, ELEM_SIZE, PAGE_SIZE>::PTR_IDX_VALID_MASK;
        if Ringbuf::<T, DEPTH, ELEM_SIZE, PAGE_SIZE>::is_empty(*self.head, idx) {
            loop {
                let new_head = self.proxy.read_head();
                match new_head {
                    Ok(new_head) => {
                        if !Ringbuf::<T, DEPTH, ELEM_SIZE, PAGE_SIZE>::is_empty(
                            new_head as usize,
                            idx,
                        ) {
                            *self.head = new_head as usize;
                            break;
                        }
                    }
                    Err(e) => {
                        log::error!("failed to read head pointer: {:?}", e);
                        return None;
                    }
                }
            }
        }
        let offset = idx * ELEM_SIZE;
        let ptr = unsafe { self.buf.as_ptr().add(offset) };

        self.read_cnt += 1;

        Some(unsafe { std::slice::from_raw_parts(ptr, ELEM_SIZE) })
    }
}

/// Drop the reader to update the tail pointer.
impl<T: CsrReaderProxy, const DEPTH: usize, const ELEM_SIZE: usize, const PAGE_SIZE: usize> Drop
    for RingbufReader<'_, '_, T, DEPTH, ELEM_SIZE, PAGE_SIZE>
{
    fn drop(&mut self) {
        *self.tail = Ringbuf::<T, DEPTH, ELEM_SIZE, PAGE_SIZE>::wrapping_add(*self.tail, self.read_cnt);
        let tail = u32::try_from(*self.tail).unwrap_or_else(|_| {
            log::error!("In read ringbuf, failed to convert from usize to u32");
            0
        });
        if let Err(e) = self.proxy.write_tail(tail) {
            log::error!("failed to write tail pointer: {:?}", e);
        }
    }
}

#[cfg(test)]
mod test {
    use std::{
        sync::{
            atomic::{AtomicBool, AtomicU32, Ordering},
            Arc,
        },
        thread::{sleep, spawn},
    };

    use crate::{device::DeviceError, utils::Buffer};

    use super::Ringbuf;

    #[derive(Debug, Clone)]
    struct Proxy(Arc<ProxyInner>);

    #[derive(Debug)]
    struct ProxyInner {
        head: AtomicU32,
        tail: AtomicU32,
    }
    impl Proxy {
        pub(crate) fn consume(&self) {
            // move the tail to the head
            let head = self.0.head.load(Ordering::Acquire);
            self.0.tail.store(head, Ordering::Release);
        }

        pub(crate) fn produce<const DEPTH: usize>(&self, cnt: usize) {
            // move the head to the tail
            let head = self.0.head.load(Ordering::Acquire);
            let tail = self.0.tail.load(Ordering::Acquire);
            if head == tail { // push when empty
                let new_head = (head + cnt as u32) % (DEPTH*2) as u32;
                self.0.head.store(new_head, Ordering::Release);
            }
        }
    }
    impl super::CsrWriterProxy for Proxy {
        fn write_head(&self, data: u32) -> Result<(), DeviceError> {
            self.0.head.store(data, Ordering::Release);
            Ok(())
        }
        fn read_tail(&self) -> Result<u32, DeviceError> {
            Ok(self.0.tail.load(Ordering::Acquire))
        }
    }
    impl super::CsrReaderProxy for Proxy {
        fn write_tail(&self, data: u32) -> Result<(), DeviceError> {
            self.0.tail.store(data, Ordering::Release);
            Ok(())
        }
        fn read_head(&self) -> Result<u32, DeviceError> {
            Ok(self.0.head.load(Ordering::Acquire))
        }
    }
    #[test]
    fn test_ringbuf_writer() {
        let proxy = Proxy(Arc::new(ProxyInner {
            head: AtomicU32::new(0),
            tail: AtomicU32::new(0),
        }));
        let thread_proxy = proxy.clone();
        let _ = spawn(move || loop {
            sleep(std::time::Duration::from_millis(10));
            thread_proxy.consume();
        });
        let buffer = Buffer::new(4096, true).unwrap();
        let mut ringbuf = Ringbuf::<Proxy, 128, 32, 4096>::new(proxy.clone(),buffer);
        let mut writer = ringbuf.write().unwrap();

        for i in 0..128 {
            let desc = writer.next().unwrap();
            desc.fill(i);
        }
        drop(writer);
        assert!(proxy.0.head.load(Ordering::Relaxed) == 128);
        assert!(proxy.0.tail.load(Ordering::Relaxed) == 0);
        sleep(std::time::Duration::from_millis(20));
        assert!(proxy.0.head.load(Ordering::Relaxed) == 128);
        assert!(proxy.0.tail.load(Ordering::Relaxed) == 128);
        // test if blocking?

        let mut writer = ringbuf.write().unwrap();
        for i in 0..=256 {
            let desc = writer.next().unwrap();
            desc.fill(i as u8);
        }
        drop(writer);
    }

    #[test]
    fn test_ringbuf_reader() {
        let proxy = Proxy(Arc::new(ProxyInner {
            head: AtomicU32::new(0),
            tail: AtomicU32::new(0),
        }));
        let thread_proxy = proxy.clone();
        let _ = spawn(move || loop {
            thread_proxy.produce::<128>(128);
            sleep(std::time::Duration::from_millis(10));
        });
        let buffer = Buffer::new(4096, true).unwrap();
        let mut ringbuf = Ringbuf::<Proxy, 128, 32, 4096>::new(proxy.clone(),buffer);
        let mut reader = ringbuf.read().unwrap();
        sleep(std::time::Duration::from_millis(100));
        for _i in 0..128 {
            let _desc = reader.next().unwrap();
        }
        drop(reader);
        assert!(proxy.0.tail.load(Ordering::Relaxed) == 128);

        let mut reader = ringbuf.read().unwrap();

        let finish_flag = Arc::new(AtomicBool::new(false));
        let finish_flag_clone = Arc::<AtomicBool>::clone(&finish_flag);
        let checker = spawn(move || {
            sleep(std::time::Duration::from_millis(60));
            if finish_flag_clone.load(Ordering::Relaxed) {
                panic!("should not block at here");
            }
        });
        for _i in 0..130 {
            let _desc = reader.next().unwrap();
        }
        drop(reader);
        finish_flag.store(true, Ordering::Relaxed);
        checker.join().unwrap();
    }
}
