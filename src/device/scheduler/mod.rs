use std::{collections::LinkedList, fmt::Debug, sync::Arc, thread::spawn};

use crossbeam_channel::{Receiver, Sender, TryRecvError};
use log::error;

use super::{DeviceError, ToCardCtrlRbDescSge, ToCardRb, ToCardWorkRbDesc, ToCardWorkRbDescCommon};

use crate::{
    types::{Pmtu, Psn, Qpn},
    utils::get_first_packet_max_length,
};

const SCHEDULER_SIZE_U32: u32 = 1024 * 32; // 32KB
const SCHEDULER_SIZE: usize = SCHEDULER_SIZE_U32 as usize;
const MAX_SGL_LENGTH: usize = 1;

pub(crate) mod round_robin;

/// A descriptor scheduler that cut descriptor into `SCHEDULER_SIZE` size and schedule with a strategy.
#[derive(Debug)]
#[allow(dead_code)]
pub(crate) struct DescriptorScheduler {
    sender: Sender<ToCardWorkRbDesc>,
    receiver: Receiver<ToCardWorkRbDesc>,
    strategy: Arc<dyn SchedulerStrategy>,
    thread_handler: std::thread::JoinHandle<()>,
}

#[allow(clippy::module_name_repetitions)]
pub(crate) trait SchedulerStrategy: Send + Sync + Debug {
    #[allow(clippy::linkedlist)]
    fn push(&self, qpn: Qpn, desc: LinkedList<ToCardWorkRbDesc>) -> Result<(), DeviceError>;

    fn pop(&self) -> Result<Option<ToCardWorkRbDesc>, DeviceError>;
}

struct SGList {
    pub(crate) data: [ToCardCtrlRbDescSge; MAX_SGL_LENGTH],
    pub(crate) cur_level: u32,
    pub(crate) len: u32,
}

impl SGList {
    pub(crate) fn new_from_sge(sge: ToCardCtrlRbDescSge) -> Self {
        let mut sge_list = Self {
            data: [ToCardCtrlRbDescSge::default(); MAX_SGL_LENGTH],
            cur_level: 0,
            len: 1,
        };
        sge_list.data[0] = sge;
        sge_list
    }
}

impl Default for SGList {
    fn default() -> Self {
        Self {
            data: [ToCardCtrlRbDescSge::default(); MAX_SGL_LENGTH],
            cur_level: 0,
            len: 0,
        }
    }
}

impl DescriptorScheduler {
    pub(crate) fn new(strat: Arc<dyn SchedulerStrategy>) -> Self {
        let (sender, receiver) = crossbeam_channel::unbounded();
        let strategy: Arc<dyn SchedulerStrategy> = Arc::<dyn SchedulerStrategy>::clone(&strat);
        let thread_receiver = receiver.clone();
        let thread_handler = spawn(move || loop {
            let desc = match thread_receiver.try_recv() {
                Ok(desc) => Some(desc),
                Err(TryRecvError::Empty) => None,
                Err(TryRecvError::Disconnected) => return,
            };
            if let Some(desc) = desc {
                let dqpn = get_to_card_desc_common(&desc).dqpn;
                let splited_descs = split_descriptor(desc);
                if let Err(e) = strategy.push(dqpn, splited_descs) {
                    error!("failed to push descriptors: {:?}", e);
                }
            }
        });
        Self {
            sender,
            strategy: strat,
            thread_handler,
            receiver,
        }
    }

    pub(crate) fn pop(self: &Arc<Self>) -> Result<Option<ToCardWorkRbDesc>, DeviceError> {
        self.strategy.pop()
    }
}

impl ToCardRb<ToCardWorkRbDesc> for DescriptorScheduler {
    fn push(&self, desc: ToCardWorkRbDesc) -> Result<(), DeviceError> {
        self.sender
            .send(desc)
            .map_err(|e| DeviceError::Scheduler(e.to_string()))
    }
}

#[allow(clippy::cast_possible_truncation)]
fn get_first_schedule_segment_length(va: u64) -> u32 {
    let offset = va % SCHEDULER_SIZE as u64;
    // the `SCHEDULER_SIZE` is in range of u32 and offset is less than `SCHEDULER_SIZE`
    // So is safe to convert
    (SCHEDULER_SIZE as u32) - offset as u32
}

