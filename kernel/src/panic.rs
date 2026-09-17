//! Crash log: what the PRD calls "diagnosis panic" (stage 1 exit criterion).
//!
//! On panic we print the message, source location, a frame-pointer backtrace and
//! then tell QEMU to exit with [`spaceabi::syscall::qemu_exit::PANIC`] so the test
//! harness can distinguish a panic from a hang or a clean shutdown.

use core::panic::PanicInfo;
use core::sync::atomic::{AtomicBool, Ordering};

use spaceabi::syscall::qemu_exit;

use crate::arch;

static PANICKING: AtomicBool = AtomicBool::new(false);

macro_rules! eprint {
    ($($arg:tt)*) => {
        // SAFETY: panic path, interrupts disabled, we never return.
        unsafe { $crate::console::emergency_print(format_args!($($arg)*)) }
    };
}

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    arch::disable_interrupts();
    if PANICKING.swap(true, Ordering::SeqCst) {
        eprint!("\n!!! nested panic, halting\n");
        arch::qemu_exit(qemu_exit::PANIC);
        arch::halt_forever();
    }
    eprint!("\n!!! KERNEL PANIC !!!\n");
    if let Some(loc) = info.location() {
        eprint!("at {}:{}:{}\n", loc.file(), loc.line(), loc.column());
    }
    eprint!("message: {}\n", info.message());
    if let Some(t) = crate::sched::try_current_name() {
        eprint!("current: {}\n", t);
    }
    backtrace();
    eprint!("spacekernel: halted after panic (uptime {} ms)\n", crate::sched::uptime_ms());
    arch::qemu_exit(qemu_exit::PANIC);
    arch::halt_forever();
}

/// Walk the frame-pointer chain (`-C force-frame-pointers=yes`). Every candidate
/// frame address is checked against the page tables before it is dereferenced so a
/// corrupted chain cannot turn a panic into a nested fault.
pub fn backtrace() {
    let mut rbp: u64;
    // SAFETY: reading a register.
    unsafe { core::arch::asm!("mov {}, rbp", out(reg) rbp, options(nomem, nostack)) };
    eprint!("backtrace (frame pointers):\n");
    let mut depth = 0;
    while rbp != 0 && depth < 32 {
        if rbp & 7 != 0
            || !crate::mm::kernel_addr_is_mapped(rbp)
            || !crate::mm::kernel_addr_is_mapped(rbp + 8)
        {
            eprint!("  (frame chain ends at unmapped {:#x})\n", rbp);
            break;
        }
        // SAFETY: both words are mapped kernel memory (checked above).
        let ret = unsafe { *((rbp + 8) as *const u64) };
        let next = unsafe { *(rbp as *const u64) };
        if ret == 0 {
            break;
        }
        eprint!("  #{depth:02} {ret:#018x}\n");
        if next <= rbp {
            break;
        }
        rbp = next;
        depth += 1;
    }
}
