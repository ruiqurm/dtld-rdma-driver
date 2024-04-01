use std::net::Ipv4Addr;

use bitflags::bitflags;
use derive_builder::Builder;
use eui48::MacAddress;
use num_enum::TryFromPrimitive;
use serde::ser::StdError;
use thiserror::Error;

use crate::Pd;

pub const PAGE_SIZE: usize = 1024 * 1024 * 2;

/// Type for `Imm`
#[derive(Debug, Clone, Copy, Hash)]
pub struct Imm(u32);
impl Imm {
    #[must_use]
    pub fn new(imm: u32) -> Self {
        Self(imm)
    }

    #[must_use]
    pub fn get(&self) -> u32 {
        self.0
    }

    #[must_use]
    pub fn into_be(self) -> u32 {
        self.0.to_be()
    }

    #[must_use]
    pub fn from_be(val: u32) -> Self {
        Self::new(val.to_le())
    }
}

impl From<u32> for Imm {
    fn from(imm: u32) -> Self {
        Self::new(imm)
    }
}

/// Message Sequence Number
#[derive(Debug, Clone, Copy, Hash, PartialEq, Eq)]
pub struct Msn(u16);
impl Msn {
    #[must_use]
    pub fn new(msn: u16) -> Self {
        Self(msn)
    }

    #[must_use]
    pub fn get(&self) -> u16 {
        self.0
    }

    #[must_use]
    pub fn into_be(self) -> u16 {
        self.0.to_be()
    }
}

impl From<u16> for Msn {
    fn from(msn: u16) -> Self {
        Self::new(msn)
    }
}

impl Default for Msn {
    fn default() -> Self {
        Self::new(0)
    }
}

/// `RKey` and `LKey`
#[derive(Debug, Clone, Copy, Hash, PartialEq, Eq, Default)]
pub struct Key(u32);
impl Key {
    #[must_use]
    pub fn new(key: u32) -> Self {
        Self(key)
    }

    #[must_use]
    pub fn get(&self) -> u32 {
        self.0
    }

    #[must_use]
    pub fn into_be(self) -> u32 {
        self.0.to_be()
    }

    #[must_use]
    pub fn from_be(val: u32) -> Self {
        // the val is already in big endian
        // So we need to convert it to little endian, use `to_be()`
        Self::new(val.to_be())
    }
}

impl From<u32> for Key {
    fn from(key: u32) -> Self {
        Self::new(key)
    }
}

/// Packet Sequence Number
pub type Psn = ThreeBytesStruct;

/// Queue Pair Number
pub type Qpn = ThreeBytesStruct;

#[derive(Debug, Clone, Copy, Hash, PartialEq, Eq, Default)]
pub struct ThreeBytesStruct(u32);

impl ThreeBytesStruct {
    const WIDTH: usize = 24;
    const MASK: u32 = u32::MAX >> (32 - Self::WIDTH);
    const MAX: u32 = Self::MASK + 1;

    #[must_use]
    pub fn new(key: u32) -> Self {
        Self(key & Self::MASK)
    }

    #[must_use]
    pub fn get(&self) -> u32 {
        self.0
    }

    #[must_use]
    pub fn into_be(self) -> u32 {
        // In little endian machine, to_le_bytes() is a no-op. Just get the layout.
        let key = self.0.to_le_bytes();
        // Then we reoder the bytes to big endian
        // Note that the last byte is exceed the 24 bits, any value in it will be ignored
        u32::from_le_bytes([key[2], key[1], key[0], 0])
    }

    #[must_use]
    pub fn from_be(val: u32) -> Self {
        // get the layout.
        let key = val.to_le_bytes();
        // from_le_bytes is also a no-op in little endian machine.
        // We just use it to convert from [u8;4] to `u32`.
        Self::new(u32::from_le_bytes([key[2], key[1], key[0], 0]))
    }

    #[must_use]
    pub fn wrapping_add(&self, rhs: u32) -> Self {
        Self((self.0 + rhs) % Self::MAX)
    }

    #[must_use]
    pub fn wrapping_sub(&self, rhs: u32) -> Self {
        let rhs = rhs % Self::MAX;
        if self.0 > rhs {
            Self(self.0 - rhs)
        } else {
            Self(Self::MAX - rhs + self.0)
        }
    }

    /// The absolute difference between two PSN
    /// We assume that the bigger PSN should not exceed the
    /// smaller PSN by more than 2^23(that half of the range)
    #[must_use]
    pub fn wrapping_abs(&self, rhs: Psn) -> u32 {
        if self.0 >= rhs.0 {
            self.0 - rhs.get()
        } else {
            self.0 + Self::MAX - rhs.0
        }
    }
}

impl From<u32> for ThreeBytesStruct {
    fn from(key: u32) -> Self {
        Self::new(key)
    }
}

bitflags! {
    #[derive(Debug,Clone,Copy)]
    pub struct MemAccessTypeFlag: u8 {
        const IbvAccessNoFlags = 0;      // Not defined in rdma-core
        const IbvAccessLocalWrite = 1;   // (1 << 0)
        const IbvAccessRemoteWrite = 2;  // (1 << 1)
        const IbvAccessRemoteRead = 4;   // (1 << 2)
        const IbvAccessRemoteAtomic = 8; // (1 << 3)
        const IbvAccessMwBind = 16;      // (1 << 4)
        const IbvAccessZeroBased = 32;   // (1 << 5)
        const IbvAccessOnDemand = 64;    // (1 << 6)
        const IbvAccessHugetlb = 128;    // (1 << 7)
                                   // IbvAccessRelaxedOrdering   = IBV_ACCESS_OPTIONAL_FIRST,
    }
}