fn get_to_card_desc_common(desc: &ToCardWorkRbDesc) -> &ToCardWorkRbDescCommon {
    match desc {
        ToCardWorkRbDesc::Read(req) => &req.common,
        ToCardWorkRbDesc::Write(req) | ToCardWorkRbDesc::ReadResp(req) => &req.common,
        ToCardWorkRbDesc::WriteWithImm(req) => &req.common,
    }
}

// We allow indexing_slicing because
// * `new_sgl_level` will always smaller than `origin_sgl` sgl level, which is less than `MAX_SGL_LENGTH`
// * `current_level` won't be greater than `origin_sgl.len`, which is less than `MAX_SGL_LENGTH`
#[allow(clippy::indexing_slicing)]
fn cut_from_sgl(mut length: u32, origin_sgl: &mut SGList) -> SGList {
    let mut current_level = origin_sgl.cur_level as usize;
    let mut new_sgl = SGList::default();
    let mut new_sgl_level: usize = 0;
    #[allow(clippy::cast_possible_truncation)]
    while (current_level as u32) < origin_sgl.len {
        // if we can cut from current level, just cut and return
        if origin_sgl.data[current_level].len >= length {
            let addr = origin_sgl.data[current_level].addr;
            new_sgl.data[new_sgl_level] = ToCardCtrlRbDescSge {
                addr,
                len: length,
                key: origin_sgl.data[current_level].key,
            };
            new_sgl.len = new_sgl_level as u32 + 1;
            origin_sgl.data[current_level].addr += u64::from(length);
            origin_sgl.data[current_level].len -= length;
            if origin_sgl.data[current_level].len == 0 {
                current_level += 1;
            }
            origin_sgl.cur_level = current_level as u32;
            return new_sgl;
        }
        // otherwise check next level
        let addr = origin_sgl.data[current_level].addr as *mut u8;
        new_sgl.data[new_sgl_level] = ToCardCtrlRbDescSge {
            addr: addr as u64,
            len: origin_sgl.data[current_level].len,
            key: origin_sgl.data[current_level].key,
        };
        new_sgl_level += 1;
        length -= origin_sgl.data[current_level].len;
        origin_sgl.data[current_level].len = 0;
        current_level += 1;
    }
    unreachable!("The length is too long");
}

fn get_total_len(desc: &ToCardWorkRbDesc) -> u32 {
    match desc {
        ToCardWorkRbDesc::Read(req) => req.common.total_len,
        ToCardWorkRbDesc::Write(req) | ToCardWorkRbDesc::ReadResp(req) => req.common.total_len,
        ToCardWorkRbDesc::WriteWithImm(req) => req.common.total_len,
    }
}

