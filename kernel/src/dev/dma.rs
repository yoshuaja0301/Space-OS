//! Memory a device reads and writes directly.
//!
//! One 4 KiB frame from the frame allocator, zeroed, reached by the CPU through the
//! linear map. x86 keeps DMA coherent with the caches, so the write-back linear map
//! is fine for it; drivers still order their descriptor writes with fences before
//! telling the device.

use spaceabi::error::Error;

use crate::mm::{frame, phys_to_virt};

/// One page of DMA memory, addressable by both the CPU and the device. It lives as
/// long as the driver that owns it (drivers are never unloaded).
pub struct DmaPage {
    pub phys: u64,
    pub virt: u64,
}

impl DmaPage {
    pub fn new() -> Result<Self, Error> {
        let f = frame::alloc_zeroed().ok_or(Error::NoMemory)?;
        let phys = f.start_address().as_u64();
        Ok(DmaPage { phys, virt: phys_to_virt(phys).as_u64() })
    }
}
