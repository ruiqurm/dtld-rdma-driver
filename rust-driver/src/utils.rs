use crate::types::Pmtu;

/// Get the length of the first packet.
///
/// A buffer will be divided into multiple packets if any slice is crossed the boundary of pmtu
/// For example, if pmtu = 256 and va = 254, then the first packet can be at most 2 bytes.
/// If pmtu = 256 and va = 256, then the first packet can be at most 256 bytes.
#[inline]
#[allow(clippy::arithmetic_side_effects)]
pub(crate) fn get_first_packet_max_length(va: u64, pmtu: u32) -> u32 {
    // The offset is smaller than pmtu, which is smaller than 4096 currently.
    #[allow(clippy::cast_possible_truncation)]
    let offset = (va.wrapping_rem(u64::from(pmtu))) as u32;

    pmtu - offset
}

/// Calculate that how many packets needs
#[allow(clippy::arithmetic_side_effects)] // total_len must be greater or equal than first_pkt_len
pub(crate) fn calculate_packet_cnt(pmtu: Pmtu, raddr: u64, total_len: u32) -> u32 {
    let first_pkt_max_len = get_first_packet_max_length(raddr, u32::from(pmtu));
    let first_pkt_len = total_len.min(first_pkt_max_len);

    1 + (total_len - first_pkt_len).div_ceil(u32::from(pmtu))
}

/// convert an u8 slice to a u64
#[allow(clippy::arithmetic_side_effects)]
pub(crate) fn u8_slice_to_u64(slice: &[u8]) -> u64 {
    // this operation convert a [u8;8] to a u64. So it's safe to left shift
    slice.iter().fold(0, |a, b| (a << 8_i32) + u64::from(*b))
}
/// align page up to `PAGE_SIZE`
#[allow(clippy::arithmetic_side_effects)]
pub(crate) fn align_up<const PAGE: usize>(addr: usize) -> usize {
    (((addr) + ((PAGE) - 1)) / PAGE) * PAGE
}