/// Split the descriptor into multiple descriptors if it is greater than the `SCHEDULER_SIZE` size.
#[allow(clippy::linkedlist)]
pub(crate) fn split_descriptor(desc: ToCardWorkRbDesc) -> LinkedList<ToCardWorkRbDesc> {
    let is_read = matches!(desc, ToCardWorkRbDesc::Read(_));
    let total_len = get_total_len(&desc);
    #[allow(clippy::cast_possible_truncation)]
    if is_read || total_len < SCHEDULER_SIZE as u32 {
        let mut list = LinkedList::new();
        list.push_back(desc);
        return list;
    }

    let (raddr, pmtu, psn, sge) = match &desc {
        ToCardWorkRbDesc::Read(_) => unreachable!(),
        ToCardWorkRbDesc::Write(req) | ToCardWorkRbDesc::ReadResp(req) => {
            (req.common.raddr, req.common.pmtu, req.common.psn, req.sge0)
        }
        ToCardWorkRbDesc::WriteWithImm(req) => {
            (req.common.raddr, req.common.pmtu, req.common.psn, req.sge0)
        }
    };

    let mut sg_list = SGList::new_from_sge(sge);

    let mut descs = LinkedList::new();
    let mut this_length = get_first_schedule_segment_length(raddr);
    let mut remain_data_length = total_len;
    let mut current_va = raddr;
    let mut base_psn = psn;
    while remain_data_length > 0 {
        let mut new_desc = desc.clone();
        let new_sge = cut_from_sgl(this_length, &mut sg_list).data[0];
        match &mut new_desc {
            ToCardWorkRbDesc::Read(_) => unreachable!(),
            ToCardWorkRbDesc::Write(ref mut req) | ToCardWorkRbDesc::ReadResp(ref mut req) => {
                req.sge0 = new_sge;
                req.common.total_len = this_length;
                req.common.raddr = current_va;
                req.common.psn = base_psn;
                req.is_first = false;
                req.is_last = false;
            }
            ToCardWorkRbDesc::WriteWithImm(ref mut req) => {
                req.sge0 = new_sge;
                req.common.total_len = this_length;
                req.common.raddr = current_va;
                req.common.psn = base_psn;
                req.is_first = false;
                req.is_last = false;
            }
        }
        base_psn = recalculate_psn(current_va, pmtu, this_length, base_psn);
        descs.push_back(new_desc);
        current_va += u64::from(this_length);
        remain_data_length -= this_length;
        this_length = if remain_data_length > SCHEDULER_SIZE_U32 {
            SCHEDULER_SIZE_U32
        } else {
            remain_data_length
        };
    }
    // The above code guarantee there at least 2 descriptors in the list
    if let Some(req) = descs.front_mut() {
        match req {
            ToCardWorkRbDesc::Read(_) => unreachable!(),
            ToCardWorkRbDesc::Write(req) | ToCardWorkRbDesc::ReadResp(req) => {
                req.is_first = true;
                req.common.total_len = total_len;
            }
            ToCardWorkRbDesc::WriteWithImm(req) => {
                req.is_first = true;
                req.common.total_len = total_len;
            }
        }
    }

    if let Some(req) = descs.back_mut() {
        match req {
            ToCardWorkRbDesc::Read(_) => unreachable!(),
            ToCardWorkRbDesc::Write(req) | ToCardWorkRbDesc::ReadResp(req) => {
                req.is_last = true;
            }
            ToCardWorkRbDesc::WriteWithImm(req) => {
                req.is_last = true;
            }
        }
    }

    descs
}

/// Recalculate the PSN of the descriptor
///
/// # Example
/// `base_psn` = 0 , `desc.raddr` = 4095,`pmtu` = 4096 and `desc.common_header.total_len` = 4096 * 4.
///
/// so the `first_packet_length` = 4096 - 4095 = 1
/// then the psn = 0 + ceil((4096 * 4 - `first_packet_length`),4096) = 4
/// That means we will send 5 packets in total(psn=0,1,2,3,4)
/// And the next psn will be 5
fn recalculate_psn(raddr: u64, pmtu: Pmtu, total_len: u32, base_psn: Psn) -> Psn {
    let pmtu = u32::from(&pmtu);
    let first_packet_length = get_first_packet_max_length(raddr, pmtu);
    let first_packet_length = total_len.min(first_packet_length);
    // first packet psn = base_psn
    // so the total psn = base_psn + (desc.common_header.total_len - first_packet_length) / pmtu + 1
    let last_packet_psn = base_psn.wrapping_add((total_len - first_packet_length).div_ceil(pmtu));
    last_packet_psn.wrapping_add(1)
}

#[cfg(test)]
mod test {
    use std::net::Ipv4Addr;
    use std::thread::sleep;
    use std::{collections::LinkedList, sync::Arc};

    use eui48::MacAddress;

    use crate::device::scheduler::SCHEDULER_SIZE;
    use crate::device::{
        ToCardCtrlRbDescSge, ToCardRb, ToCardWorkRbDesc, ToCardWorkRbDescCommon,
        ToCardWorkRbDescWrite,
    };

    use crate::types::{Key, MemAccessTypeFlag, Msn, Psn, Qpn};

    use super::{SGList, MAX_SGL_LENGTH};

    pub(crate) struct SGListBuilder {
        sg_list: Vec<ToCardCtrlRbDescSge>,
    }

    impl SGListBuilder {
        pub(crate) fn new() -> Self {
            SGListBuilder {
                sg_list: Vec::new(),
            }
        }

        pub(crate) fn with_sge(&mut self, addr: u64, len: u32, key: Key) -> &mut Self {
            self.sg_list.push(ToCardCtrlRbDescSge { addr, len, key });
            self
        }

        pub(crate) fn build(&self) -> SGList {
            let mut sg_list = SGList::default();
            for sge in self.sg_list.iter() {
                sg_list.data[sg_list.len as usize] = *sge;
                sg_list.len += 1;
            }
            while sg_list.len < MAX_SGL_LENGTH.try_into().unwrap() {
                sg_list.data[sg_list.len as usize] = ToCardCtrlRbDescSge {
                    addr: 0,
                    len: 0,
                    key: Key::default(),
                };
            }
            sg_list
        }
    }

