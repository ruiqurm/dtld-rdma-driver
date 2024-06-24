use std::{
    fs::{File, OpenOptions}, io, mem::size_of, os::fd::AsRawFd, path::Path,slice::from_raw_parts_mut, sync::Arc
};

use log::error;
use parking_lot::Mutex;

use crate::device::{
    constants::{
        CSR_ADDR_CMD_REQ_QUEUE_HEAD, CSR_ADDR_CMD_REQ_QUEUE_TAIL, CSR_ADDR_CMD_RESP_QUEUE_HEAD,
        CSR_ADDR_CMD_RESP_QUEUE_TAIL, CSR_ADDR_META_REPORT_QUEUE_HEAD,
        CSR_ADDR_META_REPORT_QUEUE_TAIL, CSR_ADDR_SEND_QUEUE_HEAD, CSR_ADDR_SEND_QUEUE_TAIL,
    },
    ringbuf::{CsrReaderProxy, CsrWriterProxy},
    DeviceError,
};

const RDMA_IOCTL_CMD: u64 = 0xc018_1b01;
const CSR_LENGTH: usize = 0x0010_0000;
const CSR_WORD_LENGTH: usize = CSR_LENGTH / 4;

#[repr(C)]
#[derive(Debug)]
struct RDMASyscallReq {
    length: u16,
    object_id: u16,
    method_id: u16,
    num_attrs: u16,
    reserved1: u64,
    driver_id: u32,
    reserved2: u32,
}

#[repr(C)]
#[derive(Debug)]
struct RDMASyscallReqAttr {
    attr_id: u16,
    len: u16,
    flags: u16,
    reserved: u16,
    data: u64,
}

#[repr(C)]
#[derive(Debug)]
struct RDMAGetUContextSyscallReq {
    req: RDMASyscallReq,
    attr: RDMASyscallReqAttr,
}

impl RDMAGetUContextSyscallReq {
    #[allow(clippy::arithmetic_side_effects, clippy::cast_possible_truncation)]
    pub(crate) fn new_get_context(resp: &RDMAGetUContextSyscallResp) -> Self {
        let req = RDMASyscallReq {
            length: (size_of::<RDMASyscallReq>() + size_of::<RDMASyscallReqAttr>()) as u16, // 40, this will never overflow
            object_id: 0,
            method_id: 3, // get context
            num_attrs: 1,
            reserved1: 0,
            driver_id: 0, // RDMA Unknown
            reserved2: 0,
        };
        let attr = RDMASyscallReqAttr {
            attr_id: 4097, // out ptr attr
            len: size_of::<RDMAGetUContextSyscallResp>() as u16,
            flags: 1,
            reserved: 0,
            data: resp as *const _ as u64
        };
        Self { req, attr }
    }
}

#[repr(C)]
#[derive(Debug,Default)]
struct RDMAGetUContextSyscallResp {
    pub(crate) csr: i64,
    pub(crate) cmdq_sq: i64,
    pub(crate) cmdq_rq: i64,
    pub(crate) workq_sq: i64,
    pub(crate) workq_rq: i64,
}

#[derive(Debug, Clone)]
pub(crate) struct CsrClient(Arc<CsrClientInner>);

#[derive(Debug)]
struct CsrClientInner {
    _device_file: File,
    mapping: Mutex<&'static mut [u32]>,
}

impl CsrClient {
    pub(crate) fn new<P: AsRef<Path>>(device_path: P) -> io::Result<Self> {
        let resp = RDMAGetUContextSyscallResp::default();
        let mut req = RDMAGetUContextSyscallReq::new_get_context(&resp);
        let device_file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(device_path)?;
        let device_file_fd = device_file.as_raw_fd();
        let ret_val = unsafe {
            libc::ioctl(
                device_file_fd,
                RDMA_IOCTL_CMD,
                std::ptr::addr_of!(req) as *mut u8,
            )
        };

        if ret_val != 0_i32 {
            return Err(io::Error::last_os_error());
        }
        let offset = resp.csr;
        log::info!("{:?}",resp);

        let mapping = unsafe {
            libc::mmap(
                core::ptr::null_mut(),
                CSR_LENGTH,
                libc::PROT_WRITE,
                libc::MAP_SHARED,
                device_file_fd,
                offset,
            )
        };

        if mapping == libc::MAP_FAILED {
            error!("map failed\n");
            return Err(io::Error::last_os_error());
        }

        let mapping =
            Mutex::new(unsafe { from_raw_parts_mut(mapping.cast::<u32>(), CSR_WORD_LENGTH) });

        Ok(Self(Arc::new(CsrClientInner {
            _device_file: device_file,
            mapping,
        })))
    }

