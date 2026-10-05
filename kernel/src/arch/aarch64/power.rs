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
