//! Kernel-stack context switch and the first entry into EL0.

use core::arch::{asm, global_asm};

global_asm!(
    ".section .text",
    ".global switch_to",
    // switch_to(prev_sp_slot: *mut u64 [x0], next_sp: u64 [x1])
    "switch_to:",
    "    sub sp, sp, #96",
    "    stp x19, x20, [sp, #0]",
    "    stp x21, x22, [sp, #16]",
    "    stp x23, x24, [sp, #32]",
    "    stp x25, x26, [sp, #48]",
    "    stp x27, x28, [sp, #64]",
    "    stp x29, x30, [sp, #80]",
    "    mov x9, sp",
    "    str x9, [x0]",
    "    mov sp, x1",
    "    ldp x19, x20, [sp, #0]",
    "    ldp x21, x22, [sp, #16]",
    "    ldp x23, x24, [sp, #32]",
    "    ldp x25, x26, [sp, #48]",
    "    ldp x27, x28, [sp, #64]",
    "    ldp x29, x30, [sp, #80]",
    "    add sp, sp, #96",
    "    ret",
);

unsafe extern "C" {
    /// Save callee-saved registers on the current stack, store `sp` into
    /// `*prev_sp_slot`, load `next_sp` and resume there.
    pub fn switch_to(prev_sp_slot: *mut u64, next_sp: u64);
}

/// Prepare a fresh kernel stack so that `switch_to` into it "returns" into `entry`.
/// Returns the initial `sp`.
///
/// # Safety
/// `stack_top` must be the 16-byte aligned top of a mapped kernel stack.
pub unsafe fn prepare_initial_stack(stack_top: u64, entry: extern "C" fn() -> !) -> u64 {
    // Twelve saved registers (x19-x30), all zero but x30, the "return" address.
    let sp = stack_top - 96;
    // SAFETY: the 96 bytes below the top of the caller's mapped stack.
    unsafe {
        core::ptr::write_bytes(sp as *mut u64, 0, 12);
        *((sp + 88) as *mut u64) = entry as usize as u64;
    }
    sp
}

/// Drop to EL0 with a clean register file and interrupts enabled.
///
/// # Safety
/// `pc`/`sp` must be mapped user addresses in the active address space, and this
/// CPU's kernel stack (`set_kernel_stack`) must be the running thread's.
pub unsafe fn enter_user(pc: u64, sp: u64) -> ! {
    let kernel_top = super::percpu::kernel_stack();
    // SAFETY: per the contract. SPSR = EL0t with DAIF clear.
    unsafe {
        asm!(
            "msr sp_el0, x1",
            "msr elr_el1, x0",
            "msr spsr_el1, xzr",
            "mov sp, x2",
            "mov x0, xzr", "mov x1, xzr", "mov x2, xzr", "mov x3, xzr",
            "mov x4, xzr", "mov x5, xzr", "mov x6, xzr", "mov x7, xzr",
            "mov x8, xzr", "mov x9, xzr", "mov x10, xzr", "mov x11, xzr",
            "mov x12, xzr", "mov x13, xzr", "mov x14, xzr", "mov x15, xzr",
            "mov x16, xzr", "mov x17, xzr", "mov x18, xzr", "mov x19, xzr",
            "mov x20, xzr", "mov x21, xzr", "mov x22, xzr", "mov x23, xzr",
            "mov x24, xzr", "mov x25, xzr", "mov x26, xzr", "mov x27, xzr",
            "mov x28, xzr", "mov x29, xzr", "mov x30, xzr",
            "eret",
            in("x0") pc,
            in("x1") sp,
            in("x2") kernel_top,
            options(noreturn)
        )
    }
}