    pub(crate) fn read_csr(&self, addr: usize) -> Result<u32, DeviceError> {
        if addr >= CSR_LENGTH {
            return Err(DeviceError::Device(format!("csr overflow :{addr}")));
        }
        let buf = self
            .0
            .mapping
            .lock();
        #[allow(clippy::arithmetic_side_effects)]
        let offset = addr / size_of::<u32>();
        #[allow(clippy::indexing_slicing)] // we have checked it before
        Ok(buf[offset])
    }

    pub(crate) fn write_csr(&self, addr: usize, data: u32) -> Result<(), DeviceError> {
        if addr >= CSR_LENGTH {
            return Err(DeviceError::Device(format!("csr overflow :{addr}")));
        }
        let mut buf = self
            .0
            .mapping
            .lock();
        #[allow(clippy::arithmetic_side_effects)]
        let offset = addr / size_of::<u32>();
        #[allow(clippy::indexing_slicing)] // we have checked it before
        {
            buf[offset] = data;
        }
        Ok(())
    }
}

#[derive(Debug)]
pub(crate) struct ToCardCtrlRbCsrProxy(CsrClient);
impl ToCardCtrlRbCsrProxy {
    const HEAD_CSR: usize = CSR_ADDR_CMD_REQ_QUEUE_HEAD;
    const TAIL_CSR: usize = CSR_ADDR_CMD_REQ_QUEUE_TAIL;
    pub(crate) fn new(client: CsrClient) -> Self {
        Self(client)
    }
}
impl CsrWriterProxy for ToCardCtrlRbCsrProxy {
    fn write_head(&self, data: u32) -> Result<(), DeviceError> {
        self.0.write_csr(Self::HEAD_CSR, data)
    }
    fn read_tail(&self) -> Result<u32, DeviceError> {
        self.0.read_csr(Self::TAIL_CSR)
    }
}

#[derive(Debug)]
pub(crate) struct ToHostCtrlRbCsrProxy(CsrClient);

impl ToHostCtrlRbCsrProxy {
    const HEAD_CSR: usize = CSR_ADDR_CMD_RESP_QUEUE_HEAD;
    const TAIL_CSR: usize = CSR_ADDR_CMD_RESP_QUEUE_TAIL;
    pub(crate) fn new(client: CsrClient) -> Self {
        Self(client)
    }
}

impl CsrReaderProxy for ToHostCtrlRbCsrProxy {
    fn write_tail(&self, data: u32) -> Result<(), DeviceError> {
        self.0.write_csr(Self::TAIL_CSR, data)
    }
    fn read_head(&self) -> Result<u32, DeviceError> {
        self.0.read_csr(Self::HEAD_CSR)
    }
}

#[derive(Debug)]
pub(crate) struct ToCardWorkRbCsrProxy(CsrClient);

impl ToCardWorkRbCsrProxy {
    const HEAD_CSR: usize = CSR_ADDR_SEND_QUEUE_HEAD;
    const TAIL_CSR: usize = CSR_ADDR_SEND_QUEUE_TAIL;
    pub(crate) fn new(client: CsrClient) -> Self {
        Self(client)
    }
}

impl CsrWriterProxy for ToCardWorkRbCsrProxy {
    fn write_head(&self, data: u32) -> Result<(), DeviceError> {
        self.0.write_csr(Self::HEAD_CSR, data)
    }
    fn read_tail(&self) -> Result<u32, DeviceError> {
        self.0.read_csr(Self::TAIL_CSR)
    }
}

#[derive(Debug)]
pub(crate) struct ToHostWorkRbCsrProxy(CsrClient);

impl ToHostWorkRbCsrProxy {
    const HEAD_CSR: usize = CSR_ADDR_META_REPORT_QUEUE_HEAD;
    const TAIL_CSR: usize = CSR_ADDR_META_REPORT_QUEUE_TAIL;
    pub(crate) fn new(client: CsrClient) -> Self {
        Self(client)
    }
}

impl CsrReaderProxy for ToHostWorkRbCsrProxy {
    fn write_tail(&self, data: u32) -> Result<(), DeviceError> {
        self.0.write_csr(Self::TAIL_CSR, data)
    }
    fn read_head(&self) -> Result<u32, DeviceError> {
        self.0.read_csr(Self::HEAD_CSR)
    }
}
