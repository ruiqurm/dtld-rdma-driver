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
                    return self.0.push(qpn, vec.into_iter());
                }
                crate::device::ToCardWorkRbDesc::WriteWithImm(raw_desc) => {
                    if matches!(raw_desc.common.qp_type, QpType::RawPacket) {
                        // handle raw packet
                        self.1.handle_common_nic_pkt(&mut vec)?;
                        return self.0.push(qpn, vec.into_iter());
                    };
                }
                crate::device::ToCardWorkRbDesc::ReadResp(_) => {}
                crate::device::ToCardWorkRbDesc::Write(_) => {}
            }
        }

        self.1.handle_data_pkt(&mut vec)?;
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
