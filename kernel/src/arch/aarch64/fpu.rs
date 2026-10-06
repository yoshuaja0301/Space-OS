//! FP/SIMD state per thread (ADR-0031): V0-V31, FPCR and FPSR.
//!
//! `CPACR_EL1.FPEN` lets EL0 use the unit (`cpu::init`), and the scheduler saves and
//! loads each thread's registers on every switch (`crate::fpu`). The kernel is built
//! soft-float and never uses them; the two routines below are the only code in it
//! that does, and the build gate checks that nothing else does.

use core::arch::global_asm;

/// 32 128-bit registers, then FPCR and FPSR.
const SIZE: usize = 32 * 16 + 16;

global_asm!(
    ".arch_extension fp",
    ".arch_extension simd",
    ".section .text",
    ".global spaceos_fpsimd_save",
    "spaceos_fpsimd_save:",
    "    stp q0, q1, [x0, #0]",
    "    stp q2, q3, [x0, #32]",
    "    stp q4, q5, [x0, #64]",
    "    stp q6, q7, [x0, #96]",
    "    stp q8, q9, [x0, #128]",
    "    stp q10, q11, [x0, #160]",
    "    stp q12, q13, [x0, #192]",
    "    stp q14, q15, [x0, #224]",
    "    stp q16, q17, [x0, #256]",
    "    stp q18, q19, [x0, #288]",
    "    stp q20, q21, [x0, #320]",
    "    stp q22, q23, [x0, #352]",
    "    stp q24, q25, [x0, #384]",
    "    stp q26, q27, [x0, #416]",
    "    stp q28, q29, [x0, #448]",
    "    stp q30, q31, [x0, #480]",
    "    mrs x1, fpcr",
    "    mrs x2, fpsr",
    "    str x1, [x0, #512]",
    "    str x2, [x0, #520]",
    "    ret",
    ".global spaceos_fpsimd_restore",
    "spaceos_fpsimd_restore:",
    "    ldp q0, q1, [x0, #0]",
    "    ldp q2, q3, [x0, #32]",
    "    ldp q4, q5, [x0, #64]",
    "    ldp q6, q7, [x0, #96]",
    "    ldp q8, q9, [x0, #128]",
    "    ldp q10, q11, [x0, #160]",
    "    ldp q12, q13, [x0, #192]",
    "    ldp q14, q15, [x0, #224]",
    "    ldp q16, q17, [x0, #256]",
    "    ldp q18, q19, [x0, #288]",
    "    ldp q20, q21, [x0, #320]",
    "    ldp q22, q23, [x0, #352]",
    "    ldp q24, q25, [x0, #384]",
    "    ldp q26, q27, [x0, #416]",
    "    ldp q28, q29, [x0, #448]",
    "    ldp q30, q31, [x0, #480]",
    "    ldr x1, [x0, #512]",
    "    ldr x2, [x0, #520]",
    "    msr fpcr, x1",
    "    msr fpsr, x2",
    "    ret",
);

unsafe extern "C" {
    fn spaceos_fpsimd_save(area: *mut u8);
    fn spaceos_fpsimd_restore(area: *const u8);
}

/// Say what a thread's FP/SIMD state is (before the heap exists: no allocation).
pub fn log() {
    println!("[kernel] fpu: V0-V31, FPCR and FPSR per thread, {SIZE} bytes");
}

pub fn area_layout() -> (usize, usize) {
    (SIZE, 16)
}

/// The initial state: every register zero, FPCR zero (round to nearest, no traps,
/// no flush-to-zero), FPSR zero.
///
/// # Safety
/// `area` points to `size` writable bytes.
pub unsafe fn initial_state(area: *mut u8, size: usize) {
    // SAFETY: as the caller promises.
    unsafe { core::ptr::write_bytes(area, 0, size) };
}

/// # Safety
/// `area` is a thread's area (see `crate::fpu::FpuArea::save`).
pub unsafe fn save(area: *mut u8) {
    // SAFETY: an area of `SIZE` bytes, 16-byte aligned.
    unsafe { spaceos_fpsimd_save(area) }
}

/// # Safety
/// `area` holds a state saved by [`save`] or made by [`initial_state`].
pub unsafe fn restore(area: *const u8) {
    // SAFETY: as above.
    unsafe { spaceos_fpsimd_restore(area) }
}
