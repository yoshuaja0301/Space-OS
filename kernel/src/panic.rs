//! Crash log: what the PRD calls "diagnosis panic" (stage 1 exit criterion).
//!
//! On panic we print the message, source location, a frame-pointer backtrace and
//! then tell QEMU to exit with [`spaceabi::syscall::qemu_exit::PANIC`] so the test
//! harness can distinguish a panic from a hang or a clean shutdown.

use core::panic::PanicInfo;
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use spaceabi::syscall::qemu_exit;

use crate::arch;

static PANICKING: AtomicBool = AtomicBool::new(false);
/// The CPU that panicked first (the one writing the report).
static PANIC_CPU: AtomicUsize = AtomicUsize::new(usize::MAX);

/// A panic is under way (the NMI that stops the other CPUs checks this). AArch64
/// runs one CPU so far, and nothing there asks.
#[cfg_attr(target_arch = "aarch64", allow(dead_code))]
pub fn in_progress() -> bool {
    PANICKING.load(Ordering::SeqCst)
}

macro_rules! eprint {
    ($($arg:tt)*) => {
        // SAFETY: panic path, interrupts disabled, we never return.
        unsafe { $crate::console::emergency_print(format_args!($($arg)*)) }
    };
}

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    arch::disable_interrupts();
    let cpu = arch::percpu::index();
    if PANICKING.swap(true, Ordering::SeqCst) {
        if PANIC_CPU.load(Ordering::SeqCst) != cpu {
            // Another CPU panicked at the same moment and is writing its report:
            // stop here and let it finish (it ends the machine).
            arch::halt_forever();
        }
        eprint!("\n!!! nested panic, halting\n");
        arch::qemu_exit(qemu_exit::PANIC);
        arch::halt_forever();
    }
    PANIC_CPU.store(cpu, Ordering::SeqCst);
    // Stop the other CPUs where they are, so nothing changes under the report.
    arch::smp::halt_others();
    crate::fb::take_back_for_panic();
    eprint!("\n!!! KERNEL PANIC !!!\n");
    if let Some(loc) = info.location() {
        eprint!("at {}:{}:{}\n", loc.file(), loc.line(), loc.column());
    }
    eprint!("message: {}\n", info.message());
    if let Some(t) = crate::sched::try_current_name() {
        eprint!("current: {} on cpu {}\n", t, cpu);
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
    // The frame record is the same on both architectures: the caller's frame
    // pointer at [fp], the return address at [fp + 8].
    let mut rbp = arch::frame_pointer();
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
