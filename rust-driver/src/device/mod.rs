
/// The layout of a desciptor in ringbuf
mod descriptor;

/// Some constants of the device
mod constant;

/// default hardware page size is 2MB.
pub const PAGE_SIZE: usize = 1024 * 1024 * 2;