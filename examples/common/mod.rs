use std::{alloc::{alloc, dealloc, Layout}, ops::{Deref, DerefMut}, slice::from_raw_parts_mut};

use log::{Level, LevelFilter, Metadata, Record, SetLoggerError};
use open_rdma_driver::types::PAGE_SIZE;

struct SimpleLogger;

impl log::Log for SimpleLogger {
    fn enabled(&self, metadata: &Metadata) -> bool {
        metadata.level() <= Level::Debug
    }

    fn log(&self, record: &Record) {
        if self.enabled(record.metadata()) {
            println!("{} - {}", record.level(), record.args());
        }
    }

    fn flush(&self) {}
}

pub fn init_logging() -> Result<(), SetLoggerError> {
    log::set_boxed_logger(Box::new(SimpleLogger)).map(|()| log::set_max_level(LevelFilter::Debug))
}

fn allocate_aligned_memory(size: usize) -> &'static mut [u8] {
    let layout = Layout::from_size_align(size, PAGE_SIZE).unwrap();
    let ptr = unsafe { alloc(layout) };
    unsafe { from_raw_parts_mut(ptr, size) }
}

fn deallocate_aligned_memory(buf: &mut [u8], size: usize) {
    let layout = Layout::from_size_align(size, PAGE_SIZE).unwrap();
    unsafe {
        dealloc(buf.as_mut_ptr(), layout);
    }
}

pub struct AlignedMemory<'a>(&'a mut [u8]);

impl<'a> AlignedMemory<'a> {
    pub fn new(size: usize) -> Self {
        AlignedMemory(allocate_aligned_memory(size))
    }
}

impl<'a> Drop for AlignedMemory<'a> {
    fn drop(&mut self) {
        deallocate_aligned_memory(self.0, self.0.len());
    }
}

impl<'a> Deref for AlignedMemory<'a>{
    type Target = [u8];
    
    fn deref(&self) -> &Self::Target {
        self.0
    }
}

impl<'a> DerefMut for AlignedMemory<'a>{
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.0
    }    
}