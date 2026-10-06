//! `syscall`/`sysret` fast path.
//!
//! Entry runs with interrupts masked (SFMASK). We stash the user stack pointer,
//! switch to the current thread's kernel stack, push a [`SyscallFrame`], and call
//! `syscall_dispatch`. The dispatcher may enable interrupts (and be preempted or
//! block); the epilogue re-disables them before `sysretq`.
//!
//! The kernel stack comes from this CPU's [`PerCpu`](super::percpu::PerCpu), found
//! through `GS`: `swapgs` in, two moves, `swapgs` back out, before anything else
//! runs. Nothing else in the kernel uses `GS`, so no other path has to know which
//! base is loaded -- the NMI and debug traps that can land inside this window run on
//! their own stacks and do not look.

use core::arch::global_asm;

use x86_64::VirtAddr;
use x86_64::registers::model_specific::{Efer, EferFlags, GsBase, KernelGsBase, LStar, SFMask, Star};
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

impl SyscallFrame {
    pub fn number(&self) -> u64 {
        self.nr
    }

    pub fn args(&self) -> [u64; 6] {
        [self.arg0, self.arg1, self.arg2, self.arg3, self.arg4, self.arg5]
    }

    /// Where the process resumes (`sysret` loads it from `rcx`).
    pub fn return_address(&self) -> u64 {
        self.rip
    }
}

global_asm!(
    ".section .text",
    ".global syscall_entry",
    "syscall_entry:",
    "    swapgs",
    "    mov qword ptr gs:[8], rsp",
    "    mov rsp, qword ptr gs:[0]",
    "    push qword ptr gs:[8]",
    "    swapgs",
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

/// Program `syscall` on the calling CPU, which is CPU `cpu`: the MSRs are per CPU.
pub fn init_cpu(cpu: usize) {
    let sel = super::gdt::selectors();
    // SAFETY: MSR programming for the syscall instruction on this CPU. `GS` holds 0
    // (loaded by `gdt`), and its base is set to 0 after that; the kernel's base
    // waits in `KERNEL_GS_BASE` for `syscall_entry`.
    unsafe {
        Efer::write(Efer::read() | EferFlags::SYSTEM_CALL_EXTENSIONS);
        Star::write(sel.user_code, sel.user_data, sel.kernel_code, sel.kernel_data)
            .expect("GDT layout must satisfy STAR constraints");
        LStar::write(VirtAddr::new(syscall_entry as *const () as usize as u64));
        SFMask::write(RFlags::INTERRUPT_FLAG | RFlags::TRAP_FLAG | RFlags::DIRECTION_FLAG);
        GsBase::write(VirtAddr::new(0));
        KernelGsBase::write(VirtAddr::new(super::percpu::block(cpu) as u64));
    }
}

pub fn init() {
    init_cpu(0);
    println!("[kernel] syscall/sysret enabled (ABI v{})", spaceabi::ABI_VERSION);
}

/// Kernel stack used by the next `syscall` from user mode on this CPU.
pub fn set_kernel_stack(top: u64) {
    super::percpu::set_syscall_stack(top);
}

#[unsafe(no_mangle)]
extern "C" fn syscall_dispatch(frame: &mut SyscallFrame) -> isize {
    crate::syscall::dispatch(frame)
}
