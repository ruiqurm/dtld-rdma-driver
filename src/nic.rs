
use crate::{
    buf::{PacketBuf, Slot, NIC_PACKET_BUFFER_SLOT_SIZE},
    device::{ToCardWorkRbDescBuilder, ToCardWorkRbDescCommon},
    types::QpType,
    Device as BlueRdmaDevice, WorkDescriptorSender,
};
use eui48::MacAddress;
use flume::{Receiver, TryRecvError};
use smoltcp::{
    phy::{Device, DeviceCapabilities, Medium, RxToken, TxToken},
    time::Instant,
};

// the first 6 bytes of the ethernet frame is the destination mac address
const ETH_DST_START: usize = 0;
const ETH_DST_END: usize = 6;

pub(crate) struct NicRecvNotification {
    pub(crate) buf: Slot<NIC_PACKET_BUFFER_SLOT_SIZE>,
    pub(crate) len: u32,
}

unsafe impl Send for NicRecvNotification {}
unsafe impl Sync for NicRecvNotification {}


unsafe impl Send for BasicNicDeivce {}
unsafe impl Sync for BasicNicDeivce {}

#[derive(Debug)]
pub(crate) struct BasicNicDeivce {
    device: BlueRdmaDevice,
    tx_buf: PacketBuf<NIC_PACKET_BUFFER_SLOT_SIZE>,
    receiver: Receiver<NicRecvNotification>,
}

impl BasicNicDeivce {
    pub(crate) fn new(
        device: BlueRdmaDevice,
        tx_buf: PacketBuf<NIC_PACKET_BUFFER_SLOT_SIZE>,
        receiver: Receiver<NicRecvNotification>,
    ) -> Self {
        Self {
            device,
            tx_buf,
            receiver,
        }
    }
}

impl Device for BasicNicDeivce {
    type RxToken<'a> = NicRxToken;
    type TxToken<'a> = NicTxToken<'a> where Self: 'a;

    #[allow(clippy::indexing_slicing)]
    fn receive(&mut self, _timestamp: Instant) -> Option<(Self::RxToken<'_>, Self::TxToken<'_>)> {
        match self.receiver.try_recv() {
            Ok(mut notification) => {
                let buf = notification.buf.as_mut_slice();
                let len = notification.len as usize;
                return Some((
                    NicRxToken(&mut buf[..len]), // the length is guaranteed to be less than the buffer size
                    NicTxToken(&self.device, &self.tx_buf),
                ));
            }
            Err(TryRecvError::Disconnected) => {
                panic!("The receiver is disconnected");
            }
            Err(TryRecvError::Empty) => None,
        }
    }

    fn transmit(&mut self, _timestamp: Instant) -> Option<Self::TxToken<'_>> {
        Some(NicTxToken(&self.device, &self.tx_buf))
    }

    fn capabilities(&self) -> DeviceCapabilities {
        let mut caps = DeviceCapabilities::default();
        caps.max_transmission_unit = NIC_PACKET_BUFFER_SLOT_SIZE;
        caps.max_burst_size = None;
        caps.medium = Medium::Ethernet;
        caps
    }
}

pub(crate) struct NicRxToken(&'static mut [u8]);
pub(crate) struct NicTxToken<'a>(
    &'a BlueRdmaDevice,
    &'a PacketBuf<NIC_PACKET_BUFFER_SLOT_SIZE>,
);

impl RxToken for NicRxToken {
    fn consume<R, F>(self, f: F) -> R
    where
        F: FnOnce(&mut [u8]) -> R,
    {
        f(self.0)
    }
}

impl TxToken for NicTxToken<'_> {
    #[allow(
        clippy::unwrap_used,
        clippy::indexing_slicing,
        clippy::cast_possible_truncation
    )]
    fn consume<R, F>(self, len: usize, f: F) -> R
    where
        F: FnOnce(&mut [u8]) -> R,
    {
        let mut buf = self.1.recycle_buf();
        let ret = f(buf.as_mut_slice());
        // we get 0..6 bytes from the buffer, so it's safe to unwrap
        let mac_addr =
            MacAddress::from_bytes(&buf.as_mut_slice()[ETH_DST_START..ETH_DST_END]).unwrap();

        let total_len = len as u32; // length is guaranteed to be less than u32::MAX
        let sge = buf.into_sge(total_len);
        let common = ToCardWorkRbDescCommon {
            qp_type: QpType::RawPacket,
            total_len,
            mac_addr,
            ..Default::default()
        };
        // we don't miss any field, so it's impossible to panic
        let desc = ToCardWorkRbDescBuilder::new_write_raw()
            .with_common(common)
            .with_sge(sge)
            .build()
            .unwrap();
        if let Err(e) = self.0.send_work_desc(desc) {
            log::error!("Failed to send work desc: {:?}", e);
        }
        ret
    }
}