    #[test]
    fn test_helper_function_first_length() {
        let length = super::get_first_schedule_segment_length(0);
        assert_eq!(length, 1024 * 32);
        let length = super::get_first_schedule_segment_length(1024 * 29);
        assert_eq!(length, 1024 * 3);
        let length = super::get_first_schedule_segment_length(1024 * 32 + 1);
        assert_eq!(length, 1024 * 32 - 1);
    }
    #[test]
    fn test_cut_from_sgl() {
        let mut sgl = SGListBuilder::new()
            .with_sge(0, 1024, Key::default())
            .build();
        let new_sgl = super::cut_from_sgl(512, &mut sgl);
        assert_eq!(new_sgl.len, 1);
        assert_eq!(new_sgl.data[0].len, 512);
        assert_eq!(sgl.data[0].len, 512);
        assert_eq!(sgl.data[0].addr, 512);
    }

    #[allow(dead_code)]
    fn convert_list_to_vec<T>(list: LinkedList<T>) -> Vec<T> {
        let mut vec = Vec::new();
        for i in list {
            vec.push(i);
        }
        vec
    }

    #[test]
    fn test_scheduler() {
        let va = 29 * 1024;
        let length = 1024 * 36; // should cut into 3 segments: 29k - 32k, 32k - 64k, 64k-65k
        let strategy = super::round_robin::RoundRobinStrategy::new();
        let scheduler = Arc::new(super::DescriptorScheduler::new(Arc::new(strategy)));
        let desc = ToCardWorkRbDesc::Write(ToCardWorkRbDescWrite {
            common: ToCardWorkRbDescCommon {
                total_len: length,
                raddr: va,
                rkey: Key::default(),
                dqp_ip: Ipv4Addr::LOCALHOST,
                dqpn: Qpn::new(2),
                mac_addr: MacAddress::default(),
                pmtu: crate::types::Pmtu::Mtu4096,
                flags: MemAccessTypeFlag::empty(),
                qp_type: crate::types::QpType::Rc,
                psn: Psn::new(0),
                msn: Msn::new(0x27),
            },
            is_last: true,
            is_first: true,
            sge0: ToCardCtrlRbDescSge {
                addr: 0,
                len: length,
                key: Key::new(3),
            },
            sge1: None,
            sge2: None,
            sge3: None,
        });
        scheduler.push(desc).unwrap();
        // schedule the thread;
        sleep(std::time::Duration::from_millis(1));
        let desc1 = scheduler.pop().unwrap();
        assert!(desc1.is_some());
        let desc1 = match desc1.unwrap() {
            ToCardWorkRbDesc::Write(req) => req,
            ToCardWorkRbDesc::Read(_)
            | ToCardWorkRbDesc::WriteWithImm(_)
            | ToCardWorkRbDesc::ReadResp(_) => unreachable!(),
        };
        assert_eq!(desc1.common.total_len, length);
        assert!(desc1.is_first);
        assert!(!desc1.is_last);

        let desc2 = scheduler.pop().unwrap();
        let desc2 = match desc2.unwrap() {
            ToCardWorkRbDesc::Write(req) => req,
            ToCardWorkRbDesc::Read(_)
            | ToCardWorkRbDesc::WriteWithImm(_)
            | ToCardWorkRbDesc::ReadResp(_) => unreachable!(),
        };
        assert_eq!(desc2.common.total_len as usize, SCHEDULER_SIZE);
        assert!(!desc2.is_first);
        assert!(!desc2.is_last);

        let desc3 = scheduler.pop().unwrap();
        let desc3 = match desc3.unwrap() {
            ToCardWorkRbDesc::Write(req) => req,
            ToCardWorkRbDesc::Read(_)
            | ToCardWorkRbDesc::WriteWithImm(_)
            | ToCardWorkRbDesc::ReadResp(_) => unreachable!(),
        };
        assert_eq!(
            desc3.common.total_len as usize,
            (length as usize) - (SCHEDULER_SIZE - va as usize) - SCHEDULER_SIZE
        );
        assert!(!desc3.is_first);
        assert!(desc3.is_last);
    }
}
