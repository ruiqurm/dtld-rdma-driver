use buddy_system_allocator::LockedHeap;

use eui48::MacAddress;
use open_rdma_driver::{
    qp::QpManager,
    types::{MemAccessTypeFlag, Pmtu, Qp, QpType, Qpn, RdmaDeviceNetwork, PAGE_SIZE},
    Device, Mr, Pd, Sge,
};
use std::slice::from_raw_parts_mut;
use std::{ffi::c_void, net::Ipv4Addr};
use log::{info, Level, LevelFilter, Metadata, Record, SetLoggerError};

const ORDER: usize = 32;
const SHM_PATH: &str = "/bluesim1\0";

#[macro_use]
extern crate ctor;

/// Use `LockedHeap` as global allocator
#[global_allocator]
static HEAP_ALLOCATOR: LockedHeap<ORDER> = LockedHeap::<ORDER>::new();
const HEAP_BLOCK_SIZE: usize = 1024 * 1024 * 64;
const BUFFER_LENGTH : usize = 1024 * 128;
const SEND_CNT: usize = 1024*64;
static mut HEAP_START_ADDR: usize = 0;

#[ctor]
fn init_global_allocator() {
    unsafe {
        let shm_fd = libc::shm_open(
            SHM_PATH.as_ptr() as *const libc::c_char,
            libc::O_RDWR,
            0o600,
        );

        let heap = libc::mmap(
            0x7f7e8e600000 as *mut c_void,
            HEAP_BLOCK_SIZE,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_SHARED,
            shm_fd,
            0,
        );

        // let align_addr = (heap as usize + PAGE_SIZE - 1) & !(PAGE_SIZE - 1);

        // let padding = align_addr - heap as usize;
        let addr = heap as usize;
        let size = HEAP_BLOCK_SIZE;
        HEAP_START_ADDR = addr;

        HEAP_ALLOCATOR.lock().init(addr, size);
    }
}

fn get_phys_addr(addr: usize) -> usize {
    addr - unsafe { HEAP_START_ADDR }
}

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
    log::set_boxed_logger(Box::new(SimpleLogger))
        .map(|()| log::set_max_level(LevelFilter::Debug))
}

#[allow(clippy::slow_vector_initialization)]
fn allocate_aligned_buf(size: usize) -> Box<[u8]> {
    let mut vec = Vec::with_capacity(size + PAGE_SIZE);
    vec.resize(size + PAGE_SIZE, 0u8);
    let buffer = Box::leak(vec.into_boxed_slice());
    let buffer_padding = get_phys_addr(buffer.as_ptr() as usize) & (PAGE_SIZE - 1);
    unsafe {
        Box::from_raw(from_raw_parts_mut(
            &buffer[buffer_padding] as *const _ as *mut u8,
            size,
        ))
    }
}

