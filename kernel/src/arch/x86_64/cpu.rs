//! Processor facts the kernel decides for everyone: the FP/SIMD units, which belong
//! to each thread (`super::fpu`, ADR-0031), and whether the CPU offers RDRAND.

use core::arch::asm;
use core::sync::atomic::{AtomicBool, Ordering};

static RDRAND: AtomicBool = AtomicBool::new(false);

fn cpuid_ecx(leaf: u32) -> u32 {
    core::arch::x86_64::__cpuid(leaf).ecx
}

pub fn init() {
    super::fpu::init_cpu(true);
    let rdrand = cpuid_ecx(1) & (1 << 30) != 0;
    RDRAND.store(rdrand, Ordering::Relaxed);
    println!("[kernel] cpu: RDRAND {}", if rdrand { "present" } else { "absent" });
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
