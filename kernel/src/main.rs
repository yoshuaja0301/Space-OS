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
mod fb;
mod initrd;
mod ipc;
mod mm;
mod panic;
mod proc;
mod sched;
mod selftest;
mod sync;
mod syscall;

use core::arch::naked_asm;

use spaceabi::boot::BootInfo;

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
    fb::init(bi);
    cmdline::init(bi);
    initrd::init(bi);
    arch::pic::init();
    arch::pit::init(sched::TICK_HZ);
    arch::syscall::init();
    sched::init();
    selftest::run_early();
    selftest::run_cmdline_fault_injection();

    match proc::spawn_init() {
        Ok(p) => println!("[kernel] init spawned as pid {}", p.pid),
        Err(e) => panic!("cannot start bin/init from initrd: {e}"),
    }
    println!("[kernel] entering idle loop; scheduler live");
    sched::idle_loop()
}
