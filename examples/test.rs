use buddy_system_allocator::LockedHeap;

use eui48::MacAddress;
use log::info;
use open_rdma_driver::{
    qp::QpManager,
    types::{
        MemAccessTypeFlag, Pmtu, QpBuilder, QpType, Qpn, RdmaDeviceNetworkParam,
        RdmaDeviceNetworkParamBuilder, PAGE_SIZE,
    },
    AlignedMemory, Device, Mr, Pd
};
use std::{ffi::c_void, net::Ipv4Addr, thread::sleep, time::Duration};

use crate::common::init_logging;

const ORDER: usize = 32;
const SHM_PATH: &str = "/bluesim1\0";

#[macro_use]
extern crate ctor;

/// Use `LockedHeap` as global allocator
#[global_allocator]
static HEAP_ALLOCATOR: LockedHeap<ORDER> = LockedHeap::<ORDER>::new();
const HEAP_BLOCK_SIZE: usize = 1024 * 1024 * 64;
const BUFFER_LENGTH: usize = 1024 * 128;
static mut HEAP_START_ADDR: usize = 0;

mod common;

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

fn create_and_init_card<'a>(
    card_id: usize,
    mock_server_addr: &str,
    qpn: Qpn,
    local_network: &RdmaDeviceNetworkParam,
    remote_network: &RdmaDeviceNetworkParam,
) -> (Device, Pd, Mr, AlignedMemory<'a>) {
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

    let mut mr_buffer = AlignedMemory::new(BUFFER_LENGTH).unwrap();

    unsafe {
        info!(
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
    let qp = QpBuilder::default()
        .pd(pd)
        .qpn(qpn)
        .qp_type(QpType::Rc)
        .rq_acc_flags(access_flag)
        .pmtu(Pmtu::Mtu4096)
        .dqp_ip(remote_network.ipaddr)
        .dqp_mac(remote_network.macaddr)
        .build()
        .unwrap();
    dev.create_qp(&qp).unwrap();
    info!("[{}] QP created", card_id);

    (dev, pd, mr, mr_buffer)
}
fn main() {
    init_logging("log.txt").unwrap();
    let qp_manager = QpManager::new();
    let qpn = qp_manager.alloc().unwrap();
    let a_network = RdmaDeviceNetworkParamBuilder::default()
        .gateway(Ipv4Addr::new(192, 168, 0, 0x1))
        .netmask(Ipv4Addr::new(255, 255, 255, 0))
        .ipaddr(Ipv4Addr::new(192, 168, 0, 2))
        .macaddr(MacAddress::new([0xaa,0xaa,0xaa,0xaa,0xaa,0xaa]))
        .build()
        .unwrap();
    let b_network = RdmaDeviceNetworkParamBuilder::default()
        .gateway(Ipv4Addr::new(192, 168, 0, 0x1))
        .netmask(Ipv4Addr::new(255, 255, 255, 0))
        .ipaddr(Ipv4Addr::new(192, 168, 0, 3))
        .macaddr(MacAddress::new([0xbb,0xbb,0xbb,0xbb,0xbb,0xbb]))
        .build()
        .unwrap();
    let (dev_a, _pd_a, mr_a, mut mr_buffer_a) =
        create_and_init_card(0, "0.0.0.0:9873", qpn, &a_network, &b_network);
    let (_dev_b, _pd_b, mr_b, mut mr_buffer_b) =
        create_and_init_card(1, "0.0.0.0:9875", qpn, &b_network, &a_network);
    
    let mut packet:[u8;40] = [
            69, 0, 0, 40, 136, 10, 64, 0, 128, 6, 251, 191, 10, 29, 186, 11, 82, 157, 96, 64, 47, 3, 29, 114, 149, 240, 25, 197, 60, 84, 195, 192, 80, 16, 1, 253, 58, 146, 0, 0
    ];
    for i in 0..32{
        packet.last_mut().map(|x| *x= i as u8);
        dev_a.write_raw(b_network.ipaddr,b_network.macaddr,&packet).unwrap();
    }
    sleep(Duration::from_secs(10));
    for i in 0..32{
        packet.last_mut().map(|x| *x= i as u8);
        dev_a.write_raw(b_network.ipaddr,b_network.macaddr,&packet).unwrap();
    }
    sleep(Duration::from_secs(10));

}
