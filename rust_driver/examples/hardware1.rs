#![allow(warnings)] // FIXME: debug only
use eui48::MacAddress;
use log::{debug, info};
use open_rdma_driver::{
    qp::QpManager,
    types::{
        Key, MemAccessTypeFlag, Pmtu, QpBuilder, QpType, Qpn, RdmaDeviceNetworkParam, RdmaDeviceNetworkParamBuilder, Sge, WorkReqSendFlag, PAGE_SIZE
    },
    Device, HugePage, Mr, Pd,
};
use std::{
    io::{self, BufRead},
    net::Ipv4Addr,
};

use crate::common::init_logging;

const BUFFER_LENGTH: usize = 1024 * 128;
const SEND_CNT: usize = 1024 * 64;

mod common;

fn create_and_init_card<'a>(
    card_id: usize,
    qpn: Qpn,
    local_network: &RdmaDeviceNetworkParam,
    remote_network: &RdmaDeviceNetworkParam,
) -> (Device, Pd, Mr, HugePage) {
    let dev = Device::new_hardware(local_network, "/dev/infiniband/uverbs0".to_owned()).unwrap();
    info!("[{}] Device created", card_id);

    let pd = dev.alloc_pd().unwrap();
    info!("[{}] PD allocated", card_id);

    let mr_buffer = HugePage::new(BUFFER_LENGTH).unwrap();

    let access_flag = MemAccessTypeFlag::IbvAccessRemoteRead
        | MemAccessTypeFlag::IbvAccessRemoteWrite
        | MemAccessTypeFlag::IbvAccessLocalWrite;
    let mr = dev
        .reg_mr(
            pd,
            mr_buffer.as_ptr() as u64,
            mr_buffer.size() as u32,
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
        .pmtu(Pmtu::Mtu1024)
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

    let a_network = RdmaDeviceNetworkParamBuilder::default()
        .gateway(Ipv4Addr::new(127, 0, 0, 0x1))
        .netmask(Ipv4Addr::new(255, 0, 0, 0))
        .ipaddr(Ipv4Addr::new(127, 0, 0, 2))
        .macaddr(MacAddress::new([0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFE]))
        .build()
        .unwrap();
    // let dev = Device::new_hardware(&a_network, "/dev/infiniband/uverbs0".to_owned()).unwrap();

    debug!("===========1====================");

    let b_network = RdmaDeviceNetworkParamBuilder::default()
        .gateway(Ipv4Addr::new(127, 0, 0, 0x1))
        .netmask(Ipv4Addr::new(255, 0, 0, 0))
        .ipaddr(Ipv4Addr::new(127, 0, 0, 3))
        .macaddr(MacAddress::new([0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF]))
        .build()
        .unwrap();
    debug!("===========2====================");

    let qp_manager = QpManager::new();
    debug!("===========3====================");
    let qpn = qp_manager.alloc().unwrap();
    debug!("===========4====================");
    let (dev_a, _pd_a, mr_a, mut mr_buffer_a) =
        create_and_init_card(0, qpn, &a_network, &b_network);
    debug!("===========5====================");
    // let (_dev_b, _pd_b, mr_b, mut mr_buffer_b) =
    //     create_and_init_card(1, qpn, &b_network, &a_network);
    let dpqn = qpn;
    for (idx, item) in mr_buffer_a.iter_mut().enumerate() {
        *item = idx as u8;
    }
    // for item in mr_buffer_b[0..].iter_mut() {
    //     *item = 0
    // }

    let sge0 = Sge::new(
        &mr_buffer_a[0] as *const u8 as u64,
        SEND_CNT.try_into().unwrap(),
        mr_a.get_key(),
    );
    debug!("===========6====================");
    // let sge1 = Sge::new(
    //     &mr_buffer_a[SEND_CNT] as *const u8 as u64,
    //     SEND_CNT.try_into().unwrap(),
    //     mr_a.get_key(),
    // );

    println!("please input peer memory info:");
    let stdin = io::stdin();
    let mut peer_mem_info_str = String::new();
    let _ = stdin
        .lock()
        .read_line(&mut peer_mem_info_str)
        .expect("wait input error");
    let splited_params_strs = peer_mem_info_str.trim().split(",").collect::<Vec<_>>();
    let raddr = u64::from_str_radix(splited_params_strs[0], 16).unwrap();
    let rkey = Key::new(u32::from_str_radix(splited_params_strs[1], 16).unwrap());

    // test write
    let ctx1 = dev_a
        .write(dpqn, raddr, rkey, WorkReqSendFlag::IbvSendSignaled, sge0)
        .unwrap();
    // let ctx2 = dev_a
    //     .write(
    //         dpqn,
    //         &mr_buffer_b[SEND_CNT] as *const u8 as u64,
    //         mr_b.get_key(),
    //         MemAccessTypeFlag::empty(),
    //         sge1,
    //     )
    //     .unwrap();

    debug!("===========7====================");
    let _ = ctx1.wait();
    debug!("===========8====================");
    // let _ = ctx2.wait();

    // if mr_buffer_a[0..SEND_CNT * 2] != mr_buffer_b[0..SEND_CNT * 2] {
    //     for i in 0..SEND_CNT * 2 {
    //         if mr_buffer_a[i] != mr_buffer_b[i] {
    //             panic!("{}: {} != {}", i, mr_buffer_a[i], mr_buffer_b[i]);
    //         }
    //     }
    // }

    // for item in mr_buffer_a.iter_mut() {
    //     *item = 0;
    // }
    // for (idx, item) in mr_buffer_a.iter_mut().enumerate() {
    //     *item = idx as u8;
    // }

    // // we read from b to a

    // let sge_read = Sge::new(
    //     &mr_buffer_a[SEND_CNT] as *const u8 as u64,
    //     SEND_CNT.try_into().unwrap(),
    //     mr_a.get_key(),
    // );

    // let ctx1 = dev_a
    //     .read(
    //         dpqn,
    //         &mr_buffer_b[0] as *const u8 as u64,
    //         mr_b.get_key(),
    //         MemAccessTypeFlag::IbvAccessNoFlags,
    //         sge_read,
    //     )
    //     .unwrap();
    // let _ = ctx1.wait();
    // info!("Read req sent");
}