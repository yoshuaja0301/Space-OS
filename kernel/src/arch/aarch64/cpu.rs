//! CPU set-up and the counters every CPU has.

use core::arch::asm;

/// SCTLR_EL1.UMA: EL0 may mask interrupts (DAIF).
const SCTLR_UMA: u64 = 1 << 9;
/// SCTLR_EL1.UCI: EL0 may run cache maintenance by address.
const SCTLR_UCI: u64 = 1 << 26;
/// SCTLR_EL1.WXN: writable implies execute-never.
const SCTLR_WXN: u64 = 1 << 19;
/// SCTLR_EL1.A: alignment checking of every access.
const SCTLR_A: u64 = 1 << 1;
/// SCTLR_EL1.SPAN: when clear, taking an exception sets PSTATE.PAN.
const SCTLR_SPAN: u64 = 1 << 23;

/// Trap FP/SIMD at EL1 and EL0 (`CPACR_EL1.FPEN = 0`): the kernel keeps no FP state
/// per thread, so a process that uses the unit is stopped (`kill_reason::NO_FPU`)
/// instead of corrupting another's registers. The kernel itself is built
/// soft-float and never touches it. EL0 may neither mask interrupts nor maintain
/// caches; whatever the firmware left there, those are privileged from here on.
///
/// The kernel reads and writes user buffers directly, after checking them against
/// the process's page tables (as on x86-64), so Privileged Access Never stays off:
/// SPAN set, so an exception does not turn it on, and PSTATE.PAN cleared where the
/// CPU has it. Unaligned accesses to normal memory are allowed (SCTLR_EL1.A clear),
/// as Rust code expects.
pub fn init() {
    let mut sctlr: u64;
    // SAFETY: reading a system register.
    unsafe { asm!("mrs {}, sctlr_el1", out(reg) sctlr, options(nomem, nostack)) };
    sctlr &= !(SCTLR_UMA | SCTLR_UCI | SCTLR_WXN | SCTLR_A);
    sctlr |= SCTLR_SPAN;
    // SAFETY: CPACR_EL1 only controls access to the FP/SIMD unit; the SCTLR_EL1
    // bits cleared only take permissions away from EL0.
    unsafe {
        asm!(
            "msr cpacr_el1, xzr",
            "msr sctlr_el1, {}",
            "isb",
            in(reg) sctlr,
            options(nomem, nostack)
        )
    };
    let mmfr1: u64;
    // SAFETY: reading an ID register.
    unsafe { asm!("mrs {}, id_aa64mmfr1_el1", out(reg) mmfr1, options(nomem, nostack)) };
    if (mmfr1 >> 20) & 0xF != 0 {
        // SAFETY: `msr pan, #0` (FEAT_PAN, present per the ID register).
        unsafe { asm!(".inst 0xd500409f", options(nomem, nostack)) };
    }
}

/// The generic timer's virtual count.
pub fn timestamp() -> u64 {
    let v: u64;
    // SAFETY: reading a counter; the `isb` keeps it from being read early.
    unsafe { asm!("isb", "mrs {}, cntvct_el0", out(reg) v, options(nomem, nostack)) };
    v
}

/// The counter's rate, as the firmware programmed it.
pub fn frequency() -> u64 {
    let f: u64;
    // SAFETY: reading a system register.
    unsafe { asm!("mrs {}, cntfrq_el0", out(reg) f, options(nomem, nostack)) };
    f
}

/// Counts of [`timestamp`] per millisecond: the generic timer states its rate.
pub fn counts_per_ms() -> u64 {
    (frequency() / 1000).max(1)
}

/// 64 bits from `RNDR` (FEAT_RNG), or `None` when the CPU has none or keeps failing.
pub fn rdrand64() -> Option<u64> {
    let isar0: u64;
    // SAFETY: reading an ID register.
    unsafe { asm!("mrs {}, id_aa64isar0_el1", out(reg) isar0, options(nomem, nostack)) };
    if (isar0 >> 60) & 0xF == 0 {
        return None;
    }
    for _ in 0..10 {
        let (v, nzcv): (u64, u64);
        // SAFETY: RNDR (s3_3_c2_c4_0) reads a random number and sets NZCV; Z set means
        // no number this time.
        unsafe {
            asm!(
                "mrs {v}, s3_3_c2_c4_0",
                "mrs {f}, nzcv",
                v = out(reg) v,
                f = out(reg) nzcv,
                options(nomem, nostack)
            )
        };
        if nzcv & (1 << 30) == 0 {
            return Some(v);
        }
    }
    None
}
