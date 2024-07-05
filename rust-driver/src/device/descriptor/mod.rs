use std::error::Error;

use thiserror::Error;

/// ctrl descriptor
pub(crate) mod ctrl;

/// work descriptor
pub(crate) mod work;

/// layout of a descriptor
mod layout;

/// convert an u8 slice to a u64
#[allow(clippy::arithmetic_side_effects)]
fn u8_slice_to_u64(slice: &[u8]) -> u64 {
    // this operation convert a [u8;8] to a u64. So it's safe to left shift
    slice
        .iter()
        .take(8)
        .fold(0, |a, b| (a << 8_i32) + u64::from(*b))
}

#[derive(Debug, Error)]
pub(crate) enum DeviceError {
    #[error("device error : {0}")]
    Device(Box<dyn Error>),
    #[error("scheduler : {0}")]
    Scheduler(String),
    #[error("parse descriptor error : {0}")]
    ParseDesc(String),
    #[error("Operation timeout")]
    Timeout,
}

#[cfg(test)]
mod tests {
    use super::u8_slice_to_u64;

    #[test]
    fn test_u8_slice_to_u64() {
        let input = [0x2, 0x3, 0x4, 0x5];
        assert_eq!(u8_slice_to_u64(&input), 0x02030405);
        let input = [0x2, 0x3, 0x4, 0x5, 0x06, 0x07];
        assert_eq!(u8_slice_to_u64(&input), 0x020304050607);
        let input = [0x2, 0x3, 0x4, 0x5, 0x06, 0x07, 0x08, 0x09];
        assert_eq!(u8_slice_to_u64(&input), 0x0203040506070809);
        let input = [0x2, 0x3, 0x4, 0x5, 0x06, 0x07, 0x08, 0x09, 0x10];
        assert_eq!(u8_slice_to_u64(&input), 0x0203040506070809);
    }
}
