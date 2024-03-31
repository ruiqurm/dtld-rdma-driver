use crate::{
    device::{
        ToCardCtrlRbDescSge, ToCardWorkRbDesc, ToCardWorkRbDescCommon, ToCardWorkRbDescOpcode,
        ToHostWorkRbDescAethCode, ToHostWorkRbDescOpcode, ToHostWorkRbDescTransType,
    },
    types::{MemAccessTypeFlag, Psn, QpType},
};

use super::{
    logic::BlueRdmaLogicError,
    packet::{Immediate, PacketError, AETH, BTH, RDMA_PAYLOAD_ALIGNMENT, RETH},
};

/// Queue-pair number
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct Qpn(u32);

impl Qpn {
    pub fn new(qpn: u32) -> Self {
        Qpn(qpn)
    }

    pub fn get(&self) -> u32 {
        self.0
    }
}

/// Protection Domain handle
#[derive(Debug, Clone, Copy)]
pub(crate) struct PDHandle(u32);

impl PDHandle {
    pub fn new(handle: u32) -> Self {
        PDHandle(handle)
    }

    #[cfg(test)]
    pub fn get(&self) -> u32 {
        self.0
    }
}

/// The general key type, like RKey, Lkey
#[derive(Debug, Clone, Copy, Hash, PartialEq, Eq)]
pub(crate) struct Key(u32);

impl Key {
    pub fn new(key: u32) -> Self {
        Key(key)
    }

    pub fn get(&self) -> u32 {
        self.0
    }
}

impl From<crate::Key> for Key {
    fn from(key: crate::Key) -> Self {
        Key::new(key.get())
    }
}

impl From<Key> for crate::Key {
    fn from(key: Key) -> Self {
        Self::new(key.get())
    }
}

/// Partition Key
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct PKey(u16);
impl PKey {
    pub fn new(key: u16) -> Self {
        Self(key)
    }

    pub fn get(&self) -> u16 {
        self.0
    }
}

/// State of the queue pair
#[allow(dead_code)]
pub enum StateQP {
    Reset,
    Init,
    Rtr,
    Rts,
    Sqd,
    Sqe,
    Err,
    Unknown,
    Create, // Not defined in rdma-core
}

/// A abstraction of a RDMA message.
#[derive(Debug, Clone)]
pub enum Metadata {
    /// RDMA write, read request and response
    General(RdmaGeneralMeta),

    /// Acknowledge message
    Acknowledge(AethHeader),
}

impl Metadata {
    pub fn get_opcode(&self) -> ToHostWorkRbDescOpcode {
        match self {
            Metadata::General(header) => header.common_meta.opcode.clone(),
            Metadata::Acknowledge(header) => header.common_meta.opcode.clone(),
        }
    }

    pub fn common_meta(&self) -> &RdmaMessageMetaCommon {
        match self {
            Metadata::General(header) => &header.common_meta,
            Metadata::Acknowledge(header) => &header.common_meta,
        }
    }
}

/// A scatter-gather list element.
#[derive(Debug, Clone, Copy)]
pub(crate) struct SGListElement {
    pub data: *const u8,
    pub len: usize,
}

/// A payload info, which contains the scatter-gather list and the total length of the payload.
#[derive(Debug, Clone)]
pub(crate) struct PayloadInfo {
    sg_list: Vec<SGListElement>,
    total_len: usize,
}

impl PayloadInfo {
    pub fn new() -> Self {
        PayloadInfo {
            sg_list: Vec::new(),
            total_len: 0,
        }
    }

    pub fn new_with_data(data: *const u8, len: usize) -> Self {
        PayloadInfo {
            sg_list: vec![SGListElement { data, len }],
            total_len: len,
        }
    }

    #[cfg(test)]
    pub fn get_length(&self) -> usize {
        self.total_len
    }

