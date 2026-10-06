//! Switching the machine off: PSCI `SYSTEM_OFF`, through the conduit the FADT
//! names (ARM boot architecture flags: bit 0 PSCI compliant, bit 1 use HVC).

use core::arch::asm;
use core::sync::atomic::{AtomicU8, Ordering};

use crate::acpi;

const NONE: u8 = 0;
const SMC: u8 = 1;
const HVC: u8 = 2;
static CONDUIT: AtomicU8 = AtomicU8::new(NONE);
const SYSTEM_OFF: u64 = 0x8400_0008;
const CPU_ON: u64 = 0xC400_0003;

/// True when the FADT named a PSCI conduit.
pub fn psci_present() -> bool {
    CONDUIT.load(Ordering::Relaxed) != NONE
}

/// A PSCI call with three arguments; the result (0 on success, a negative PSCI
/// error otherwise, -1 "not supported" without a conduit).
fn psci(function: u64, a1: u64, a2: u64, a3: u64) -> i64 {
    let r: u64;
    // SAFETY: a PSCI call through the conduit the firmware named; the arguments
    // are what the call takes.
    unsafe {
        match CONDUIT.load(Ordering::Relaxed) {
            HVC => {
                asm!("hvc #0", inout("x0") function => r, in("x1") a1, in("x2") a2, in("x3") a3, options(nostack))
            }
            SMC => {
                asm!("smc #0", inout("x0") function => r, in("x1") a1, in("x2") a2, in("x3") a3, options(nostack))
            }
            _ => return -1,
        }
    }
    r as i64
}

/// Start the CPU with affinity `target` at physical `entry`, with `context` in its
/// `x0` (PSCI `CPU_ON`, 64-bit).
pub fn cpu_on(target: u64, entry: u64, context: u64) -> i64 {
    psci(CPU_ON, target, entry, context)
}

pub fn init(rsdp: u64) {
    let flags = acpi::find(rsdp, b"FACP")
        .ok()
        .filter(|f| f.len() >= 131)
        .map(|f| u16::from_le_bytes([f[129], f[130]]));
    match flags {
        Some(f) if f & 1 != 0 => {
            let hvc = f & 2 != 0;
            CONDUIT.store(if hvc { HVC } else { SMC }, Ordering::Relaxed);
            println!("[kernel] power: PSCI SYSTEM_OFF through {}", if hvc { "HVC" } else { "SMC" });
        }
        _ => println!("[kernel] power: no PSCI in the FADT; shutting down halts the machine"),
    }
}

/// Switch the machine off. Returns only if it did not go off.
pub fn poweroff() {
    println!("[kernel] switching off through PSCI SYSTEM_OFF");
    // SAFETY: a PSCI call; SYSTEM_OFF does not return when it works.
    unsafe {
        match CONDUIT.load(Ordering::Relaxed) {
            HVC => asm!("hvc #0", inout("x0") SYSTEM_OFF => _, options(nostack)),
            SMC => asm!("smc #0", inout("x0") SYSTEM_OFF => _, options(nostack)),
            _ => return,
        }
    }
    crate::dev::wait::pause(1_000);
}