#[non_exhaustive]
#[derive(TryFromPrimitive, Debug, Clone, Copy)]
#[repr(u8)]
pub enum QpType {
    Rc = 2,
    Uc = 3,
    Ud = 4,
    RawPacket = 8,
    XrcSend = 9,
    XrcRecv = 10,
}

#[non_exhaustive]
#[derive(Debug, Clone, Copy)]
pub enum Pmtu {
    Mtu256 = 1,
    Mtu512 = 2,
    Mtu1024 = 3,
    Mtu2048 = 4,
    Mtu4096 = 5,
}

impl From<&Pmtu> for u64 {
    fn from(pmtu: &Pmtu) -> u64 {
        match pmtu {
            Pmtu::Mtu256 => 256,
            Pmtu::Mtu512 => 512,
            Pmtu::Mtu1024 => 1024,
            Pmtu::Mtu2048 => 2048,
            Pmtu::Mtu4096 => 4096,
        }
    }
}

impl From<&Pmtu> for u32 {
    fn from(pmtu: &Pmtu) -> u32 {
        match pmtu {
            Pmtu::Mtu256 => 256,
            Pmtu::Mtu512 => 512,
            Pmtu::Mtu1024 => 1024,
            Pmtu::Mtu2048 => 2048,
            Pmtu::Mtu4096 => 4096,
        }
    }
}

#[non_exhaustive]
#[derive(Debug, Clone, Copy)]
pub struct Sge {
    pub addr: u64,
    pub len: u32,
    pub key: Key,
}

#[derive(Debug, Builder, Clone, Copy)]
#[non_exhaustive]
pub struct RdmaDeviceNetwork {
    pub gateway: Ipv4Addr,
    pub netmask: Ipv4Addr,
    pub ipaddr: Ipv4Addr,
    pub macaddr: MacAddress,
}

#[non_exhaustive]
#[derive(Builder, Debug, Clone, Copy)]
pub struct Qp {
    pub pd: Pd,
    pub qpn: Qpn,
    pub qp_type: QpType,
    pub rq_acc_flags: MemAccessTypeFlag,
    pub pmtu: Pmtu,
    pub dqp_ip: Ipv4Addr,
    pub dqp_mac: MacAddress,
}

#[non_exhaustive]
#[derive(Debug, Error)]
pub enum Error {
    #[error(transparent)]
    Device(Box<dyn StdError>),
    #[error("init failed: {0}")]
    DoubleInit(String),
    #[error("device busy")]
    DeviceBusy,
    #[error("device return failed")]
    DeviceReturnFailed,
    #[error("QP busy")]
    QpBusy,
    #[error("invalid {0}")]
    Invalid(String),
    #[error("PD in use :{0}")]
    PdInUse(String),
    #[error("Failed to insert to map {0} : key {1} already exist")]
    InsertFailed(&'static str, String),
    #[error("no available resource : {0}")]
    ResourceNoAvailable(String),
    #[error("allocate page table failed")]
    AllocPageTable,
    #[error("build descriptor failed, lack of `{0}`")]
    BuildDescFailed(&'static str),
    #[error("In ctrl, set network param failed")]
    SetNetworkParamFailed,
    #[error("Mutex lock {0} poisoned")]
    LockPoisoned(&'static str),
    #[error("Address of {0} is not aligned,which is {1:x}")]
    AddressNotAlign(&'static str, usize),
    #[error("MSN exist, create operation context failed")]
    CreateOpCtxFailed,
    #[error("Set context result failed")]
    SetCtxResultFailed,
    #[error("Context op id {0} have been used")]
    OpIdUsed(u32),
    #[error("Get physical address failed:{0}")]
    GetPhysAddrFailed(String),
    #[error("Not support environment : {0}")]
    NotSupport(&'static str),
}

#[cfg(test)]
mod tests {
    use crate::types::Psn;
    use std::slice::from_raw_parts;

    #[test]
    fn test_wrapping_add() {
        let psn = Psn::new(0xffffff);
        let ret = psn.wrapping_add(1);
        assert_eq!(0, ret.get());

        let ret = psn.wrapping_add(2);
        assert_eq!(ret.get(), 1);

        let ret = psn.wrapping_add(0xffffff);
        assert_eq!(ret.get(), 0xffffff - 1);
    }

    #[test]
    fn test_to_be() {
        let psn = Psn::new(0x123456);
        let mem = psn.into_be();
        let buf = unsafe { from_raw_parts(&mem as *const _ as *const u8, 4) };
        assert_eq!(buf, &[0x12, 0x34, 0x56, 0]);
        assert_eq!(Psn::from_be(mem).get(), 0x123456);

        let key = crate::types::Key::new(0x12345678);
        let mem = key.into_be();
        let buf = unsafe { from_raw_parts(&mem as *const _ as *const u8, 4) };
        assert_eq!(buf, &[0x12, 0x34, 0x56, 0x78]);
        assert_eq!(crate::types::Key::from_be(mem).get(), 0x12345678);
    }

    #[test]
    fn test_wrapping_abs() {
        let psn = Psn::new(0);
        let psn2 = psn.wrapping_sub(1);
        assert_eq!(psn2.get(), 0xffffff);

        let psn = psn.wrapping_abs(psn2);
        assert_eq!(psn, 1);

        // psn greater than 2**24
        let psn = Psn::new(0x123456);
        let psn2 = psn.wrapping_sub(0x12345678);
        assert_eq!(psn2.get(), psn.wrapping_sub(0x345678).get());
    }
}
