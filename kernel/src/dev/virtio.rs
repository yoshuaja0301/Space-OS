//! Shared pieces of the VirtIO 1.0 PCI transport: finding the modern capability
//! structures, reset and feature negotiation, and split virtqueues that live in one
//! DMA page each.
//!
//! The block driver predates this module and keeps its own copy of the transport
//! code; new drivers use this one.

use core::sync::atomic::{Ordering, fence};

use spaceabi::error::Error;
use x86_64::structures::paging::PhysFrame;

use crate::dev::pci;
use crate::mm::{PAGE_SIZE, frame, mmio, phys_to_virt};

pub const VIRTIO_VENDOR: u16 = 0x1AF4;

const CAP_VENDOR: u8 = 0x09;
const CFG_COMMON: u8 = 1;
const CFG_NOTIFY: u8 = 2;
const CFG_DEVICE: u8 = 4;

// virtio_pci_common_cfg field offsets (virtio 1.0, 4.1.4.3).
const CC_DEVICE_FEATURE_SELECT: u64 = 0x00;
const CC_DEVICE_FEATURE: u64 = 0x04;
const CC_DRIVER_FEATURE_SELECT: u64 = 0x08;
const CC_DRIVER_FEATURE: u64 = 0x0C;
const CC_NUM_QUEUES: u64 = 0x12;
const CC_DEVICE_STATUS: u64 = 0x14;
const CC_QUEUE_SELECT: u64 = 0x16;
const CC_QUEUE_SIZE: u64 = 0x18;
const CC_QUEUE_ENABLE: u64 = 0x1C;
const CC_QUEUE_NOTIFY_OFF: u64 = 0x1E;
const CC_QUEUE_DESC: u64 = 0x20;
const CC_QUEUE_DRIVER: u64 = 0x28;
const CC_QUEUE_DEVICE: u64 = 0x30;

pub const STATUS_ACKNOWLEDGE: u8 = 1;
pub const STATUS_DRIVER: u8 = 2;
pub const STATUS_DRIVER_OK: u8 = 4;
pub const STATUS_FEATURES_OK: u8 = 8;

/// Feature bit 32 (bit 0 of word 1): the device speaks virtio 1.0, not legacy.
pub const F_VERSION_1: u32 = 1 << 0;

/// The device writes into this buffer (as opposed to reading it).
pub const DESC_F_WRITE: u16 = 2;
/// Avail ring flag: "do not interrupt me". Drivers here poll the used ring.
const AVAIL_F_NO_INTERRUPT: u16 = 1;

/// Spins before a reset that never completes is treated as a dead device.
const RESET_SPIN_LIMIT: u32 = 10_000_000;

pub fn mmio_read<T: Copy>(addr: u64) -> T {
    // SAFETY: callers pass addresses inside a device mapping created by `mmio::map`.
    unsafe { core::ptr::read_volatile(addr as *const T) }
}

pub fn mmio_write<T: Copy>(addr: u64, value: T) {
    // SAFETY: as above.
    unsafe { core::ptr::write_volatile(addr as *mut T, value) }
}

/// One 4 KiB page of DMA memory, addressable by both CPU and device.
pub struct DmaPage {
    pub phys: u64,
    pub virt: u64,
}

impl DmaPage {
    pub fn new() -> Result<Self, Error> {
        let f: PhysFrame = frame::alloc_zeroed().ok_or(Error::NoMemory)?;
        let phys = f.start_address().as_u64();
        Ok(DmaPage { phys, virt: phys_to_virt(phys).as_u64() })
    }
}

/// The mapped configuration structures of one modern virtio PCI function.
pub struct Transport {
    pub common: u64,
    notify: u64,
    notify_multiplier: u32,
    /// Device-specific configuration, 0 when the device exposes none.
    pub device_cfg: u64,
}

