use eui48::MacAddress;
use log::info;
use open_rdma_driver::{
    qp::QpManager, types::{
        MemAccessTypeFlag, Msn, Pmtu, QpBuilder, QpType, Qpn, RdmaDeviceNetworkParam,
        RdmaDeviceNetworkParamBuilder, Sge, WorkReqSendFlag, PAGE_SIZE,
    }, AlignedMemory, Device, DeviceConfigBuilder, DeviceType, Mr, Pd, RetryConfig, SealedDesc, TestingHandler, TestingStrategy
};
use parking_lot::Mutex;
use serial_test::serial;
use std::{collections::HashSet, net::Ipv4Addr, sync::Arc, time::Duration};

use crate::common::init_logging;

const SHM_PATH: &str = "/bluesim1\0";
const HEAP_BLOCK_SIZE: usize = 1024 * 1024 * 64;
const SEND_CNT: usize = 1024 * 6;
const BUFFER_LENGTH: usize = 1024 * 128;

mod common;

setup_emulator!(
    0x7f7e8e600000,
    HEAP_BLOCK_SIZE,
    SHM_PATH,
    "../blue-rdma\0",
    "run_system_test.sh\0"
);
#[derive(Default, Debug, Clone)]
struct Handler {
    msn_hashset: Arc<Mutex<HashSet<Msn>>>,
}

impl TestingHandler for Handler {
    fn handle_pkt(&self, desc: &mut Vec<SealedDesc>) -> Result<(), Box<dyn std::error::Error>> {
        assert_eq!(desc.len(), 1);
        if let Some(msn) = desc[0].msn() {
            let mut set = self.msn_hashset.lock();
            if set.contains(&msn) {
                return Ok(());
            }
            set.insert(msn);
            let new_desc = desc[0].reorder(vec![1, 2, 4, 5, 7, 8, 10]);
            desc.clear();
            desc.extend(new_desc);
        }
        Ok(())
    }
}
fn create_and_init_card<'a>(
    card_id: usize,
    mock_server_addr: &str,
    qpn: Qpn,
    local_network: RdmaDeviceNetworkParam,
    remote_network: &RdmaDeviceNetworkParam,
) -> (Device, Pd, Mr, AlignedMemory<'a>) {
    let head_start_addr = unsafe { HEAP_START_ADDR };
    let config = DeviceConfigBuilder::default()
        .network_config(local_network)
        .retry_config(RetryConfig::new(true,3, Duration::from_millis( 10000), Duration::from_millis(100)))
        .device_type(DeviceType::Emulated {
            rpc_server_addr: mock_server_addr.parse().unwrap(),
            heap_mem_start_addr: head_start_addr,
        })
        .strategy(TestingStrategy::new(Handler::default()))
        .build()
        .unwrap();
    let dev = Device::new(config).unwrap();
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
        .peer_qpn(qpn)
        .qp_type(QpType::Rc)
        .rq_acc_flags(access_flag)
        .pmtu(Pmtu::Mtu512)
        .dqp_ip(remote_network.ipaddr)
        .dqp_mac(remote_network.macaddr)
        .build()
        .unwrap();
    dev.create_qp(&qp).unwrap();
    info!("[{}] QP created", card_id);

    (dev, pd, mr, mr_buffer)
}

#[ignore] // FIXME: ci failed to allocate memory?
#[test]
fn test_emulated() {
    init_logging("log.txt").unwrap();
    let qp_manager = QpManager::new();
    let qpn = qp_manager.alloc().unwrap();
    let a_network = RdmaDeviceNetworkParamBuilder::default()
        .gateway(Ipv4Addr::new(192, 168, 0, 0x1))
        .netmask(Ipv4Addr::new(255, 255, 255, 0))
        .ipaddr(Ipv4Addr::new(192, 168, 0, 2))
        .macaddr(MacAddress::new([0xAA, 0xAB, 0xAC, 0xAD, 0xAE, 0xFE]))
        .build()
        .unwrap();
    let b_network = RdmaDeviceNetworkParamBuilder::default()
        .gateway(Ipv4Addr::new(192, 168, 0, 0x1))
        .netmask(Ipv4Addr::new(255, 255, 255, 0))
        .ipaddr(Ipv4Addr::new(192, 168, 0, 3))
        .macaddr(MacAddress::new([0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF]))
        .build()
        .unwrap();
    let (dev_a, _pd_a, mr_a, mut mr_buffer_a) =
        create_and_init_card(0, "0.0.0.0:9873", qpn, a_network, &b_network);
    let (_dev_b, _pd_b, mr_b, mut mr_buffer_b) =
        create_and_init_card(1, "0.0.0.0:9875", qpn, b_network, &a_network);

    let dpqn = qpn;

    for (idx, item) in mr_buffer_a.iter_mut().enumerate() {
        *item = idx as u8;
    }
    for item in mr_buffer_b[0..].iter_mut() {
        *item = 0
    }

    let sge0 = Sge::new(
        &mr_buffer_a[0] as *const u8 as u64,
        512 * 11,
        mr_a.get_key(),
    );

    let ctx1 = dev_a
        .write(
            dpqn,
            &mr_buffer_b[0] as *const u8 as u64,
            mr_b.get_key(),
            WorkReqSendFlag::IbvSendSignaled,
            sge0,
        )
        .unwrap();

    let _ = ctx1.wait();
    assert_eq!(mr_buffer_a[0..SEND_CNT], mr_buffer_b[0..SEND_CNT]);
    info!("write success");

}