    pub fn get_pad_cnt(&self) -> usize {
        let mut pad_cnt = (RDMA_PAYLOAD_ALIGNMENT - self.total_len % RDMA_PAYLOAD_ALIGNMENT) as u8;
        if pad_cnt as usize == RDMA_PAYLOAD_ALIGNMENT {
            pad_cnt = 0
        }
        pad_cnt as usize
    }

    pub fn with_pad_length(&self) -> usize {
        self.total_len + self.get_pad_cnt()
    }

    pub fn add(&mut self, data: *const u8, len: usize) {
        self.sg_list.push(SGListElement { data, len });
        self.total_len += len;
    }

    #[cfg(test)]
    pub fn get_sg_list(&self) -> &Vec<SGListElement> {
        &self.sg_list
    }

    pub fn copy_to(&self, mut dst: *mut u8) {
        for i in 0..self.sg_list.len() {
            unsafe {
                std::ptr::copy_nonoverlapping(self.sg_list[i].data, dst, self.sg_list[i].len);
            }
            unsafe {
                dst = dst.add(self.sg_list[i].len);
            }
        }
    }

    /// Get the first and only element of the scatter-gather list.
    /// Note that you should only use this function when you are sure that the payload only contains one element.
    pub fn direct_data_ptr(&self) -> &[u8] {
        unsafe {
            std::slice::from_raw_parts(self.sg_list[0].data, self.sg_list[0].len)
        }
    }
}