impl Transport {
    /// Map the modern capability structures of `addr`. `Ok(None)` when the function
    /// only speaks legacy virtio (no common or notify structure).
    pub fn probe(addr: pci::Address, device_cfg_len: u64) -> Result<Option<Transport>, Error> {
        let mut common = 0u64;
        let mut notify = 0u64;
        let mut notify_multiplier = 0u32;
        let mut device_cfg = 0u64;
        for (id, off) in addr.capabilities() {
            if id != CAP_VENDOR {
                continue;
            }
            let cfg_type = addr.read8(off + 3);
            let bar = addr.read8(off + 4);
            let cap_offset = addr.read32(off + 8) as u64;
            let cap_len = addr.read32(off + 12) as u64;
            let Some(bar_base) = addr.bar_address(bar) else { continue };
            match cfg_type {
                CFG_COMMON if common == 0 => common = mmio::map(bar_base + cap_offset, cap_len.max(0x38))?,
                CFG_NOTIFY if notify == 0 => {
                    notify_multiplier = addr.read32(off + 16);
                    notify = mmio::map(bar_base + cap_offset, cap_len.max(4))?;
                }
                CFG_DEVICE if device_cfg == 0 && device_cfg_len > 0 => {
                    device_cfg = mmio::map(bar_base + cap_offset, cap_len.max(device_cfg_len))?
                }
                _ => {}
            }
        }
        if common == 0 || notify == 0 {
            return Ok(None);
        }
        Ok(Some(Transport { common, notify, notify_multiplier, device_cfg }))
    }

    pub fn status(&self) -> u8 {
        mmio_read::<u8>(self.common + CC_DEVICE_STATUS)
    }

    pub fn set_status(&self, s: u8) {
        mmio_write::<u8>(self.common + CC_DEVICE_STATUS, s);
    }

    /// Reset the device. `false` when it never reports the reset as done.
    pub fn reset(&self) -> bool {
        self.set_status(0);
        let mut spins = 0u32;
        while self.status() != 0 {
            spins += 1;
            if spins > RESET_SPIN_LIMIT {
                return false;
            }
            core::hint::spin_loop();
        }
        true
    }

    /// Announce the driver and agree on features. The driver accepts, from what the
    /// device offers, only the bits in `want_lo` (word 0) plus `VIRTIO_F_VERSION_1`,
    /// which it requires. Returns the accepted word 0, or `None` when the device
    /// refused or does not speak virtio 1.0.
    pub fn negotiate(&self, want_lo: u32) -> Option<u32> {
        self.set_status(STATUS_ACKNOWLEDGE);
        self.set_status(STATUS_ACKNOWLEDGE | STATUS_DRIVER);
        mmio_write::<u32>(self.common + CC_DEVICE_FEATURE_SELECT, 1);
        let offered_hi = mmio_read::<u32>(self.common + CC_DEVICE_FEATURE);
        if offered_hi & F_VERSION_1 == 0 {
            return None;
        }
        mmio_write::<u32>(self.common + CC_DEVICE_FEATURE_SELECT, 0);
        let offered_lo = mmio_read::<u32>(self.common + CC_DEVICE_FEATURE);
        let accepted_lo = offered_lo & want_lo;
        mmio_write::<u32>(self.common + CC_DRIVER_FEATURE_SELECT, 0);
        mmio_write::<u32>(self.common + CC_DRIVER_FEATURE, accepted_lo);
        mmio_write::<u32>(self.common + CC_DRIVER_FEATURE_SELECT, 1);
        mmio_write::<u32>(self.common + CC_DRIVER_FEATURE, F_VERSION_1);
        self.set_status(STATUS_ACKNOWLEDGE | STATUS_DRIVER | STATUS_FEATURES_OK);
        if self.status() & STATUS_FEATURES_OK == 0 {
            return None;
        }
        Some(accepted_lo)
    }

    pub fn num_queues(&self) -> u16 {
        mmio_read::<u16>(self.common + CC_NUM_QUEUES)
    }

