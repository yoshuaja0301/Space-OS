//! The counter x86-64 keeps time with: the ACPI power-management timer.
//!
//! It counts at 3.579545 MHz whatever the processors do, and every PC with ACPI tables
//! has one except "hardware-reduced" ones; the FADT names its I/O port. Its rate is
//! fixed by the specification, so nothing has to be measured against anything (the
//! TSC's rate would have to be).

use core::sync::atomic::{AtomicU16, Ordering};

use x86_64::instructions::port::Port;

use crate::acpi;
use crate::clock::Counter;

/// ACPI 6.5 §4.8.3.3: the PM timer's rate on every machine.
const PM_TIMER_HZ: u64 = 3_579_545;
/// FADT flags: the counter has 32 bits rather than 24; no fixed ACPI hardware.
const TMR_VAL_EXT: u32 = 1 << 8;
const HW_REDUCED_ACPI: u32 = 1 << 20;
/// Generic address structure: system I/O space.
const SPACE_IO: u8 = 1;

static PORT: AtomicU16 = AtomicU16::new(0);

pub fn counter(rsdp: u64) -> Result<Counter, &'static str> {
    let fadt = acpi::find(rsdp, b"FACP").map_err(|_| "no FADT")?;
    if fadt.len() < 116 {
        return Err("the FADT is too short");
    }
    let flags = acpi::u32_at(fadt, 112);
    if flags & HW_REDUCED_ACPI != 0 {
        return Err("hardware-reduced ACPI: no PM timer");
    }
    // X_PM_TMR_BLK (ACPI 2.0+) takes precedence when it is an I/O port; then the
    // 32-bit PM_TMR_BLK, whose length must be 4.
    let mut port = u64::from(acpi::u32_at(fadt, 76));
    if fadt.len() >= 220 && fadt[208] == SPACE_IO && acpi::u64_at(fadt, 212) != 0 {
        port = acpi::u64_at(fadt, 212);
    } else if fadt[91] != 4 {
        port = 0;
    }
    if port == 0 || port > 0xFFFF {
        return Err("the FADT names no PM timer port");
    }
    PORT.store(port as u16, Ordering::Relaxed);
    let mask: u64 = if flags & TMR_VAL_EXT != 0 { 0xFFFF_FFFF } else { 0xFF_FFFF };
    // It must count: a port that reads the same for a millisecond is not a timer.
    let first = read() & mask;
    if !crate::dev::wait::until(1, || read() & mask != first) {
        PORT.store(0, Ordering::Relaxed);
        return Err("the PM timer does not count");
    }
    Ok(Counter { name: "ACPI PM timer", hz: PM_TIMER_HZ, mask })
}

pub fn read() -> u64 {
    let port = PORT.load(Ordering::Relaxed);
    if port == 0 {
        return 0;
    }
    // SAFETY: the PM timer's port, as the FADT names it; reading it has no effect.
    u64::from(unsafe { Port::<u32>::new(port).read() })
}
