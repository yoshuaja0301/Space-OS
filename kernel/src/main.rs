//! `spacekernel` – the Space OS microkernel (stage 1 + 2 of the PRD roadmap).
//!
//! What lives in the kernel (PRD §2 "Batas kernel dan layanan"): address spaces,
//! threads, IPC channels, capability handles, timer, interrupts, memory quotas.
//! Everything else (services, inference, SpaceLink, the shell) is user space.
//!
//! Boot flow: `spaceboot` (UEFI) → [`_start`] → [`kmain`] → `bin/init` from the initrd.

#![no_std]
#![no_main]
#![deny(unsafe_op_in_unsafe_fn)]

extern crate alloc;

#[macro_use]
mod console;

mod arch;
mod cmdline;
mod dev;
mod fb;
mod fs;
mod initrd;
mod input;
mod ipc;
mod mm;
mod panic;
mod proc;
mod sched;
mod selftest;
mod sync;
mod syscall;

use core::arch::{asm, naked_asm};

use spaceabi::boot::BootInfo;

use crate::sync::StaticCell;

/// The BootInfo pointer, stashed so `kmain_on_guarded_stack` can find it after the
/// stack switch.
static BOOT_INFO: StaticCell<*const BootInfo> = StaticCell::new(core::ptr::null());

pub const KERNEL_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Entry point. `spaceboot` jumps here with `rdi = &BootInfo`, interrupts disabled, on
/// the boot stack. We re-align the stack for the SysV ABI and never return.
#[unsafe(naked)]
#[unsafe(no_mangle)]
pub extern "C" fn _start() -> ! {
    naked_asm!(
        "and rsp, -16",
        "xor rbp, rbp",
        "call {kmain}",
        "ud2",
        kmain = sym kmain,
    )
}

extern "C" fn kmain(boot_info: *const BootInfo) -> ! {
    arch::serial::init();
    println!();
    println!("spacekernel {KERNEL_VERSION}: Space OS kernel booting");

    // SAFETY: spaceboot guarantees a valid, readable BootInfo behind `rdi`.
    let bi: &'static BootInfo = unsafe { &*boot_info };
    if !bi.is_valid() {
        panic!("invalid BootInfo (magic {:#x}, version {})", bi.magic, bi.version);
    }
    println!(
        "[kernel] boot info ok: {} memory regions, initrd {} bytes, cmdline {} bytes, rsdp {:#x}",
        bi.memory_map_entries, bi.initrd.len, bi.cmdline.len, bi.rsdp
    );

    arch::gdt::init();
    arch::idt::init();
    mm::init(bi);

    // The bootloader's stack lives inside a 2 MiB page of the linear map and has no
    // guard page: an overflow there would silently corrupt boot data. Move the boot
    // context (which becomes the idle thread) onto a guarded kernel-stack slot.
    let stack = mm::kstack::KernelStack::new().expect("guarded kernel stack for the boot context");
    let top = stack.top;
    core::mem::forget(stack); // lives for the whole kernel lifetime
    // SAFETY: single CPU, early boot; the pointer is read once on the new stack.
    unsafe { *BOOT_INFO.get_mut() = boot_info };
    // SAFETY: switches to a freshly mapped stack and never returns; operands are
    // pinned to explicit registers so nothing in the sequence clobbers them.
    unsafe {
        asm!(
            "mov rsp, rax",
            "xor ebp, ebp",
            "call {f}",
            "ud2",
            in("rax") top,
            f = sym kmain_on_guarded_stack,
            options(noreturn)
        )
    }
}

extern "C" fn kmain_on_guarded_stack() -> ! {
    // SAFETY: set by `kmain` right before the stack switch.
    let bi: &'static BootInfo = unsafe { &**BOOT_INFO.get() };
    let mut rsp: u64;
    // SAFETY: reading a register.
    unsafe { asm!("mov {}, rsp", out(reg) rsp, options(nomem, nostack)) };
    let top = (rsp + mm::kstack::SLOT_SIZE - 1) & !(mm::kstack::SLOT_SIZE - 1);
    println!("[kernel] boot context moved to a guarded kernel stack (top {top:#x})");

    fb::init(bi);
    cmdline::init(bi);
    initrd::init(bi);
    dev::init();
    fs::init();
    arch::pic::init();
    println!(
        "[kernel] console input: keyboard (IRQ1){}",
        if arch::serial::init_input() { " and COM2 serial (IRQ3)" } else { "; no COM2 UART" }
    );
    arch::pit::init(sched::TICK_HZ);
    arch::syscall::init();
    sched::init(top);
    selftest::run_early();
    selftest::run_cmdline_fault_injection();

    match proc::spawn_init() {
        Ok(p) => println!("[kernel] init spawned as pid {}", p.pid),
        Err(e) => panic!("cannot start bin/init from initrd: {e}"),
    }
    println!("[kernel] entering idle loop; scheduler live");
    sched::idle_loop()
}
