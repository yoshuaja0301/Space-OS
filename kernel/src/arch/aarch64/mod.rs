//! AArch64 (ADR-0028): EL1 kernel, EL0 processes, GICv3, the generic timer, a PL011
//! console, PCI through ECAM, power-off through PSCI.
//!
//! The kernel is built for `aarch64-unknown-none-softfloat` and the FP/SIMD unit is
//! trapped at EL1 and EL0 (`cpu::init`), for the same reason x86-64 switches x87 and
//! SSE off: no FP state is kept per thread.

pub mod clock;
pub mod context;
pub mod cpu;
pub mod exceptions;
pub mod gic;
pub mod mmu;
pub mod pci;
pub mod percpu;
pub mod power;
pub mod ps2;
pub mod rtc;
pub mod serial;
pub mod smp;
pub mod syscall;
pub mod timer;

use core::arch::asm;
use core::sync::atomic::{AtomicBool, Ordering};

use spaceabi::boot::BootInfo;

/// Entry point. `spaceboot` jumps here with `x0 = &BootInfo`, `DAIF` masked, on the
/// boot stack (16-byte aligned). Never returns.
#[unsafe(naked)]
#[unsafe(no_mangle)]
pub extern "C" fn _start() -> ! {
    // TPIDR_EL1 holds the CPU index (`percpu`), which the console's lock reads
    // from the very first line printed: zero it before anything else.
    core::arch::naked_asm!(
        "msr spsel, #1",
        "msr tpidr_el1, xzr",
        "mov x29, xzr",
        "mov x30, xzr",
        "bl {kmain}",
        "brk #0",
        kmain = sym crate::kmain,
    )
}

/// This CPU's exception vectors and FP trap, before memory management.
pub fn init_cpu() {
    percpu::init_boot_cpu();
    exceptions::init();
    cpu::init();
    mmu::disable_lower_half();
}

/// After memory management, before devices: PCI configuration space (ECAM) and the
/// firmware facts the platform needs later.
pub fn init_after_mm(bi: &BootInfo) {
    rtc::set_boot_time(bi.boot_time);
    pci::init(bi.rsdp);
}

/// Interrupt controller, console input, the tick and system calls; after devices,
/// before the scheduler.
pub fn init_platform(bi: &BootInfo, tick_hz: u32) {
    if crate::cmdline::get("exit") == Some("semihosting") {
        SEMIHOSTING.store(true, Ordering::Relaxed);
    }
    if let Err(why) = gic::init(bi.rsdp) {
        panic!("interrupt controller: {why}");
    }
    match serial::init_input(bi.rsdp) {
        Some(intid) => println!("[kernel] console input: PL011 UART (INTID {intid})"),
        None => println!("[kernel] console input: none (no PL011 UART with an interrupt)"),
    }
    timer::init(bi.rsdp, tick_hz);
    syscall::init();
}

pub fn enable_interrupts() {
    // SAFETY: unmasking IRQs; the vectors are installed.
    unsafe { asm!("msr daifclr, #2", options(nomem, nostack)) };
}

pub fn disable_interrupts() {
    // SAFETY: masking IRQs.
    unsafe { asm!("msr daifset, #2", options(nomem, nostack)) };
}

pub fn interrupts_enabled() -> bool {
    let daif: u64;
    // SAFETY: reading PSTATE.DAIF.
    unsafe { asm!("mrs {}, daif", out(reg) daif, options(nomem, nostack)) };
    daif & (1 << 7) == 0
}

pub fn hlt() {
    // SAFETY: waits for an interrupt; no other effect.
    unsafe { asm!("wfi", options(nomem, nostack)) };
}

/// Sleep until an interrupt is pending, then take it. Called with IRQs masked:
/// `wfi` wakes on a pending interrupt even while it is masked, so one that arrives
/// between the caller's last look and the `wfi` is not missed.
pub fn enable_interrupts_and_wait() {
    // SAFETY: as above; the interrupt is taken right after the unmask.
    unsafe { asm!("wfi", "msr daifclr, #2", "isb", options(nomem, nostack)) };
}

pub fn halt_forever() -> ! {
    loop {
        disable_interrupts();
        hlt();
    }
}

/// QEMU semihosting is enabled by the harness (`exit=semihosting` on the command
/// line, with QEMU's `-semihosting`). Elsewhere the `hlt` it uses would be an
/// undefined instruction, so it is never issued unless asked for.
static SEMIHOSTING: AtomicBool = AtomicBool::new(false);

/// End a QEMU run with the same status x86-64's debug-exit device gives
/// (`(code << 1) | 1`), through semihosting `SYS_EXIT_EXTENDED`. A no-op unless
/// semihosting was asked for.
pub fn qemu_exit(code: u32) {
    if !SEMIHOSTING.load(Ordering::Relaxed) {
        return;
    }
    // ADP_Stopped_ApplicationExit and the status.
    let block: [u64; 2] = [0x2_0026, u64::from((code << 1) | 1)];
    // SAFETY: the semihosting call reads the two-word block and ends the VM.
    unsafe {
        asm!(
            "hlt #0xf000",
            in("x0") 0x20u64,
            in("x1") block.as_ptr(),
            options(nostack)
        )
    };
}

/// AArch64 has no per-CPU stack register to update: the kernel stack of a thread
/// in EL0 is `SP_EL1` as `eret` left it -- the top, every frame having been popped.
/// Only the first entry into EL0 needs to be told (`context::enter_user`).
pub fn set_kernel_stack(top: u64) {
    percpu::set_kernel_stack(top);
}

/// Order memory accesses against devices and their DMA: everything before is seen
/// by the device before anything after. A device sits outside the inner-shareable
/// domain an ordinary fence covers, so this is a full-system barrier.
pub fn dma_mb() {
    // SAFETY: a barrier.
    unsafe { asm!("dsb sy", options(nostack, preserves_flags)) };
}

/// Device-written memory read after this is at least as new as the flag read
/// before it.
pub fn dma_rmb() {
    // SAFETY: a barrier.
    unsafe { asm!("dmb oshld", options(nostack, preserves_flags)) };
}

/// Memory written before this reaches the device before what is written after.
pub fn dma_wmb() {
    // SAFETY: a barrier.
    unsafe { asm!("dmb oshst", options(nostack, preserves_flags)) };
}

/// This function's caller's frame pointer, for the panic backtrace.
#[inline(always)]
pub fn frame_pointer() -> u64 {
    let fp: u64;
    // SAFETY: reading a register.
    unsafe { asm!("mov {}, x29", out(reg) fp, options(nomem, nostack)) };
    fp
}

/// Switch to the stack whose top is `top` and call `f` there, never to return.
pub fn run_on_stack(top: u64, f: extern "C" fn() -> !) -> ! {
    // SAFETY: `top` is the 16-byte aligned top of a mapped kernel stack.
    unsafe {
        asm!(
            "mov sp, x0",
            "mov x29, xzr",
            "mov x30, xzr",
            "blr x1",
            "brk #1",
            in("x0") top,
            in("x1") f,
            options(noreturn)
        )
    }
}

/// The stack pointer right now.
#[inline(always)]
pub fn stack_pointer() -> u64 {
    let sp: u64;
    // SAFETY: reading a register.
    unsafe { asm!("mov {}, sp", out(reg) sp, options(nomem, nostack)) };
    sp
}
