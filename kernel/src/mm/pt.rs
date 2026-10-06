//! Four-level page tables, for every architecture the kernel runs on.
//!
//! x86-64 and AArch64 (4 KiB granule, 48-bit addresses) walk the same shape: four
//! levels of 512 eight-byte entries, nine address bits per level, 4 KiB pages at
//! the bottom and blocks of 2 MiB and 1 GiB one and two levels up. Only what an
//! entry *says* differs, so the walk lives here once and the encoding of an entry
//! lives in `arch::mmu`.
//!
//! Levels count down from the root: 3 (PML4 / L0), 2, 1, 0 (the page level).

use spaceabi::error::Error;

use super::{MapFlags, PAGE_SIZE, frame, phys_to_virt};
use crate::arch::mmu;

const ENTRIES: usize = 512;

fn index(va: u64, level: usize) -> usize {
    ((va >> (12 + 9 * level)) & (ENTRIES as u64 - 1)) as usize
}

/// The table at physical address `pa`, through the linear map.
///
/// # Safety
/// `pa` must be a page table of the live hierarchy and the caller must serialise
/// writers to it (the kernel map lock, or `&mut` of the address space).
pub unsafe fn table(pa: u64) -> &'static mut [u64; ENTRIES] {
    // SAFETY: per the contract; page tables are RAM reachable through the linear map.
    unsafe { &mut *phys_to_virt(pa).as_mut_ptr::<[u64; ENTRIES]>() }
}

/// Map the 4 KiB page at `va` to the frame at `pa`, building the tables on the way.
///
/// Fails with `Busy` if something is mapped there already (blocks included) and
/// `NoMemory` if a table could not be allocated; a table allocated on the way stays
/// in place either way, as the tables of a hierarchy only go when it does.
pub fn map(root: u64, va: u64, pa: u64, flags: MapFlags) -> Result<(), Error> {
    let mut t = root;
    for level in (1..=3).rev() {
        // SAFETY: `t` is the root or a table reached from it; see `table`.
        let tab = unsafe { table(t) };
        let i = index(va, level);
        let e = tab[i];
        if !mmu::present(e) {
            let new = frame::alloc_zeroed().ok_or(Error::NoMemory)?.start_address().as_u64();
            mmu::write_entry(&mut tab[i], mmu::table_entry(new, flags));
            t = new;
        } else if mmu::is_block(e, level) {
            return Err(Error::Busy);
        } else {
            let widened = mmu::widen_table(e, flags);
            if widened != e {
                mmu::write_entry(&mut tab[i], widened);
            }
            t = mmu::addr(e);
        }
    }
    // SAFETY: as above.
    let tab = unsafe { table(t) };
    let i = index(va, 0);
    if mmu::present(tab[i]) {
        return Err(Error::Busy);
    }
    mmu::write_entry(&mut tab[i], mmu::page_entry(pa, flags));
    mmu::mapped(va);
    Ok(())
}

/// Unmap the 4 KiB page at `va` and flush it from this CPU's TLB; the frame that was
/// there, if any.
pub fn unmap(root: u64, va: u64) -> Option<u64> {
    let mut t = root;
    for level in (1..=3).rev() {
        // SAFETY: see `table`.
        let e = unsafe { table(t) }[index(va, level)];
        if !mmu::present(e) || mmu::is_block(e, level) {
            return None;
        }
        t = mmu::addr(e);
    }
    // SAFETY: see `table`.
    let tab = unsafe { table(t) };
    let i = index(va, 0);
    let e = tab[i];
    if !mmu::present(e) {
        return None;
    }
    mmu::write_entry(&mut tab[i], 0);
    mmu::unmapped(va);
    Some(mmu::addr(e))
}

/// What `va` is mapped to under `root`: the physical address (page offset included),
/// the access the entry gives, and whether it is a 4 KiB page rather than a block.
/// Read only and lock-free, so the panic path can use it.
pub fn translate(root: u64, va: u64) -> Option<(u64, MapFlags, bool)> {
    let mut t = root;
    for level in (1..=3).rev() {
        // SAFETY: read-only walk of a live hierarchy.
        let e = unsafe { table(t) }[index(va, level)];
        if !mmu::present(e) {
            return None;
        }
        if mmu::is_block(e, level) {
            let size = PAGE_SIZE << (9 * level);
            return Some((mmu::addr(e) + (va & (size - 1)), mmu::flags(e), false));
        }
        t = mmu::addr(e);
    }
    // SAFETY: as above.
    let e = unsafe { table(t) }[index(va, 0)];
    if !mmu::present(e) {
        return None;
    }
    Some((mmu::addr(e) + (va & (PAGE_SIZE - 1)), mmu::flags(e), true))
}

/// Free every table below root slots `slots` (not the pages they map, nor the root),
/// and clear those slots. Blocks are left alone: only the kernel's linear map uses
/// them, and it is never freed.
pub fn free_tables(root: u64, slots: core::ops::Range<usize>) {
    // SAFETY: the caller owns the hierarchy (an address space being torn down).
    let top = unsafe { table(root) };
    for i in slots {
        let e = top[i];
        if !mmu::present(e) {
            continue;
        }
        free_below(mmu::addr(e), 2);
        frame::free_phys(mmu::addr(e));
        top[i] = 0;
    }
}

fn free_below(t: u64, level: usize) {
    if level == 0 {
        return;
    }
    // SAFETY: a table of a hierarchy being torn down (see `free_tables`).
    let tab = unsafe { table(t) };
    for &e in tab.iter() {
        if !mmu::present(e) || mmu::is_block(e, level) {
            continue;
        }
        free_below(mmu::addr(e), level - 1);
        frame::free_phys(mmu::addr(e));
    }
}
