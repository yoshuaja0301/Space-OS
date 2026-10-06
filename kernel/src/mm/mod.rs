//! Memory management: physical frames, kernel/user page tables, kernel heap and
//! kernel stacks. Quotas are enforced at the address-space level (see `proc`).
//!
//! Kernel virtual layout (PML4 slot in parentheses):
//!
//! | range                    | use                                   |
//! |--------------------------|---------------------------------------|
//! | 0xFFFF_8000_0000_0000 (256) | linear map of physical memory (`PHYS_OFFSET`) |
//! | 0xFFFF_9000_0000_0000 (288) | kernel heap                          |
//! | 0xFFFF_A000_0000_0000 (320) | kernel stacks (64 KiB slots with guard pages) |
//! | 0xFFFF_FFFF_8000_0000 (511) | kernel image                         |

pub mod frame;
pub mod heap;
pub mod kstack;
pub mod mmio;
pub mod paging;
pub mod pt;

pub use paging::{AddressSpace, phys_to_virt};
use spaceabi::boot::BootInfo;

pub const PAGE_SIZE: u64 = 4096;
pub const HEAP_BASE: u64 = 0xFFFF_9000_0000_0000;
pub const HEAP_SIZE: usize = 16 * 1024 * 1024;
pub const KSTACK_BASE: u64 = 0xFFFF_A000_0000_0000;
/// Window for device MMIO, mapped uncached (PML4 slot 352).
pub const MMIO_BASE: u64 = 0xFFFF_B000_0000_0000;
pub const MMIO_WINDOW: u64 = 1 << 30;
/// Exclusive end of user space (lower canonical half).
pub const USER_SPACE_END: u64 = 0x0000_8000_0000_0000;

/// What a mapping allows, independent of how an architecture encodes it. Every
/// mapping can be read; the flags add to that.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct MapFlags(u8);

impl MapFlags {
    /// Kernel-only, read-only, not executable, cached.
    pub const KERNEL_RO: MapFlags = MapFlags(0);
    pub const WRITABLE: MapFlags = MapFlags(1);
    /// Reachable from user mode.
    pub const USER: MapFlags = MapFlags(2);
    pub const EXECUTABLE: MapFlags = MapFlags(4);
    /// Device registers: uncached, never speculated into.
    pub const DEVICE: MapFlags = MapFlags(8);
    /// The same in every address space (kernel mappings), so it may stay in the TLB
    /// across address-space switches.
    pub const GLOBAL: MapFlags = MapFlags(16);

    pub const fn contains(self, other: MapFlags) -> bool {
        self.0 & other.0 == other.0
    }
}

impl core::ops::BitOr for MapFlags {
    type Output = MapFlags;
    fn bitor(self, rhs: MapFlags) -> MapFlags {
        MapFlags(self.0 | rhs.0)
    }
}

impl core::ops::BitOrAssign for MapFlags {
    fn bitor_assign(&mut self, rhs: MapFlags) {
        self.0 |= rhs.0;
    }
}

pub fn init(bi: &BootInfo) {
    frame::init(bi);
    paging::init(bi);
    heap::init();
    kstack::init();
    mmio::init();
    let (total, free) = frame::stats();
    println!(
        "[kernel] memory: {} MiB usable, {} MiB free after kernel init",
        total * 4 / 1024,
        free * 4 / 1024
    );
}

/// True when `addr` is mapped in the kernel page tables (used by the panic backtrace).
pub fn kernel_addr_is_mapped(addr: u64) -> bool {
    addr >= spaceabi::boot::PHYS_OFFSET && paging::translate_current(addr).is_some()
}

/// True when every byte of `[va, va + len)` is mapped in the kernel's half of the
/// live tables. Works from the first instruction on (the bootloader's tables, read
/// through the linear map), so the hand-over can be checked before it is read.
pub fn range_is_mapped(va: u64, len: u64) -> bool {
    let Some(end) = va.checked_add(len) else { return false };
    if va < spaceabi::boot::PHYS_OFFSET {
        return false;
    }
    let mut at = va;
    loop {
        let Some(step) = paging::mapped_extent(at) else { return false };
        match (at & !(step - 1)).checked_add(step) {
            Some(next) if next < end => at = next,
            _ => return true,
        }
    }
}
