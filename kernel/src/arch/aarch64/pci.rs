//! PCI configuration space through ECAM (the ACPI MCFG table): every function's
//! 4 KiB of registers is memory at `base + (bus << 20 | device << 15 | function << 12)`.
//!
//! Only the first segment is used, and of it the first [`MAX_BUSES`] buses: the
//! devices of a virtual machine sit on bus 0, and each bus costs 1 MiB of the
//! device window. A function outside what is mapped reads as absent (all ones), as
//! an empty slot does.

use core::sync::atomic::{AtomicU8, AtomicU64, Ordering};

use crate::{acpi, mm};

const MAX_BUSES: u64 = 16;

/// Virtual address of the mapped ECAM window, 0 without one.
static BASE: AtomicU64 = AtomicU64::new(0);
static FIRST_BUS: AtomicU8 = AtomicU8::new(0);
static BUSES: AtomicU8 = AtomicU8::new(0);

pub fn init(rsdp: u64) {
    let Ok(mcfg) = acpi::find(rsdp, b"MCFG") else {
        println!("[kernel] pci: no MCFG table; no PCI devices");
        return;
    };
    // Allocation structures start at 44, 16 bytes each: base (8), segment (2),
    // start bus (1), end bus (1).
    let Some(e) = mcfg.get(44..60) else {
        println!("[kernel] pci: the MCFG lists no configuration space; no PCI devices");
        return;
    };
    let (base, segment, first, last) = (acpi::u64_at(e, 0), u16::from_le_bytes([e[8], e[9]]), e[10], e[11]);
    if segment != 0 || last < first {
        println!("[kernel] pci: the MCFG's first range is segment {segment}, buses {first}-{last}; not used");
        return;
    }
    let buses = (u64::from(last - first) + 1).min(MAX_BUSES);
    match mm::mmio::map(base, buses << 20) {
        Ok(virt) => {
            FIRST_BUS.store(first, Ordering::Relaxed);
            BUSES.store(buses as u8, Ordering::Relaxed);
            BASE.store(virt, Ordering::Relaxed);
            println!("[kernel] pci: ECAM at {base:#x}, buses {first}-{}", u64::from(first) + buses - 1);
        }
        Err(e) => println!("[kernel] pci: cannot map ECAM at {base:#x}: {e}"),
    }
}

fn register(bus: u8, device: u8, function: u8, offset: u8) -> Option<u64> {
    let base = BASE.load(Ordering::Relaxed);
    let rel = bus.checked_sub(FIRST_BUS.load(Ordering::Relaxed))?;
    if base == 0 || rel >= BUSES.load(Ordering::Relaxed) || device >= 32 || function >= 8 {
        return None;
    }
    Some(
        base + (u64::from(rel) << 20)
            + (u64::from(device) << 15)
            + (u64::from(function) << 12)
            + u64::from(offset & 0xFC),
    )
}

pub fn read32(bus: u8, device: u8, function: u8, offset: u8) -> u32 {
    match register(bus, device, function, offset) {
        // SAFETY: a register inside the mapped ECAM window.
        Some(a) => unsafe { core::ptr::read_volatile(a as *const u32) },
        None => u32::MAX,
    }
}

pub fn write32(bus: u8, device: u8, function: u8, offset: u8, value: u32) {
    if let Some(a) = register(bus, device, function, offset) {
        // SAFETY: as above.
        unsafe { core::ptr::write_volatile(a as *mut u32, value) };
    }
}
