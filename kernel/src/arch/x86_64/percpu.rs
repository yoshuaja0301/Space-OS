//! Which CPU is this, and what each CPU keeps for itself.
//!
//! Every CPU has its own TSS, and the TSS descriptors sit side by side in the one
//! GDT, so the task register names the CPU: [`index`] is one `str` away, needs no
//! segment base and works from any context, the NMI handler included. Before a CPU
//! has loaded its task register `str` reads 0, which counts as CPU 0: only the boot
//! CPU runs that early, and an application processor loads its TSS before it does
//! anything else.
//!
//! The one thing that has to be reachable without a free register is the kernel
//! stack for `syscall`: the entry stub finds it through `GS` ([`PerCpu`], pointed to
//! by `IA32_KERNEL_GS_BASE`).

use core::arch::asm;
use core::sync::atomic::{AtomicU32, Ordering};

use crate::sync::StaticCell;

/// Most CPUs the kernel will run on. Further ones stay parked where the firmware
/// left them, and the boot log says so.
pub const MAX_CPUS: usize = 64;

/// Selector of CPU 0's TSS; CPU `i`'s is `TSS_SELECTOR_BASE + 16 * i` (a 64-bit TSS
/// descriptor takes two GDT slots).
pub const TSS_SELECTOR_BASE: u16 = 0x28;

/// Read by `syscall_entry` through `GS`: keep the layout in step with it.
#[repr(C, align(64))]
pub struct PerCpu {
    /// Top of the kernel stack the next `syscall` on this CPU runs on (offset 0).
    pub kernel_rsp: u64,
    /// The user stack pointer while `syscall_entry` switches stacks (offset 8).
    pub user_rsp: u64,
}

static PERCPU: [StaticCell<PerCpu>; MAX_CPUS] =
    [const { StaticCell::new(PerCpu { kernel_rsp: 0, user_rsp: 0 }) }; MAX_CPUS];

/// Local APIC ID of each CPU index (`u32::MAX` while unknown).
static APIC_IDS: [AtomicU32; MAX_CPUS] = [const { AtomicU32::new(u32::MAX) }; MAX_CPUS];

/// Index of the CPU this runs on.
#[inline]
pub fn index() -> usize {
    let sel: u16;
    // SAFETY: `str` only reads the task register.
    unsafe { asm!("str {0:x}", out(reg) sel, options(nomem, nostack, preserves_flags)) };
    if sel < TSS_SELECTOR_BASE { 0 } else { ((sel - TSS_SELECTOR_BASE) / 16) as usize }
}

/// The per-CPU block of CPU `cpu`, for `IA32_KERNEL_GS_BASE`.
pub fn block(cpu: usize) -> *mut PerCpu {
    PERCPU[cpu].get()
}

/// Stack for the next `syscall` on this CPU.
pub fn set_syscall_stack(top: u64) {
    // SAFETY: only this CPU writes its own block (with interrupts disabled, from the
    // scheduler) and only its own `syscall_entry` reads it.
    unsafe { core::ptr::write_volatile(core::ptr::addr_of_mut!((*PERCPU[index()].get()).kernel_rsp), top) };
}

pub fn set_apic_id(cpu: usize, id: u32) {
    APIC_IDS[cpu].store(id, Ordering::Relaxed);
}

pub fn apic_id(cpu: usize) -> u32 {
    APIC_IDS[cpu].load(Ordering::Relaxed)
}
