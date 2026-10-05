//! Processor state the kernel decides for everyone: no x87/SSE/AVX for user space,
//! and whether the CPU offers RDRAND.
//!
//! The kernel keeps no floating-point or vector registers per thread -- nothing it
//! runs is built to use them (`x86_64-unknown-none` is a soft-float target). The
//! firmware, though, hands over a CPU with SSE switched on, and left that way one
//! process could read the XMM registers another one left behind. So the unit is
//! switched off instead: with CR0.EM set and CR4.OSFXSR/OSXSAVE clear, the first
//! such instruction kills the process that tried it -- x87 raises `#NM`, SSE and AVX
//! raise `#UD` -- rather than handing it someone else's state.

use core::arch::asm;
use core::sync::atomic::{AtomicBool, Ordering};

use x86_64::registers::control::{Cr0, Cr0Flags, Cr4, Cr4Flags};

static RDRAND: AtomicBool = AtomicBool::new(false);

fn cpuid_ecx(leaf: u32) -> u32 {
    core::arch::x86_64::__cpuid(leaf).ecx
}

/// The unit off on the calling CPU. Control registers are per CPU, so every CPU
/// does this for itself before it runs anything of user space.
pub fn fpu_off() {
    // SAFETY: nothing in the kernel uses the x87/SSE/AVX state these bits govern.
    unsafe {
        Cr0::update(|f| {
            f.insert(Cr0Flags::EMULATE_COPROCESSOR);
            f.remove(Cr0Flags::MONITOR_COPROCESSOR | Cr0Flags::TASK_SWITCHED);
        });
        Cr4::update(|f| f.remove(Cr4Flags::OSFXSR | Cr4Flags::OSXMMEXCPT_ENABLE | Cr4Flags::OSXSAVE));
    }
}

pub fn init() {
    fpu_off();
    let rdrand = cpuid_ecx(1) & (1 << 30) != 0;
    RDRAND.store(rdrand, Ordering::Relaxed);
    println!(
        "[kernel] cpu: x87/SSE/AVX off for user space (no per-thread FPU state); RDRAND {}",
        if rdrand { "present" } else { "absent" }
    );
}

/// The time-stamp counter: a count that only goes up, at a rate this kernel does
/// not measure. Every x86_64 CPU has one.
/// Counts of [`timestamp`] per millisecond, for device waits: an upper bound, as the
/// TSC rate is not known this early (see `dev::wait`).
pub fn counts_per_ms() -> u64 {
    5_000_000
}

pub fn timestamp() -> u64 {
    // SAFETY: RDTSC reads a counter and has no other effect.
    unsafe { core::arch::x86_64::_rdtsc() }
}

/// 64 bits from RDRAND, or `None` when the CPU has none or keeps failing. Intel
/// recommends ten retries: a failure means the generator is momentarily drained.
pub fn rdrand64() -> Option<u64> {
    if !RDRAND.load(Ordering::Relaxed) {
        return None;
    }
    for _ in 0..10 {
        let v: u64;
        let ok: u8;
        // SAFETY: RDRAND is present (CPUID above); it only writes the output
        // register and the carry flag.
        unsafe {
            asm!("rdrand {v}", "setc {ok}", v = out(reg) v, ok = out(reg_byte) ok, options(nomem, nostack))
        };
        if ok != 0 {
            return Some(v);
        }
    }
    None
}