#[derive(Debug, Clone)]
pub struct RdmaMessage {
    pub meta_data: Metadata,
    pub payload: PayloadInfo,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RethHeader {
    pub va: u64,
    pub rkey: Key,
    pub len: u32,
}

impl From<&RETH> for RethHeader {
    fn from(reth: &RETH) -> Self {
        RethHeader {
            va: reth.get_va(),
            rkey: Key::new(reth.get_rkey()),
            len: reth.get_dlen(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct RdmaMessageMetaCommon {
    pub tran_type: ToHostWorkRbDescTransType,
    pub opcode: ToHostWorkRbDescOpcode,
    pub solicited: bool,
    pub pkey: PKey,
    pub dqpn: Qpn,
    pub ack_req: bool,
    pub psn: Psn,
}

impl TryFrom<&BTH> for RdmaMessageMetaCommon {
    type Error = PacketError;
    fn try_from(bth: &BTH) -> Result<Self, PacketError> {
        Ok(Self {
            tran_type: ToHostWorkRbDescTransType::try_from(bth.get_transaction_type())
                .map_err(|_| PacketError::FailedToConvertTransType)?,
            opcode: ToHostWorkRbDescOpcode::try_from(bth.get_opcode())?,
            solicited: bth.get_solicited(),
            pkey: PKey::new(bth.get_pkey()),
            dqpn: Qpn(bth.get_destination_qpn()),
            ack_req: bth.get_ack_req(),
            psn: Psn::new(bth.get_psn()),
        })
    }
}

#[derive(Debug, Clone)]
pub struct RdmaGeneralMeta {
    pub common_meta: RdmaMessageMetaCommon,
    pub reth: RethHeader,
    pub imm: Option<u32>,
    pub secondary_reth: Option<RethHeader>,
}

impl RdmaGeneralMeta {
    pub fn new_from_packet(
        bth: &BTH,
        reth: &RETH,
        imm: Option<&Immediate>,
        secondary_reth: Option<&RETH>,
    ) -> Result<Self, PacketError> {
        Ok(RdmaGeneralMeta {
            common_meta: RdmaMessageMetaCommon::try_from(bth)?,
            reth: RethHeader::from(reth),
            imm: imm.map(|v| v.get_immediate()),
            secondary_reth: secondary_reth.map(RethHeader::from),
        })
    }

    pub fn is_read_request(&self) -> bool {
        matches!(
            self.common_meta.opcode,
            ToHostWorkRbDescOpcode::RdmaReadRequest
        )
    }

    pub fn has_payload(&self) -> bool {
        matches!(
            self.common_meta.opcode,
            ToHostWorkRbDescOpcode::RdmaWriteFirst
                | ToHostWorkRbDescOpcode::RdmaWriteMiddle
                | ToHostWorkRbDescOpcode::RdmaWriteLast
                | ToHostWorkRbDescOpcode::RdmaWriteLastWithImmediate
                | ToHostWorkRbDescOpcode::RdmaWriteOnly
                | ToHostWorkRbDescOpcode::RdmaWriteOnlyWithImmediate
                | ToHostWorkRbDescOpcode::RdmaReadResponseFirst
                | ToHostWorkRbDescOpcode::RdmaReadResponseMiddle
                | ToHostWorkRbDescOpcode::RdmaReadResponseLast
                | ToHostWorkRbDescOpcode::RdmaReadResponseOnly
        )
    }

    pub fn needed_permissions(&self) -> MemAccessTypeFlag {
        if self.has_payload() {
            MemAccessTypeFlag::IbvAccessRemoteWrite
        } else if self.is_read_request() {
            MemAccessTypeFlag::IbvAccessRemoteRead
        } else {
            MemAccessTypeFlag::IbvAccessNoFlags
        }
    }
}
#[derive(Debug, Clone)]
pub struct AethHeader {
    pub common_meta: RdmaMessageMetaCommon,
    pub aeth_code: ToHostWorkRbDescAethCode,
    pub aeth_value: u8,
    pub msn: u32,
}

impl AethHeader {
    pub fn new_from_packet(bth: &BTH, aeth: &AETH) -> Result<Self, PacketError> {
        let aeth_code = ToHostWorkRbDescAethCode::try_from(aeth.get_aeth_code())?;
        let aeth_value = aeth.get_aeth_value();
        let msn = aeth.get_msn();

        Ok(AethHeader {
            common_meta: RdmaMessageMetaCommon::try_from(bth)?,
            aeth_code,
            aeth_value,
            msn,
        })
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct SGListElementWithKey {
    pub(crate) addr: u64,
    pub(crate) len: u32,
    pub(crate) key: Key,
}

impl Default for SGListElementWithKey {
    fn default() -> Self {
        SGListElementWithKey {
            addr: 0,
            len: 0,
            key: Key::new(0),
        }
    }
}

// impl SGListElementWithKey {
//     /// Cut a buffer of length from a scatter-gather element
//     pub(crate) fn cut(&mut self, length: u32) -> Result<PayloadInfo, BlueRdmaLogicError> {
//         let mut payload = PayloadInfo::new();
//         if self.len >= length {
//             let addr = self.addr as *mut u8;
//             payload.add(addr, length as usize);
//             self.addr += length as u64;
//             self.len -= length;
//             return Ok(payload);
//         }
//         Err(BlueRdmaLogicError::Unreachable)
//     }
// }

impl From<ToCardCtrlRbDescSge> for SGListElementWithKey {
    fn from(sge: ToCardCtrlRbDescSge) -> Self {
        SGListElementWithKey {
            addr: sge.addr,
            len: sge.len,
            key: Key::new(sge.key.get()),
        }
    }
}

#[derive(Debug)]
pub struct SGList {
    pub data: [SGListElementWithKey; 4],
    pub cur_level: u32,
    pub len: u32,
}

impl SGList {
    #[allow(dead_code)]
    pub fn new() -> Self {
        SGList {
            data: [SGListElementWithKey::default(); 4],
            cur_level: 0,
            len: 0,
        }
    }

    pub fn new_with_sge(sge: ToCardCtrlRbDescSge) -> Self {
        SGList {
            data: [
                SGListElementWithKey::from(sge),
                SGListElementWithKey::default(),
                SGListElementWithKey::default(),
                SGListElementWithKey::default(),
            ],
            cur_level: 0,
            len: 1,
        }
    }

    pub fn get_total_length(&self) -> u32 {
        self.data.iter().map(|sge| sge.len).sum()
    }

    fn get_sge_from_option(sge: Option<ToCardCtrlRbDescSge>) -> (SGListElementWithKey, u32) {
        match sge {
            Some(sge) => (SGListElementWithKey::from(sge), 1),
            None => (SGListElementWithKey::default(), 0),
        }
    }

    pub fn new_with_sge_list(
        sge0: ToCardCtrlRbDescSge,
        sge1: Option<ToCardCtrlRbDescSge>,
        sge2: Option<ToCardCtrlRbDescSge>,
        sge3: Option<ToCardCtrlRbDescSge>,
    ) -> Self {
        let sge0 = SGListElementWithKey::from(sge0);
        let mut counter = 1;
        let (sge1, sge1_counter) = Self::get_sge_from_option(sge1);
        counter += sge1_counter;
        let (sge2, sge2_counter) = Self::get_sge_from_option(sge2);
        counter += sge2_counter;
        let (sge3, sge3_counter) = Self::get_sge_from_option(sge3);
        counter += sge3_counter;
        SGList {
            data: [sge0, sge1, sge2, sge3],
            cur_level: 0,
            len: counter,
        }
    }

    /// Cut a buffer of length from the scatter-gather list
    ///
    /// The function iterate from `cur_level` of the scatter-gather list and cut the buffer of `length` from the list.
    /// If current level is not enough, it will move to the next level.
    /// All the slice will be added to the `payload`.
    pub(crate) fn cut(&mut self, mut length: u32) -> Result<PayloadInfo, BlueRdmaLogicError> {
        let mut current_level = self.cur_level as usize;
        let mut payload = PayloadInfo::new();
        while (current_level as u32) < self.len {
            if self.data[current_level].len >= length {
                let addr = self.data[current_level].addr as *mut u8;
                payload.add(addr, length as usize);
                self.data[current_level].addr += length as u64;
                self.data[current_level].len -= length;
                if self.data[current_level].len == 0 {
                    current_level += 1;
                    self.cur_level = current_level as u32;
                }
                return Ok(payload);
            } else {
                // check next level
                let addr = self.data[current_level].addr as *mut u8;
                payload.add(addr, self.data[current_level].len as usize);
                length -= self.data[current_level].len;
                self.data[current_level].len = 0;
                current_level += 1;
            }
        }
        Err(BlueRdmaLogicError::Unreachable)
    }

    pub(crate) fn cut_all_levels(&mut self) -> PayloadInfo {
        let mut payload = PayloadInfo::new();
        for i in 0..self.len as usize {
            let addr = self.data[i].addr as *mut u8;
            let length = self.data[i].len as usize;
            payload.add(addr, length);
            self.data[i].len = 0;
        }
        payload
    }

    #[cfg(test)]
    pub fn into_four_sges(
        self,
    ) -> (
        ToCardCtrlRbDescSge,
        Option<ToCardCtrlRbDescSge>,
        Option<ToCardCtrlRbDescSge>,
        Option<ToCardCtrlRbDescSge>,
    ) {
        use crate::types::Key;

        let sge1 = (self.len > 1).then(|| ToCardCtrlRbDescSge {
            addr: self.data[1].addr,
            len: self.data[1].len,
            key: Key::new(self.data[1].key.get()),
        });

        let sge2 = (self.len > 2).then(|| ToCardCtrlRbDescSge {
            addr: self.data[2].addr,
            len: self.data[2].len,
            key: Key::new(self.data[2].key.get()),
        });

        let sge3 = (self.len > 3).then(|| ToCardCtrlRbDescSge {
            addr: self.data[3].addr,
            len: self.data[3].len,
            key: Key::new(self.data[3].key.get()),
        });
        (
            ToCardCtrlRbDescSge {
                addr: self.data[0].addr,
                len: self.data[0].len,
                key: Key::new(self.data[0].key.get()),
            },
            sge1,
            sge2,
            sge3,
        )
    }
}

#[derive(Debug)]
pub(crate) enum ToCardDescriptor {
    Write(ToCardWriteDescriptor),
    Read(ToCardReadDescriptor),
}

impl ToCardDescriptor {
    pub fn is_raw_packet(&self) -> bool {
        match self {
            ToCardDescriptor::Write(desc) => {
                matches!(desc.opcode, ToCardWorkRbDescOpcode::Write)
                    && matches!(desc.common.qp_type, QpType::RawPacket)
            }
            ToCardDescriptor::Read(_) => false,
        }
    }

    pub fn common(&self) -> &ToCardWorkRbDescCommon {
        match self {
            ToCardDescriptor::Write(desc) => &desc.common,
            ToCardDescriptor::Read(desc) => &desc.common,
        }
    }

    pub fn first_sge_mut(&mut self) -> &mut SGList {
        match self {
            ToCardDescriptor::Write(desc) => &mut desc.sg_list,
            ToCardDescriptor::Read(desc) => &mut desc.sge,
        }
    }
}

#[derive(Debug)]
pub(crate) struct ToCardWriteDescriptor {
    pub(crate) opcode: ToCardWorkRbDescOpcode,
    pub(crate) common: ToCardWorkRbDescCommon,
    pub(crate) imm: Option<u32>,
    pub(crate) is_first: bool,
    pub(crate) is_last: bool,
    pub(crate) sg_list: SGList,
}

impl ToCardWriteDescriptor {
    pub fn write_only_opcode_with_imm(&self) -> (ToHostWorkRbDescOpcode, Option<u32>) {
        if self.is_first && self.is_last {
            // is_first = True and is_last = True, means only one packet
            match (self.is_resp(), self.has_imm()) {
                (true, true) => (ToHostWorkRbDescOpcode::RdmaReadResponseOnly, None),
                (true, false) => (ToHostWorkRbDescOpcode::RdmaReadResponseOnly, None),
                (false, true) => (ToHostWorkRbDescOpcode::RdmaWriteOnlyWithImmediate, self.imm),
                (false, false) => (ToHostWorkRbDescOpcode::RdmaWriteOnly, None),
            }
        } else if self.is_first {
            // self.is_last = False
            if self.is_resp() {
                (ToHostWorkRbDescOpcode::RdmaReadResponseFirst, None)
            } else {
                (ToHostWorkRbDescOpcode::RdmaWriteFirst, None)
            }
        } else {
            // self.is_last = True
            match (self.is_resp(), self.has_imm()) {
                (true, true) => (ToHostWorkRbDescOpcode::RdmaReadResponseLast, None), // ignore
                (true, false) => (ToHostWorkRbDescOpcode::RdmaReadResponseLast, None),
                (false, true) => (ToHostWorkRbDescOpcode::RdmaWriteLastWithImmediate, self.imm),
                (false, false) => (ToHostWorkRbDescOpcode::RdmaWriteLast, None),
            }
        }
    }

    pub fn write_first_opcode(&self) -> ToHostWorkRbDescOpcode {
        match (self.is_first, self.is_resp()) {
            (true, true) => ToHostWorkRbDescOpcode::RdmaReadResponseFirst,
            (true, false) => ToHostWorkRbDescOpcode::RdmaWriteFirst,
            (false, true) => ToHostWorkRbDescOpcode::RdmaReadResponseMiddle,
            (false, false) => ToHostWorkRbDescOpcode::RdmaWriteMiddle,
        }
    }

    pub fn write_middle_opcode(&self) -> ToHostWorkRbDescOpcode {
        if self.is_resp() {
            ToHostWorkRbDescOpcode::RdmaReadResponseMiddle
        } else {
            ToHostWorkRbDescOpcode::RdmaWriteMiddle
        }
    }

    pub fn write_last_opcode_with_imm(&self) -> (ToHostWorkRbDescOpcode, Option<u32>) {
        match (self.is_last, self.is_resp(), self.has_imm()) {
            (true, true, true) => (ToHostWorkRbDescOpcode::RdmaReadResponseLast, None), // ignore
            (true, true, false) => (ToHostWorkRbDescOpcode::RdmaReadResponseLast, None),
            (true, false, true) => (ToHostWorkRbDescOpcode::RdmaWriteLastWithImmediate, self.imm),
            (true, false, false) => (ToHostWorkRbDescOpcode::RdmaWriteLast, None),
            (false, true, true) => (ToHostWorkRbDescOpcode::RdmaReadResponseMiddle, None),
            (false, true, false) => (ToHostWorkRbDescOpcode::RdmaReadResponseMiddle, None),
            (false, false, true) => (ToHostWorkRbDescOpcode::RdmaWriteMiddle, None),
            (false, false, false) => (ToHostWorkRbDescOpcode::RdmaWriteMiddle, None),
        }
    }
    
    pub fn is_resp(&self) -> bool {
        matches!(self.opcode, ToCardWorkRbDescOpcode::ReadResp)
    }

    pub fn has_imm(&self) -> bool {
        self.imm.is_some()
    }
}

#[derive(Debug)]
pub(crate) struct ToCardReadDescriptor {
    pub(crate) common: ToCardWorkRbDescCommon,
    pub(crate) sge: SGList,
}

impl From<ToCardWorkRbDesc> for ToCardDescriptor {
    fn from(desc: ToCardWorkRbDesc) -> Self {
        match desc {
            ToCardWorkRbDesc::Write(desc) => ToCardDescriptor::Write(ToCardWriteDescriptor {
                opcode: ToCardWorkRbDescOpcode::Write,
                common: desc.common,
                is_first: desc.is_first,
                is_last: desc.is_last,
                imm: None,
                sg_list: SGList::new_with_sge_list(desc.sge0, desc.sge1, desc.sge2, desc.sge3),
            }),
            ToCardWorkRbDesc::Read(desc) => ToCardDescriptor::Read(ToCardReadDescriptor {
                common: desc.common,
                sge: SGList::new_with_sge(desc.sge),
            }),
            ToCardWorkRbDesc::WriteWithImm(desc) => {
                ToCardDescriptor::Write(ToCardWriteDescriptor {
                    opcode: ToCardWorkRbDescOpcode::WriteWithImm,
                    common: desc.common,
                    is_first: desc.is_first,
                    is_last: desc.is_last,
                    imm: Some(desc.imm),
                    sg_list: SGList::new_with_sge_list(desc.sge0, desc.sge1, desc.sge2, desc.sge3),
                })
            }
            ToCardWorkRbDesc::ReadResp(desc) => ToCardDescriptor::Write(ToCardWriteDescriptor {
                opcode: ToCardWorkRbDescOpcode::ReadResp,
                common: desc.common,
                is_first: desc.is_first,
                is_last: desc.is_last,
                imm: None,
                sg_list: SGList::new_with_sge_list(desc.sge0, desc.sge1, desc.sge2, desc.sge3),
            }),
        }
    }
}

impl From<&QpType> for ToHostWorkRbDescTransType{
    fn from(value: &QpType) -> Self {
        match value {
            QpType::RawPacket => ToHostWorkRbDescTransType::Rc,
            QpType::Rc => ToHostWorkRbDescTransType::Rc,
            QpType::Ud => ToHostWorkRbDescTransType::Ud,
            QpType::Uc => ToHostWorkRbDescTransType::Uc,
            QpType::XrcRecv => ToHostWorkRbDescTransType::Xrc,
            QpType::XrcSend => ToHostWorkRbDescTransType::Xrc,
        }
    }
}