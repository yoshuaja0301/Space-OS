//! Other processors. For now the kernel runs on the boot CPU only on AArch64; the
//! others stay where the firmware parked them (PSCI CPU_ON would start them) and
//! the boot log says how many there are.

use crate::acpi;

pub fn init(rsdp: u64) {
    let listed = acpi::find(rsdp, b"APIC")
        .map(|madt| {
            acpi::madt_entries(madt)
                .filter(|(kind, e)| *kind == 0x0B && e.len() >= 16 && acpi::u32_at(e, 12) & 1 != 0)
                .count()
        })
        .unwrap_or(1);
    if listed <= 1 {
        println!("[kernel] smp: one processor");
    } else {
        println!(
            "[kernel] smp: running on the boot CPU only; {} more listed stay parked (not started on AArch64 yet)",
            listed - 1
        );
    }
}

/// Tell CPU `cpu` to look at the run queue: only one runs.
pub fn kick(_cpu: usize) {}

/// Stop every other CPU (panic path): there are none running.
pub fn halt_others() {}
