//! What an x86-64 page-table entry says (the walk itself is `mm::pt`).

use x86_64::instructions::tlb;
use x86_64::registers::control::{Cr3, Cr3Flags};
use x86_64::structures::paging::PhysFrame;
use x86_64::{PhysAddr, VirtAddr};

use crate::mm::MapFlags;

const PRESENT: u64 = 1;
const WRITABLE: u64 = 1 << 1;
const USER: u64 = 1 << 2;
const WRITE_THROUGH: u64 = 1 << 3;
const NO_CACHE: u64 = 1 << 4;
const HUGE: u64 = 1 << 7;
const GLOBAL: u64 = 1 << 8;
const NO_EXECUTE: u64 = 1 << 63;
const ADDR: u64 = 0x000F_FFFF_FFFF_F000;

pub fn present(e: u64) -> bool {
    e & PRESENT != 0
}

/// A 1 GiB or 2 MiB page rather than a pointer to the next table.
pub fn is_block(e: u64, level: usize) -> bool {
    (level == 1 || level == 2) && e & HUGE != 0
}

pub fn addr(e: u64) -> u64 {
    e & ADDR
}

/// An entry pointing at the next-level table at `pa`. Access is decided by the
/// leaf, except that user mode needs the user bit on every level on the way.
pub fn table_entry(pa: u64, flags: MapFlags) -> u64 {
    pa | PRESENT | WRITABLE | if flags.contains(MapFlags::USER) { USER } else { 0 }
}

/// `e`, a table entry, opened up enough for a leaf with `flags` below it.
pub fn widen_table(e: u64, flags: MapFlags) -> u64 {
    if flags.contains(MapFlags::USER) { e | USER } else { e }
}

pub fn page_entry(pa: u64, flags: MapFlags) -> u64 {
    let mut e = pa | PRESENT;
    if flags.contains(MapFlags::WRITABLE) {
        e |= WRITABLE;
    }
    if flags.contains(MapFlags::USER) {
        e |= USER;
    }
    if flags.contains(MapFlags::GLOBAL) {
        e |= GLOBAL;
    }
    if flags.contains(MapFlags::DEVICE) {
        e |= NO_CACHE | WRITE_THROUGH;
    }
    if !flags.contains(MapFlags::EXECUTABLE) {
        e |= NO_EXECUTE;
    }
    e
}

pub fn flags(e: u64) -> MapFlags {
    let mut f = MapFlags::KERNEL_RO;
    if e & WRITABLE != 0 {
        f |= MapFlags::WRITABLE;
    }
    if e & USER != 0 {
        f |= MapFlags::USER;
    }
    if e & GLOBAL != 0 {
        f |= MapFlags::GLOBAL;
    }
    if e & NO_CACHE != 0 {
        f |= MapFlags::DEVICE;
    }
    if e & NO_EXECUTE == 0 {
        f |= MapFlags::EXECUTABLE;
    }
    f
}

pub fn write_entry(slot: &mut u64, e: u64) {
    // SAFETY: a valid reference; volatile so the store is not merged or dropped
    // before the walker can see it.
    unsafe { core::ptr::write_volatile(slot, e) };
}

/// After a new page appeared at `va`. Nothing can be cached for a page that was not
/// mapped, but the walk is cheap to make certain of.
pub fn mapped(va: u64) {
    tlb::flush(VirtAddr::new(va));
}

/// After the page at `va` went away: this CPU forgets it. Other CPUs never hold it
/// (ADR-0024 decision 5: every switch reloads CR3).
pub fn unmapped(va: u64) {
    tlb::flush(VirtAddr::new(va));
}

/// The root that translates `va` right now. On x86-64 one root holds both halves.
pub fn root_for(_va: u64) -> u64 {
    Cr3::read().0.start_address().as_u64()
}

/// Make `root` the kernel's tables; called once, when the kernel leaves the
/// bootloader's tables (and with them the identity map) behind.
pub fn install_kernel_root(root: u64) {
    load_user_root(root);
}

/// Switch to the address space at `root`, flushing what this CPU cached of the
/// previous one -- even when it is the same root (ADR-0024).
pub fn load_user_root(root: u64) {
    // SAFETY: only complete PML4s (the kernel's or a process's, with the kernel
    // half copied in) are passed here.
    unsafe { Cr3::write(PhysFrame::containing_address(PhysAddr::new(root)), Cr3Flags::empty()) };
}

/// True while `root` is this CPU's user address space.
pub fn user_root_active(root: u64) -> bool {
    Cr3::read().0.start_address().as_u64() == root
}