fn create_and_init_card(
    card_id: usize,
    mock_server_addr: &str,
    qpn: Qpn,
    local_network: &RdmaDeviceNetwork,
    remote_network: &RdmaDeviceNetwork,
) -> (Device, Pd, Mr, Box<[u8]>) {
    let head_start_addr = unsafe { HEAP_START_ADDR };
    let dev = Device::new_emulated(
        mock_server_addr.parse().unwrap(),
        head_start_addr,
        local_network,
    )
    .unwrap();
    info!("[{}] Device created", card_id);

    let pd = dev.alloc_pd().unwrap();
    info!("[{}] PD allocated", card_id);

    let mut mr_buffer = allocate_aligned_buf(BUFFER_LENGTH);

    unsafe {
        println!(
            "[{}] MR's PA_START={:X}",
            card_id,
            mr_buffer.as_mut_ptr() as usize - HEAP_START_ADDR
        );
    }

    let access_flag = MemAccessTypeFlag::IbvAccessRemoteRead
        | MemAccessTypeFlag::IbvAccessRemoteWrite
        | MemAccessTypeFlag::IbvAccessLocalWrite;
    let mr = dev
        .reg_mr(
            pd,
            mr_buffer.as_mut_ptr() as u64,
            mr_buffer.len() as u32,
            PAGE_SIZE as u32,
            access_flag,
        )
        .unwrap();
    info!("[{}] MR registered", card_id);
    let qp = Qp::new(
        pd,
        qpn,
        QpType::Rc,
        access_flag,
        Pmtu::Mtu4096,
        remote_network.ipaddr,
        remote_network.macaddr,
    );
    dev.create_qp(&qp).unwrap();
    info!("[{}] QP created", card_id);

    (dev, pd, mr, mr_buffer)
}
fn main() {
    init_logging().unwrap();
    let qp_manager = QpManager::new();
    let qpn = qp_manager.alloc().unwrap();
    let a_network = RdmaDeviceNetwork::new(
        Ipv4Addr::new(192, 168, 0, 0x1),
        Ipv4Addr::new(255, 255, 255, 0),
        Ipv4Addr::new(192, 168, 0, 2),
        MacAddress::new([0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFE]),
    );
    let b_network = RdmaDeviceNetwork::new(
        Ipv4Addr::new(192, 168, 0, 0x1),
        Ipv4Addr::new(255, 255, 255, 0),
        Ipv4Addr::new(192, 168, 0, 3),
        MacAddress::new([0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF]),
    );
    let (dev_a, _pd_a, mr_a, mut mr_buffer_a) =
        create_and_init_card(0, "0.0.0.0:9873", qpn, &a_network, &b_network);
    let (_dev_b, _pd_b, mr_b, mut mr_buffer_b) =
        create_and_init_card(1, "0.0.0.0:9875", qpn, &b_network, &a_network);
    let dpqn = qpn;
    for (idx, item) in mr_buffer_a.iter_mut().enumerate() {
        *item = idx as u8;
    }
    for item in mr_buffer_b[0..].iter_mut() {
        *item = 0
    }

    let sge0 = Sge::new(
        &mr_buffer_a[0] as *const u8 as u64,
        SEND_CNT.try_into().unwrap(),
        mr_a.get_key(),
    );

    // let sge1 = Sge::new(
    //     &mr_buffer_a[1024 * 8] as *const u8 as u64,
    //     1024 * 8,
    //     mr_a.get_key(),
    // );

    // let sge2 = Sge {
    //     addr: &mr_buffer_a[2] as *const u8 as u64,
    //     len: 1,
    //     key: mr_a.get_key(),
    // };

    // let sge3 = Sge {
    //     addr: &mr_buffer_a[3] as *const u8 as u64,
    //     // len: 32767 - 3,
    //     len: SEND_CNT as u32 - 3,
    //     key: mr_a.get_key(),
    // };
    let ctx1 = dev_a
        .write(
            &dpqn,
            &mr_buffer_b[0] as *const u8 as u64,
            mr_b.get_key(),
            MemAccessTypeFlag::empty(),
            sge0,
            None,
            None,
            None,
        )
        .unwrap();

    // let ctx2 = dev_a
    //     .write(
    //         &dpqn,
    //         &mr_buffer_b[1024 * 8] as *const u8 as u64,
    //         mr_b.get_key(),
    //         MemAccessTypeFlag::IbvAccessRemoteRead
    //             | MemAccessTypeFlag::IbvAccessRemoteWrite
    //             | MemAccessTypeFlag::IbvAccessLocalWrite,
    //         sge1,
    //         None,
    //         None,
    //         None,
    //     )
    //     .unwrap();
    ctx1.wait();
    // ctx2.wait();
    assert_eq!(mr_buffer_a[0..SEND_CNT], mr_buffer_b[0..SEND_CNT]);

    // for item in mr_buffer_a.iter_mut() {
    //     *item = 0;
    // }
    // for (idx, item) in mr_buffer_a.iter_mut().enumerate() {
    //     *item = idx as u8;
    // }

    // // we read from b to a

    // let sge_read = Sge {
    //     addr: &mr_buffer_a[0] as *const u8 as u64,
    //     len: SEND_CNT as u32,
    //     key: mr_a.get_key(),
    // };

    // // // read text from b to a.
    // let ctx1 = dev_a
    //     .read(
    //         dpqn,
    //         &mr_buffer_b[1024] as *const u8 as u64,
    //         mr_b.get_key(),
    //         MemAccessTypeFlag::IbvAccessNoFlags,
    //         sge_read,
    //     )
    //     .unwrap();
    // info!("Read req sent");

    // // assert!(mr_buffer_a[0..SEND_CNT] == mr_buffer_b[1024..1024 + SEND_CNT]);

    // for item in mr_buffer_a.iter_mut() {
    //     *item = 0;
    // }
    // for (idx, item) in mr_buffer_a.iter_mut().enumerate() {
    //     *item = idx as u8;
    // }

    // let sge_read = Sge {
    //     addr: &mr_buffer_a[0] as *const u8 as u64,
    //     len: SEND_CNT as u32,
    //     key: mr_a.get_key(),
    // };

    // // read text from b to a.
    // let ctx2 = dev_a
    //     .read(
    //         dpqn,
    //         &mr_buffer_b[1024] as *const u8 as u64,
    //         mr_b.get_key(),
    //         MemAccessTypeFlag::IbvAccessNoFlags,
    //         sge_read,
    //     )
    //     .unwrap();
    // ctx1.wait();
    // ctx2.wait();

    info!("Read req sent");
    // dev_a.dereg_mr(mr_a).unwrap();
    // dev_b.dereg_mr(mr_b).unwrap();
    // info!("MR deregistered");
}
