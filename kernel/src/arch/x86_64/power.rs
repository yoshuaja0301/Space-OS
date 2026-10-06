//! Switching the machine off: ACPI sleep state S5.
//!
//! QEMU's debug-exit device ends a test run and does nothing on a PC. There, off is
//! S5: the DSDT's `\_S5` package gives the SLP_TYP values, and writing them with
//! SLP_EN to the PM1a (and PM1b, when there is one) control registers the FADT names
//! cuts the power. Both are read once, at boot. Nothing else of the DSDT is
//! interpreted -- there is no AML interpreter, only a search for that one package --
//! and a machine whose tables do not say is reported, and halted instead of switched
//! off.

use core::sync::atomic::{AtomicU64, Ordering};

use x86_64::instructions::port::Port;

use super::acpi;

const SLP_EN: u16 = 1 << 13;
/// PM1 control: the system is in ACPI mode (the firmware has handed ACPI over).
const SCI_EN: u16 = 1;

/// PM1a port (bits 0-15), PM1b port (16-31), SLP_TYPa (32-34), SLP_TYPb (35-37),
/// SMI command port (40-55), and bit 63 set once S5 is known.
static S5: AtomicU64 = AtomicU64::new(0);
/// The value the SMI command port takes to hand ACPI to the OS (0: none needed).
static ACPI_ENABLE: AtomicU64 = AtomicU64::new(0);
const KNOWN: u64 = 1 << 63;

/// An AML integer at the start of `b`: its value and how many bytes it took.
fn integer(b: &[u8]) -> Option<(u64, usize)> {
    match *b.first()? {
        0x00 => Some((0, 1)),                                                       // ZeroOp
        0x01 => Some((1, 1)),                                                       // OneOp
        0x0A => Some((u64::from(*b.get(1)?), 2)),                                   // BytePrefix
        0x0B => Some((u64::from(u16::from_le_bytes([*b.get(1)?, *b.get(2)?])), 3)), // WordPrefix
        _ => None,
    }
}

/// SLP_TYPa and SLP_TYPb from `Name (_S5, Package (n) { a, b, ... })` in the DSDT.
fn sleep_types(dsdt: &[u8]) -> Option<(u64, u64)> {
    let body = dsdt.get(36..)?;
    let mut i = 1;
    while i + 4 < body.len() {
        // NameOp, optionally with the root prefix, then the name, then PackageOp.
        let named = body[i - 1] == 0x08 || (i >= 2 && body[i - 1] == b'\\' && body[i - 2] == 0x08);
        if &body[i..i + 4] == b"_S5_" && named && body.get(i + 4) == Some(&0x12) {
            let lead = *body.get(i + 5)?;
            // PkgLength: bits 7-6 of its first byte count the bytes that follow.
            let elements = i + 6 + usize::from(lead >> 6) + 1;
            let (a, n) = integer(body.get(elements..)?)?;
            let (b, _) = integer(body.get(elements + n..)?)?;
            return Some((a & 7, b & 7));
        }
        i += 1;
    }
    None
}

/// Read what switching off takes from the ACPI tables. `Err` says why it cannot be
/// done on this machine.
fn probe(rsdp: u64) -> Result<(u64, u64), &'static str> {
    let fadt = acpi::find(rsdp, b"FACP").map_err(|_| "no FADT")?;
    if fadt.len() < 116 {
        return Err("the FADT is too short");
    }
    let pm1a = acpi::u32_at(fadt, 64);
    let pm1b = acpi::u32_at(fadt, 68);
    if pm1a == 0 || pm1a > 0xFFFF || pm1b > 0xFFFF {
        return Err("no PM1 control register in I/O space");
    }
    let x_dsdt = if fadt.len() >= 148 { acpi::u64_at(fadt, 140) } else { 0 };
    let dsdt_pa = if x_dsdt != 0 { x_dsdt } else { u64::from(acpi::u32_at(fadt, 40)) };
    let dsdt = acpi::table_at(dsdt_pa).ok_or("the DSDT is not readable")?;
    let (a, b) = sleep_types(dsdt).ok_or("the DSDT has no \\_S5")?;
    let smi_cmd = u64::from(acpi::u32_at(fadt, 48) & 0xFFFF);
    let enable = u64::from(fadt[52]);
    let packed = u64::from(pm1a) | (u64::from(pm1b) << 16) | (a << 32) | (b << 35) | (smi_cmd << 40) | KNOWN;
    Ok((packed, enable))
}

pub fn init(rsdp: u64) {
    match probe(rsdp) {
        Ok((packed, enable)) => {
            S5.store(packed, Ordering::Relaxed);
            ACPI_ENABLE.store(enable, Ordering::Relaxed);
            println!(
                "[kernel] power: ACPI S5 through PM1a_CNT {:#x} (SLP_TYP {})",
                packed & 0xFFFF,
                (packed >> 32) & 7
            );
        }
        Err(why) => println!("[kernel] power: no ACPI S5 ({why}); shutting down halts the machine"),
    }
}

/// Switch the machine off. Returns only if it did not go off.
pub fn poweroff() {
    let s5 = S5.load(Ordering::Relaxed);
    if s5 & KNOWN == 0 {
        return;
    }
    let (pm1a, pm1b) = ((s5 & 0xFFFF) as u16, ((s5 >> 16) & 0xFFFF) as u16);
    let (typ_a, typ_b) = (((s5 >> 32) & 7) as u16, ((s5 >> 35) & 7) as u16);
    let smi_cmd = ((s5 >> 40) & 0xFFFF) as u16;
    let enable = ACPI_ENABLE.load(Ordering::Relaxed) as u8;
    let mut cnt_a = Port::<u16>::new(pm1a);
    // SAFETY: the PM1 control and SMI command ports the FADT names; the machine is
    // being switched off, nothing else runs.
    unsafe {
        if cnt_a.read() & SCI_EN == 0 && smi_cmd != 0 && enable != 0 {
            // Still in legacy mode: ask the firmware to hand ACPI over first.
            Port::<u8>::new(smi_cmd).write(enable);
            crate::dev::wait::until(1_000, || cnt_a.read() & SCI_EN != 0);
        }
        println!("[kernel] switching off through ACPI S5");
        cnt_a.write((typ_a << 10) | SLP_EN);
        if pm1b != 0 {
            Port::<u16>::new(pm1b).write((typ_b << 10) | SLP_EN);
        }
    }
    // The power goes within moments; if it has not after a second, it will not.
    crate::dev::wait::pause(1_000);
}
