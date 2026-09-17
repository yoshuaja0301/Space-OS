//! Bitmap physical frame allocator over the bootloader memory map.
//!
//! One bit per 4 KiB frame (1 = used). `stats()` is what `SYS_KSTATS` reports and
//! what the K03 reclamation test compares before/after spawn/exit cycles.

use spaceabi::boot::{BootInfo, MemRegion, mem_kind};
use x86_64::PhysAddr;
use x86_64::structures::paging::PhysFrame;

use super::PAGE_SIZE;
use super::phys_to_virt;
use crate::sync::SpinLock;

struct Bitmap {
    bits: &'static mut [u64],
    nframes: usize,
    usable: usize,
    free: usize,
    hint: usize,
}

static ALLOC: SpinLock<Option<Bitmap>> = SpinLock::new(None);

fn regions(bi: &BootInfo) -> &'static [MemRegion] {
    // SAFETY: the bootloader placed the array in KERNEL memory that stays mapped.
    unsafe {
        core::slice::from_raw_parts(
            phys_to_virt(bi.memory_map.phys).as_ptr::<MemRegion>(),
            bi.memory_map_entries as usize,
        )
    }
}

fn is_reclaimable(kind: u32) -> bool {
    kind == mem_kind::USABLE || kind == mem_kind::BOOTLOADER_RECLAIMABLE
}

pub fn init(bi: &BootInfo) {
    let regs = regions(bi);
    let mut phys_end = 0u64;
    for r in regs {
        if is_reclaimable(r.kind) {
            phys_end = phys_end.max(r.end());
        }
    }
    let nframes = (phys_end / PAGE_SIZE) as usize;
    let words = nframes.div_ceil(64);
    let bitmap_bytes = (words * 8) as u64;
    let bitmap_pages = bitmap_bytes.div_ceil(PAGE_SIZE);

    // Host the bitmap in the first usable region (above 1 MiB) that can hold it.
    let host = regs
        .iter()
        .find(|r| r.kind == mem_kind::USABLE && r.start >= 0x10_0000 && r.len >= bitmap_pages * PAGE_SIZE)
        .expect("no usable region large enough for the frame bitmap");
    let bitmap_phys = host.start;
    // SAFETY: `words` u64s inside a region we just verified is free RAM.
    let bits: &'static mut [u64] =
        unsafe { core::slice::from_raw_parts_mut(phys_to_virt(bitmap_phys).as_mut_ptr::<u64>(), words) };
    bits.fill(u64::MAX);

    let mut usable = 0usize;
    for r in regs {
        if !is_reclaimable(r.kind) {
            continue;
        }
        let first = (r.start / PAGE_SIZE) as usize;
        let last = (r.end() / PAGE_SIZE) as usize;
        for f in first..last {
            if f == 0 {
                continue; // never hand out physical page 0
            }
            bits[f / 64] &= !(1u64 << (f % 64));
            usable += 1;
        }
    }
    // The bitmap's own frames are in use.
    let first = (bitmap_phys / PAGE_SIZE) as usize;
    for f in first..first + bitmap_pages as usize {
        bits[f / 64] |= 1u64 << (f % 64);
        usable -= 1;
    }
    let free = usable;
    let mut kinds = [0u64; 10];
    for r in regs {
        if (r.kind as usize) < kinds.len() {
            kinds[r.kind as usize] += r.len;
        }
    }
    println!(
        "[kernel] frames: {} usable ({} MiB), bitmap {} KiB at {:#x}; reserved {} MiB, kernel {} MiB, acpi {} MiB, mmio {} MiB",
        usable,
        usable * 4 / 1024,
        bitmap_bytes / 1024,
        bitmap_phys,
        kinds[mem_kind::RESERVED as usize] >> 20,
        kinds[mem_kind::KERNEL as usize] >> 20,
        (kinds[mem_kind::ACPI_RECLAIMABLE as usize] + kinds[mem_kind::ACPI_NVS as usize]) >> 20,
        kinds[mem_kind::MMIO as usize] >> 20,
    );
    *ALLOC.lock() = Some(Bitmap { bits, nframes, usable, free, hint: 0 });
}

pub fn alloc() -> Option<PhysFrame> {
    let mut g = ALLOC.lock();
    let b = g.as_mut()?;
    let words = b.bits.len();
    let start_word = b.hint / 64;
    for i in 0..words {
        let w = (start_word + i) % words;
        let word = b.bits[w];
        if word != u64::MAX {
            let bit = word.trailing_ones() as usize;
            let idx = w * 64 + bit;
            if idx >= b.nframes {
                continue;
            }
            b.bits[w] |= 1u64 << bit;
            b.free -= 1;
            b.hint = idx;
            return Some(PhysFrame::containing_address(PhysAddr::new(idx as u64 * PAGE_SIZE)));
        }
    }
    None
}

/// Allocate a frame and clear it through the linear map.
pub fn alloc_zeroed() -> Option<PhysFrame> {
    let f = alloc()?;
    // SAFETY: freshly allocated frame, accessible through the linear map.
    unsafe {
        core::ptr::write_bytes(
            phys_to_virt(f.start_address().as_u64()).as_mut_ptr::<u8>(),
            0,
            PAGE_SIZE as usize,
        )
    };
    Some(f)
}

pub fn free(frame: PhysFrame) {
    let idx = (frame.start_address().as_u64() / PAGE_SIZE) as usize;
    let mut g = ALLOC.lock();
    let b = g.as_mut().expect("frame allocator not initialised");
    assert!(idx < b.nframes, "free of frame outside RAM: {:#x}", frame.start_address());
    let mask = 1u64 << (idx % 64);
    assert!(b.bits[idx / 64] & mask != 0, "double free of frame {:#x}", frame.start_address());
    b.bits[idx / 64] &= !mask;
    b.free += 1;
    if idx < b.hint {
        b.hint = idx;
    }
}

/// `(usable, free)` frame counts.
pub fn stats() -> (usize, usize) {
    let g = ALLOC.lock();
    match g.as_ref() {
        Some(b) => (b.usable, b.free),
        None => (0, 0),
    }
}
