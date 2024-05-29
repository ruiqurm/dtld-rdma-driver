use std::error::Error;

use crate::{
    types::{QpType, Qpn},
    RoundRobinStrategy, SchedulerStrategy, SealedDesc, POP_BATCH_SIZE,
};

pub(crate) trait TestingHandler: Send + Sync + core::fmt::Debug + Clone + 'static {
    fn handle_read_pkt(&self, desc: &mut Vec<SealedDesc>) -> Result<(), Box<dyn Error>>;
    fn handle_common_nic_pkt(&self, desc: &mut Vec<SealedDesc>) -> Result<(), Box<dyn Error>>;
    fn handle_data_pkt(&self, desc: &mut Vec<SealedDesc>) -> Result<(), Box<dyn Error>>;
}

#[derive(Debug, Clone)]
pub(crate) struct TestingStrategy<T: TestingHandler>(RoundRobinStrategy, T);

impl<T: TestingHandler> TestingStrategy<T> {
    pub(crate) fn new(handlers: T) -> Self {
        TestingStrategy(RoundRobinStrategy::new(), handlers)
    }
}

impl<T: TestingHandler> SchedulerStrategy for TestingStrategy<T> {
    fn push<I>(&self, qpn: Qpn, desc: I) -> Result<(), Box<dyn Error>>
    where
        I: Iterator<Item = SealedDesc>,
    {
        // dispatch to different handlers
        let mut vec = desc.collect::<Vec<_>>();

        if vec.is_empty() {
            return Ok(());
        }

        #[allow(clippy::indexing_slicing)]
        if vec.len() == 1 {
            // handle read request, ack or nack
            match &mut *vec[0].0 {
                crate::device::ToCardWorkRbDesc::Read(_) => {
                    self.1.handle_read_pkt(&mut vec)?;
                    if vec.is_empty() {
                        return Ok(());
                    }
                    return self.0.push(qpn, vec.into_iter());
                }
                crate::device::ToCardWorkRbDesc::WriteWithImm(raw_desc) => {
                    if matches!(raw_desc.common.qp_type, QpType::RawPacket) {
                        // handle raw packet
                        self.1.handle_common_nic_pkt(&mut vec)?;
                        if vec.is_empty() {
                            return Ok(());
                        }
                        return self.0.push(qpn, vec.into_iter());
                    };
                }
                crate::device::ToCardWorkRbDesc::ReadResp(_) => {}
                crate::device::ToCardWorkRbDesc::Write(_) => {}
            }
        }

        self.1.handle_data_pkt(&mut vec)?;
        if vec.is_empty() {
            return Ok(());
        }
        self.0.push(qpn, vec.into_iter())
    }

    #[allow(
        clippy::unwrap_in_result,
        clippy::unwrap_used,
        clippy::arithmetic_side_effects,
        clippy::indexing_slicing
    )]
    fn pop_batch(&self) -> Result<([Option<SealedDesc>; POP_BATCH_SIZE], u32), Box<dyn Error>> {
        self.0.pop_batch()
    }
}

#[cfg(test)]
mod tests {
    use crate::{
        device::{ToCardWorkRbDesc, ToCardWorkRbDescCommon, ToCardWorkRbDescWrite},
        types::{Psn, Qpn},
        SchedulerStrategy, SealedDesc,
    };

    use super::{TestingHandler, TestingStrategy};

    #[derive(Debug, Clone)]
    struct Handlers {
        handler1: fn(&mut Vec<SealedDesc>),
        handler2: fn(&mut Vec<SealedDesc>),
        handler3: fn(&mut Vec<SealedDesc>),
    }

    impl TestingHandler for Handlers {
        fn handle_read_pkt(
            &self,
            desc: &mut Vec<SealedDesc>,
        ) -> Result<(), Box<dyn std::error::Error>> {
            (self.handler1)(desc);
            Ok(())
        }

        fn handle_common_nic_pkt(
            &self,
            desc: &mut Vec<SealedDesc>,
        ) -> Result<(), Box<dyn std::error::Error>> {
            (self.handler2)(desc);
            Ok(())
        }

        fn handle_data_pkt(
            &self,
            desc: &mut Vec<SealedDesc>,
        ) -> Result<(), Box<dyn std::error::Error>> {
            (self.handler3)(desc);
            Ok(())
        }
    }
    fn generate_read_request() -> SealedDesc {
        let desc = Box::new(ToCardWorkRbDesc::Read(Default::default()));
        SealedDesc::from(desc)
    }
    fn generate_random_descriptors(qpn: u32, psn: u32, num: usize) -> Vec<SealedDesc> {
        let common = ToCardWorkRbDescCommon {
            psn: Psn::new(psn),
            dqpn: Qpn::new(qpn),
            ..ToCardWorkRbDescCommon::default()
        };
        (0..num)
            .map(|idx| {
                let mut common_clone = common.clone();
                common_clone.psn = common_clone.psn.wrapping_add(idx.try_into().unwrap());
                let desc = Box::new(ToCardWorkRbDesc::Write(ToCardWorkRbDescWrite {
                    common: common_clone,
                    ..Default::default()
                }));
                SealedDesc::from(desc)
            })
            .collect()
    }

    #[test]
    fn test_testing_strategy() {
        fn filter_threes_fold(desc: &mut Vec<SealedDesc>) {
            desc.retain(|desc| desc.get_psn().get() % 3 != 0);
        }
        fn block_read(desc: &mut Vec<SealedDesc>) {
            desc.clear()
        }
        fn do_nothing(_desc: &mut Vec<SealedDesc>) {
            let _ = _desc;
        }
        let strategy = TestingStrategy::new(Handlers {
            handler1: block_read,
            handler2: do_nothing,
            handler3: filter_threes_fold,
        });
        // generate psn from 1 to 10
        let desc = generate_random_descriptors(2, 1, 10);
        // should filter 3, 6, 9
        strategy.push(Qpn::new(2), desc.into_iter()).unwrap();
        let (batch1, n) = strategy.pop_batch().unwrap();
        assert_eq!(n, 7);
        let result_psn = batch1
            .into_iter()
            .flatten()
            .map(|desc| desc.get_psn().get())
            .collect::<Vec<u32>>();
        assert_eq!(result_psn, vec![1, 2, 4, 5, 7, 8, 10]);

        let (_, n) = strategy.pop_batch().unwrap();
        assert_eq!(n, 0);

        // test filter read
        let desc = generate_read_request();
        strategy.push(Qpn::new(2), vec![desc].into_iter()).unwrap();
        let (_, n) = strategy.pop_batch().unwrap();
        assert_eq!(n, 0);
    }
}
