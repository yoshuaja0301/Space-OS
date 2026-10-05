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

mod acpi;
mod arch;
mod clock;
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

use spaceabi::boot::BootInfo;

use crate::sync::StaticCell;

/// The BootInfo pointer, stashed so `kmain_on_guarded_stack` can find it after the
/// stack switch.
static BOOT_INFO: StaticCell<*const BootInfo> = StaticCell::new(core::ptr::null());

pub const KERNEL_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Reached from the architecture's entry point (`arch::entry`) on the boot stack,
/// interrupts disabled, with the bootloader's `BootInfo`. Never returns.
extern "C" fn kmain(boot_info: *const BootInfo) -> ! {
    arch::serial::init(boot_info);
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

    arch::init_cpu();
    mm::init(bi);
    arch::init_after_mm(bi);

    // The bootloader's stack lives inside a 2 MiB page of the linear map and has no
    // guard page: an overflow there would silently corrupt boot data. Move the boot
    // context (which becomes the idle thread) onto a guarded kernel-stack slot.
    let stack = mm::kstack::KernelStack::new().expect("guarded kernel stack for the boot context");
    let top = stack.top;
    core::mem::forget(stack); // lives for the whole kernel lifetime
    // SAFETY: single CPU, early boot; the pointer is read once on the new stack.
    unsafe { *BOOT_INFO.get_mut() = boot_info };
    arch::run_on_stack(top, kmain_on_guarded_stack)
}

extern "C" fn kmain_on_guarded_stack() -> ! {
    // SAFETY: set by `kmain` right before the stack switch.
    let bi: &'static BootInfo = unsafe { &**BOOT_INFO.get() };
    let rsp = arch::stack_pointer();
    let top = (rsp + mm::kstack::SLOT_SIZE - 1) & !(mm::kstack::SLOT_SIZE - 1);
    println!("[kernel] boot context moved to a guarded kernel stack (top {top:#x})");

    fb::init(bi);
    cmdline::init(bi);
    initrd::init(bi);
    dev::init();
    fs::init();
    arch::init_platform(bi, sched::TICK_HZ);
    clock::init(bi.rsdp);
    sched::init(top);
    arch::rtc::init(sched::uptime_ms());
    selftest::run_early();
    selftest::run_cmdline_fault_injection();
    // How to switch off comes first: on AArch64 the same firmware interface (PSCI)
    // also starts the other processors.
    arch::power::init(bi.rsdp);
    // The other processors join last: everything they use exists by now, and the
    // self-tests above ran with nobody else around.
    arch::smp::init(bi.rsdp);

    match proc::spawn_init() {
        Ok((p, name)) => println!("[kernel] init spawned as pid {} from {name}", p.pid),
        // Name the program that actually failed: with `init=` on the command line it
        // is not necessarily bin/init, and a wrong name is the likeliest reason.
        Err(e) => panic!("cannot start {:?} from initrd: {e}", cmdline::get("init").unwrap_or("bin/init")),
    }
    println!("[kernel] entering idle loop; scheduler live");
    sched::idle_loop()
}
