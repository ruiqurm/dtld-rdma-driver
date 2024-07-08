use std::error::Error;

use thiserror::Error;


/// The layout of a desciptor in ringbuf
mod descriptor;

/// Some constants of the device
mod constant;

/// A ring buffer for interaction between the driver and the hardware device
mod ringbuf;

/// default hardware page size is 2MB.
pub const PAGE_SIZE: usize = 1024 * 1024 * 2;

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