//! Kernel-stack context switch and the ring 3 entry sequence.

use core::arch::{asm, global_asm};

global_asm!(
    ".section .text",
    ".global switch_to",
    // switch_to(prev_rsp_slot: *mut u64 [rdi], next_rsp: u64 [rsi])
    "switch_to:",
    "    push rbp",
    "    push rbx",
    "    push r12",
    "    push r13",
    "    push r14",
    "    push r15",
    "    mov [rdi], rsp",
    "    mov rsp, rsi",
    "    pop r15",
    "    pop r14",
    "    pop r13",
    "    pop r12",
    "    pop rbx",
    "    pop rbp",
    "    ret",
);

unsafe extern "C" {
    /// Save callee-saved registers on the current stack, store `rsp` into
    /// `*prev_rsp_slot`, load `next_rsp` and resume there.
    pub fn switch_to(prev_rsp_slot: *mut u64, next_rsp: u64);
}

/// Prepare a fresh kernel stack so that `switch_to` into it "returns" into `entry`.
/// Returns the initial `rsp`.
///
/// # Safety
/// `stack_top` must be the 16-byte aligned top of a mapped kernel stack.
pub unsafe fn prepare_initial_stack(stack_top: u64, entry: extern "C" fn() -> !) -> u64 {
    // Layout (growing down): entry return address, then 6 zeroed callee-saved slots.
    let mut sp = stack_top;
    // Keep the stack aligned so `entry` sees rsp ≡ 8 (mod 16) after the `ret`.
    sp -= 8;
    unsafe {
        sp -= 8;
        *(sp as *mut u64) = entry as usize as u64;
        for _ in 0..6 {
            sp -= 8;
            *(sp as *mut u64) = 0;
        }
    }
    sp
}

/// Drop to ring 3 with a clean register file.
///
/// # Safety
/// `rip`/`rsp` must be mapped user addresses in the active address space; the TSS
/// and `SYSCALL_KERNEL_RSP` must already point at this thread's kernel stack.
pub unsafe fn enter_user(rip: u64, rsp: u64) -> ! {
    let sel = super::gdt::selectors();
    let user_cs = sel.user_code.0 as u64;
    let user_ss = sel.user_data.0 as u64;
    const RFLAGS_IF: u64 = 0x202;
    unsafe {
        asm!(
            "push {ss}",
            "push {rsp}",
            "push {rflags}",
            "push {cs}",
            "push {rip}",
            "xor eax, eax",
            "xor ebx, ebx",
            "xor ecx, ecx",
            "xor edx, edx",
            "xor esi, esi",
            "xor edi, edi",
            "xor ebp, ebp",
            "xor r8d, r8d",
            "xor r9d, r9d",
            "xor r10d, r10d",
            "xor r11d, r11d",
            "xor r12d, r12d",
            "xor r13d, r13d",
            "xor r14d, r14d",
            "xor r15d, r15d",
            "iretq",
            ss = in(reg) user_ss,
            rsp = in(reg) rsp,
            rflags = const RFLAGS_IF,
            cs = in(reg) user_cs,
            rip = in(reg) rip,
            options(noreturn)
        )
    }
}
