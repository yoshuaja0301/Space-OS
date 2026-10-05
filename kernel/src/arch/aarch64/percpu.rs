//! Which CPU is this, and what each CPU keeps for itself.
//!
//! `TPIDR_EL1` holds the CPU's index, written once when the CPU starts: [`index`] is
//! one register read and works from any context.

use core::arch::asm;
use core::sync::atomic::{AtomicU64, Ordering};

/// Most CPUs the kernel will run on.
pub const MAX_CPUS: usize = 64;

/// Top of the kernel stack of the thread each CPU runs, for its first `eret` to EL0.
static KERNEL_STACK: [AtomicU64; MAX_CPUS] = [const { AtomicU64::new(0) }; MAX_CPUS];

pub fn init_boot_cpu() {
    // SAFETY: TPIDR_EL1 is the kernel's own; nothing else uses it.
    unsafe { asm!("msr tpidr_el1, xzr", options(nomem, nostack)) };
}

/// Index of the CPU this runs on.
#[inline]
pub fn index() -> usize {
    let i: u64;
    // SAFETY: reading a system register.
    unsafe { asm!("mrs {}, tpidr_el1", out(reg) i, options(nomem, nostack, preserves_flags)) };
    i as usize
}

pub fn set_kernel_stack(top: u64) {
    KERNEL_STACK[index()].store(top, Ordering::Relaxed);
}

pub fn kernel_stack() -> u64 {
    KERNEL_STACK[index()].load(Ordering::Relaxed)
}
