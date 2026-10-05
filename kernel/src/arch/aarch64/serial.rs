//! PL011 UART: the console. Its registers are mapped by the bootloader at
//! `EARLY_UART_VIRT` (from ACPI SPCR), so the kernel prints from its first line.

use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use spaceabi::boot::{BootInfo, EARLY_UART_VIRT, uart_kind};

use super::gic;
use crate::acpi;

const DR: u64 = 0x00;
const FR: u64 = 0x18;
const IMSC: u64 = 0x38;
const ICR: u64 = 0x44;
const FR_TXFF: u32 = 1 << 5;
const FR_RXFE: u32 = 1 << 4;
const RXIM: u32 = 1 << 4;
const RTIM: u32 = 1 << 6;

/// Virtual address of the UART registers, 0 without one.
static BASE: AtomicU64 = AtomicU64::new(0);
/// The receive interrupt, 0 until input is on.
static RX_INTID: AtomicU32 = AtomicU32::new(0);

fn rd(off: u64) -> u32 {
    // SAFETY: BASE is the mapped register page of the UART.
    unsafe { core::ptr::read_volatile((BASE.load(Ordering::Relaxed) + off) as *const u32) }
}

fn wr(off: u64, v: u32) {
    // SAFETY: as above.
    unsafe { core::ptr::write_volatile((BASE.load(Ordering::Relaxed) + off) as *mut u32, v) }
}

/// Take the UART the bootloader found, if any. The firmware left it configured.
pub fn init(boot_info: *const BootInfo) {
    // SAFETY: the bootloader passes a readable BootInfo; its fields are checked
    // before they are believed.
    let bi = unsafe { &*boot_info };
    if bi.is_valid() && bi.uart_kind == uart_kind::PL011 && bi.uart != 0 {
        BASE.store(EARLY_UART_VIRT + (bi.uart & 0xFFF), Ordering::Relaxed);
    }
}

fn write_byte(b: u8) {
    // A bounded wait: a UART that never drains must not hang the kernel.
    for _ in 0..1_000_000 {
        if rd(FR) & FR_TXFF == 0 {
            wr(DR, u32::from(b));
            return;
        }
        core::hint::spin_loop();
    }
}

pub fn write_str(s: &str) {
    if BASE.load(Ordering::Relaxed) == 0 {
        return;
    }
    for b in s.bytes() {
        if b == b'\n' {
            write_byte(b'\r');
        }
        write_byte(b);
    }
}

/// Turn on receive interrupts; the interrupt ID, from SPCR (offset 54), if the UART
/// has one.
pub fn init_input(rsdp: u64) -> Option<u32> {
    if BASE.load(Ordering::Relaxed) == 0 {
        return None;
    }
    let spcr = acpi::find(rsdp, b"SPCR").ok()?;
    // Interrupt type (52) bit 3: ARMH GIC; global system interrupt at 54.
    if spcr.len() < 58 || spcr[52] & (1 << 3) == 0 {
        return None;
    }
    let intid = acpi::u32_at(spcr, 54);
    if intid < 32 {
        return None;
    }
    // Drop whatever arrived before the kernel listened, then interrupt on receive
    // and on receive timeout (a short line that does not fill the FIFO).
    while rd(FR) & FR_RXFE == 0 {
        let _ = rd(DR);
    }
    wr(ICR, 0x7FF);
    wr(IMSC, RXIM | RTIM);
    RX_INTID.store(intid, Ordering::Relaxed);
    gic::enable(intid, false);
    Some(intid)
}

pub fn input_intid() -> Option<u32> {
    match RX_INTID.load(Ordering::Relaxed) {
        0 => None,
        n => Some(n),
    }
}

/// Everything the UART has received into the console input buffer.
pub fn drain_input() {
    let mut n = 0;
    while rd(FR) & FR_RXFE == 0 && n < 4096 {
        crate::input::push((rd(DR) & 0xFF) as u8);
        n += 1;
    }
    wr(ICR, RXIM | RTIM);
}

/// Nothing to resume: the receive interrupt is never paused.
pub fn resume_input() {}