    /// Set up queue `index` with at most `max` entries. The size is the largest power
    /// of two that both sides accept; `Ok(None)` when the queue does not exist or
    /// would be smaller than `min`.
    pub fn setup_queue(&self, index: u16, max: u16, min: u16) -> Result<Option<SplitQueue>, Error> {
        mmio_write::<u16>(self.common + CC_QUEUE_SELECT, index);
        let device_max = mmio_read::<u16>(self.common + CC_QUEUE_SIZE);
        if device_max == 0 {
            return Ok(None);
        }
        let wanted = max.min(device_max);
        if wanted == 0 {
            return Ok(None);
        }
        // Largest power of two not above `wanted`.
        let size: u16 = 1 << (15 - wanted.leading_zeros());
        if size < min {
            return Ok(None);
        }
        mmio_write::<u16>(self.common + CC_QUEUE_SIZE, size);
        // The device may clamp further; what it reports back is what it will use.
        let size = mmio_read::<u16>(self.common + CC_QUEUE_SIZE);
        if size < min || size > max || !size.is_power_of_two() {
            return Ok(None);
        }
        let page = DmaPage::new()?;
        let desc_off = 0u64;
        let avail_off = 16 * size as u64;
        let used_off = (avail_off + 6 + 2 * size as u64).div_ceil(4) * 4;
        if used_off + 6 + 8 * size as u64 > PAGE_SIZE {
            return Ok(None);
        }
        let notify_off = mmio_read::<u16>(self.common + CC_QUEUE_NOTIFY_OFF);
        mmio_write::<u64>(self.common + CC_QUEUE_DESC, page.phys + desc_off);
        mmio_write::<u64>(self.common + CC_QUEUE_DRIVER, page.phys + avail_off);
        mmio_write::<u64>(self.common + CC_QUEUE_DEVICE, page.phys + used_off);
        let q = SplitQueue {
            index,
            size,
            page,
            desc_off,
            avail_off,
            used_off,
            notify_addr: self.notify + notify_off as u64 * self.notify_multiplier as u64,
            avail_idx: 0,
            last_used: 0,
        };
        // Polled queues: ask the device not to interrupt for completions.
        // SAFETY: the avail ring flags word lies inside the queue page.
        unsafe { core::ptr::write_volatile((q.page.virt + q.avail_off) as *mut u16, AVAIL_F_NO_INTERRUPT) };
        mmio_write::<u16>(self.common + CC_QUEUE_ENABLE, 1);
        Ok(Some(q))
    }

    pub fn driver_ok(&self) {
        self.set_status(STATUS_ACKNOWLEDGE | STATUS_DRIVER | STATUS_FEATURES_OK | STATUS_DRIVER_OK);
    }
}

#[repr(C)]
#[derive(Clone, Copy)]
struct Desc {
    addr: u64,
    len: u32,
    flags: u16,
    next: u16,
}

/// A split virtqueue whose descriptor table and both rings share one page.
pub struct SplitQueue {
    pub index: u16,
    pub size: u16,
    page: DmaPage,
    desc_off: u64,
    avail_off: u64,
    used_off: u64,
    notify_addr: u64,
    avail_idx: u16,
    last_used: u16,
}

// SAFETY: the queue owns its page exclusively; drivers serialise access with a lock.
unsafe impl Send for SplitQueue {}

impl SplitQueue {
    /// Point descriptor `i` at a buffer.
    pub fn set_desc(&mut self, i: u16, addr: u64, len: u32, flags: u16, next: u16) {
        assert!(i < self.size, "descriptor index out of range");
        let p =
            (self.page.virt + self.desc_off + i as u64 * core::mem::size_of::<Desc>() as u64) as *mut Desc;
        // SAFETY: descriptor slot inside the queue page (checked above).
        unsafe { core::ptr::write_volatile(p, Desc { addr, len, flags, next }) };
    }

    /// Offer the chain starting at descriptor `head` to the device.
    pub fn push_avail(&mut self, head: u16) {
        let slot = (self.avail_idx % self.size) as u64;
        // SAFETY: avail ring slots and index inside the queue page.
        unsafe {
            core::ptr::write_volatile((self.page.virt + self.avail_off + 4 + slot * 2) as *mut u16, head);
            // The entry must be visible before the index that publishes it.
            fence(Ordering::SeqCst);
            self.avail_idx = self.avail_idx.wrapping_add(1);
            core::ptr::write_volatile((self.page.virt + self.avail_off + 2) as *mut u16, self.avail_idx);
        }
        fence(Ordering::SeqCst);
    }

    /// Tell the device there is new work on this queue.
    pub fn notify(&self) {
        mmio_write::<u16>(self.notify_addr, self.index);
    }

    fn used_idx(&self) -> u16 {
        fence(Ordering::SeqCst);
        // SAFETY: used ring index inside the queue page.
        unsafe { core::ptr::read_volatile((self.page.virt + self.used_off + 2) as *const u16) }
    }

    /// True when the device has completed something not yet taken.
    pub fn has_used(&self) -> bool {
        self.used_idx() != self.last_used
    }

    /// Take the next completion: `(descriptor head, bytes the device wrote)`.
    pub fn pop_used(&mut self) -> Option<(u32, u32)> {
        if self.used_idx() == self.last_used {
            return None;
        }
        let slot = (self.last_used % self.size) as u64;
        // SAFETY: used ring element inside the queue page; read after the index.
        let (id, len) = unsafe {
            let e = self.page.virt + self.used_off + 4 + slot * 8;
            (core::ptr::read_volatile(e as *const u32), core::ptr::read_volatile((e + 4) as *const u32))
        };
        self.last_used = self.last_used.wrapping_add(1);
        Some((id, len))
    }
}
