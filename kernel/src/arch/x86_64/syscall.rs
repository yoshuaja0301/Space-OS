//! `syscall`/`sysret` fast path.
//!
//! Entry runs with interrupts masked (SFMASK). We stash the user stack pointer,
//! switch to the current thread's kernel stack, push a [`SyscallFrame`], and call
//! `syscall_dispatch`. The dispatcher may enable interrupts (and be preempted or
//! block); the epilogue re-disables them before `sysretq`.
//!
//! Single-CPU simplification: the kernel stack pointer lives in a global that the
//! scheduler updates on every context switch (SMP would use `swapgs` + per-CPU data).

use core::arch::global_asm;

use x86_64::VirtAddr;
use x86_64::registers::model_specific::{Efer, EferFlags, LStar, SFMask, Star};
use x86_64::registers::rflags::RFlags;

/// Saved user state on syscall entry (lowest address first).
#[repr(C)]
#[derive(Debug)]
pub struct SyscallFrame {
    pub arg5: u64,   // r9
    pub arg4: u64,   // r8
    pub arg3: u64,   // r10
    pub arg2: u64,   // rdx
    pub arg1: u64,   // rsi
    pub arg0: u64,   // rdi
    pub nr: u64,     // rax (overwritten with the result)
    pub rip: u64,    // rcx
    pub rflags: u64, // r11
    pub user_rsp: u64,
}

#[unsafe(no_mangle)]
pub static mut SYSCALL_KERNEL_RSP: u64 = 0;
#[unsafe(no_mangle)]
pub static mut SYSCALL_USER_RSP_SCRATCH: u64 = 0;

global_asm!(
    ".section .text",
    ".global syscall_entry",
    "syscall_entry:",
    "    mov [rip + SYSCALL_USER_RSP_SCRATCH], rsp",
    "    mov rsp, [rip + SYSCALL_KERNEL_RSP]",
    "    push [rip + SYSCALL_USER_RSP_SCRATCH]",
    "    push r11",
    "    push rcx",
    "    push rax",
    "    push rdi",
    "    push rsi",
    "    push rdx",
    "    push r10",
    "    push r8",
    "    push r9",
    "    cld",
    "    mov rdi, rsp",
    "    call syscall_dispatch",
    "    cli",
    "    mov [rsp + 48], rax",
    "    pop r9",
    "    pop r8",
    "    pop r10",
    "    pop rdx",
    "    pop rsi",
    "    pop rdi",
    "    pop rax",
    "    pop rcx",
    "    pop r11",
    "    pop rsp",
    "    sysretq",
);

unsafe extern "C" {
    fn syscall_entry();
}

pub fn init() {
    let sel = super::gdt::selectors();
    // SAFETY: MSR programming for the syscall instruction, done once at boot.
    unsafe {
        Efer::write(Efer::read() | EferFlags::SYSTEM_CALL_EXTENSIONS);
        Star::write(sel.user_code, sel.user_data, sel.kernel_code, sel.kernel_data)
            .expect("GDT layout must satisfy STAR constraints");
        LStar::write(VirtAddr::new(syscall_entry as *const () as usize as u64));
        SFMask::write(RFlags::INTERRUPT_FLAG | RFlags::TRAP_FLAG | RFlags::DIRECTION_FLAG);
    }
    println!("[kernel] syscall/sysret enabled (ABI v{})", spaceabi::ABI_VERSION);
}

/// Kernel stack used by the next `syscall` from user mode.
pub fn set_kernel_stack(top: u64) {
    // SAFETY: single CPU; the value is only read by `syscall_entry`.
    unsafe { core::ptr::write_volatile(core::ptr::addr_of_mut!(SYSCALL_KERNEL_RSP), top) };
}

#[unsafe(no_mangle)]
extern "C" fn syscall_dispatch(frame: &mut SyscallFrame) -> isize {
    crate::syscall::dispatch(frame)
}
