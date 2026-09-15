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
pub mod paging;

pub use paging::{AddressSpace, phys_to_virt};
use spaceabi::boot::BootInfo;

pub const PAGE_SIZE: u64 = 4096;
pub const HEAP_BASE: u64 = 0xFFFF_9000_0000_0000;
pub const HEAP_SIZE: usize = 16 * 1024 * 1024;
pub const KSTACK_BASE: u64 = 0xFFFF_A000_0000_0000;
/// Exclusive end of user space (lower canonical half).
pub const USER_SPACE_END: u64 = 0x0000_8000_0000_0000;

pub fn init(bi: &BootInfo) {
    frame::init(bi);
    paging::init(bi);
    heap::init();
    kstack::init();
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
