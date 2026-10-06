//! Exception vectors and trap dispatch.
//!
//! * `svc` from EL0 is a system call (`x8` the number, `x0`-`x5` the arguments,
//!   the result back in `x0`).
//! * Any other synchronous exception from EL0 kills the process (K02: "akses memori
//!   terlarang mematikan proses uji, bukan kernel").
//! * A synchronous exception at EL1 is a kernel bug: dump the frame and panic.
//! * IRQs come from the GIC: the timer tick, the console UART.
//!
//! Every vector saves the whole register file into a [`TrapFrame`] on the current
//! kernel stack -- `SP_EL1`, which for an exception from EL0 is the top of the
//! running thread's kernel stack -- and `eret`s from it.

use core::arch::{asm, global_asm};

use spaceabi::syscall::{ExitStatus, kill_reason};

use super::{gic, serial, timer};
use crate::sched;

/// Register file as the vectors save it (lowest address first).
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct TrapFrame {
    pub x: [u64; 31],
    pub sp_el0: u64,
    pub elr: u64,
    pub spsr: u64,
    pub esr: u64,
    pub far: u64,
}

const FRAME_SIZE: usize = core::mem::size_of::<TrapFrame>();
// What the vectors reserve (`sub sp, sp, #288`): 36 registers, and a multiple of
// 16, so the stack stays aligned.
const _: () = assert!(FRAME_SIZE == 288);

impl TrapFrame {
    /// From EL0: SPSR.M[3:0] is EL0t.
    pub fn is_user(&self) -> bool {
        self.spsr & 0xF == 0
    }
}

// Sixteen vectors of 0x80 bytes: {current EL with SP0, current EL with SPx, lower
// EL AArch64, lower EL AArch32} x {synchronous, IRQ, FIQ, SError}. Each saves x0/x1,
// puts its number in x0 and joins the common path.
global_asm!(
    ".section .text.vectors, \"ax\"",
    ".balign 2048",
    ".global exception_vectors",
    "exception_vectors:",
    ".irp n, 0,1,2,3,4,5,6,7,8,9,10,11,12,13,14,15",
    ".balign 0x80",
    "    sub sp, sp, #288",
    "    stp x0, x1, [sp, #0]",
    "    mov x0, #\\n",
    "    b exception_common",
    ".endr",
    "",
    "exception_common:",
    "    stp x2, x3, [sp, #16]",
    "    stp x4, x5, [sp, #32]",
    "    stp x6, x7, [sp, #48]",
    "    stp x8, x9, [sp, #64]",
    "    stp x10, x11, [sp, #80]",
    "    stp x12, x13, [sp, #96]",
    "    stp x14, x15, [sp, #112]",
    "    stp x16, x17, [sp, #128]",
    "    stp x18, x19, [sp, #144]",
    "    stp x20, x21, [sp, #160]",
    "    stp x22, x23, [sp, #176]",
    "    stp x24, x25, [sp, #192]",
    "    stp x26, x27, [sp, #208]",
    "    stp x28, x29, [sp, #224]",
    "    mrs x2, sp_el0",
    "    stp x30, x2, [sp, #240]",
    "    mrs x2, elr_el1",
    "    mrs x3, spsr_el1",
    "    stp x2, x3, [sp, #256]",
    "    mrs x2, esr_el1",
    "    mrs x3, far_el1",
    "    stp x2, x3, [sp, #272]",
    "    mov x1, x0",
    "    mov x0, sp",
    "    bl aarch64_trap",
    "    ldp x2, x3, [sp, #256]",
    "    msr elr_el1, x2",
    "    msr spsr_el1, x3",
    "    ldp x30, x2, [sp, #240]",
    "    msr sp_el0, x2",
    "    ldp x2, x3, [sp, #16]",
    "    ldp x4, x5, [sp, #32]",
    "    ldp x6, x7, [sp, #48]",
    "    ldp x8, x9, [sp, #64]",
    "    ldp x10, x11, [sp, #80]",
    "    ldp x12, x13, [sp, #96]",
    "    ldp x14, x15, [sp, #112]",
    "    ldp x16, x17, [sp, #128]",
    "    ldp x18, x19, [sp, #144]",
    "    ldp x20, x21, [sp, #160]",
    "    ldp x22, x23, [sp, #176]",
    "    ldp x24, x25, [sp, #192]",
    "    ldp x26, x27, [sp, #208]",
    "    ldp x28, x29, [sp, #224]",
    "    ldp x0, x1, [sp, #0]",
    "    add sp, sp, #288",
    "    eret",
);

unsafe extern "C" {
    static exception_vectors: u8;
}

/// The vector table's address, for a CPU that installs it itself (`smp`).
pub fn vectors() -> u64 {
    core::ptr::addr_of!(exception_vectors) as u64
}

pub fn init() {
    // SAFETY: the vector table is 2 KiB aligned code in the kernel image.
    unsafe {
        asm!(
            "msr vbar_el1, {}",
            "isb",
            in(reg) core::ptr::addr_of!(exception_vectors),
            options(nostack)
        )
    };
}

/// Exception classes (ESR_EL1.EC) the kernel tells apart.
mod ec {
    pub const UNKNOWN: u64 = 0x00;
    pub const FP_ACCESS: u64 = 0x07;
    pub const ILLEGAL_STATE: u64 = 0x0E;
    pub const SVC64: u64 = 0x15;
    pub const SYSREG: u64 = 0x18;
    pub const IABT_LOWER: u64 = 0x20;
    pub const IABT_SAME: u64 = 0x21;
    pub const PC_ALIGN: u64 = 0x22;
    pub const DABT_LOWER: u64 = 0x24;
    pub const DABT_SAME: u64 = 0x25;
    pub const SP_ALIGN: u64 = 0x26;
    pub const BREAKPOINT_LOWER: u64 = 0x30;
    pub const STEP_LOWER: u64 = 0x32;
    pub const WATCHPOINT_LOWER: u64 = 0x34;
    pub const BRK: u64 = 0x3C;
}

fn class_name(class: u64) -> &'static str {
    match class {
        ec::UNKNOWN => "undefined instruction",
        ec::FP_ACCESS => "FP/SIMD instruction",
        ec::ILLEGAL_STATE => "illegal execution state",
        ec::SVC64 => "system call",
        ec::SYSREG => "privileged system register access",
        ec::IABT_LOWER | ec::IABT_SAME => "instruction abort (page fault)",
        ec::PC_ALIGN => "PC alignment fault",
        ec::DABT_LOWER | ec::DABT_SAME => "data abort (page fault)",
        ec::SP_ALIGN => "SP alignment fault",
        ec::BREAKPOINT_LOWER | ec::STEP_LOWER | ec::WATCHPOINT_LOWER => "debug exception",
        ec::BRK => "breakpoint (brk)",
        _ => "other exception",
    }
}

fn kill_reason_for(class: u64) -> u32 {
    match class {
        ec::UNKNOWN => kill_reason::INVALID_OPCODE,
        ec::FP_ACCESS => kill_reason::NO_FPU,
        ec::ILLEGAL_STATE | ec::SYSREG | ec::PC_ALIGN | ec::SP_ALIGN => kill_reason::GENERAL_PROTECTION,
        ec::IABT_LOWER | ec::DABT_LOWER => kill_reason::PAGE_FAULT,
        ec::BREAKPOINT_LOWER | ec::STEP_LOWER | ec::WATCHPOINT_LOWER => kill_reason::DEBUG,
        ec::BRK => kill_reason::BREAKPOINT,
        _ => kill_reason::OTHER_EXCEPTION,
    }
}

pub fn dump_frame(f: &TrapFrame) {
    // SAFETY: called from the panic path (interrupts off).
    unsafe {
        crate::console::emergency_print(format_args!(
            "  esr={:#x} ({}) far={:#x}\n  pc={:#018x} spsr={:#x} sp_el0={:#018x}\n",
            f.esr,
            class_name(f.esr >> 26),
            f.far,
            f.elr,
            f.spsr,
            f.sp_el0
        ));
        // No allocation here: this runs on the panic path.
        for (i, x) in f.x.iter().enumerate() {
            let end = if i % 4 == 3 || i == 30 { "\n" } else { "" };
            crate::console::emergency_print(format_args!("  x{i:<2}={x:#018x}{end}"));
        }
    }
}

const VEC_SYNC: u64 = 0;
const VEC_IRQ: u64 = 1;

#[unsafe(no_mangle)]
extern "C" fn aarch64_trap(frame: &mut TrapFrame, vector: u64) {
    let (group, kind) = (vector / 4, vector % 4);
    match (group, kind) {
        // Lower EL, AArch64.
        (2, VEC_SYNC) => user_sync(frame),
        (2, VEC_IRQ) | (1, VEC_IRQ) => handle_irq(),
        // Current EL with SPx: the kernel itself.
        (1, VEC_SYNC) => kernel_fault(frame, "synchronous exception"),
        (_, 2) => kernel_fault(frame, "FIQ"),
        (_, 3) => kernel_fault(frame, "SError"),
        // Current EL with SP0 is never used, and EL0 never runs AArch32.
        _ => kernel_fault(frame, "exception from an unexpected state"),
    }
    if frame.is_user() {
        crate::proc::check_pending_kill();
    }
}

fn user_sync(frame: &mut TrapFrame) {
    let class = frame.esr >> 26;
    if class == ec::SVC64 {
        frame.x[0] = crate::syscall::dispatch(frame) as u64;
        return;
    }
    let reason = kill_reason_for(class);
    let fault_addr = match class {
        ec::IABT_LOWER | ec::DABT_LOWER | ec::PC_ALIGN | ec::WATCHPOINT_LOWER => frame.far,
        _ => frame.elr,
    };
    {
        // Scoped so the name String is freed before exit_current (which never returns).
        let (pid, name) = crate::proc::current_identity();
        println!(
            "[kernel] pid {pid} '{name}' killed: {} at pc={:#x} (esr={:#x}, addr={:#x})",
            class_name(class),
            frame.elr,
            frame.esr,
            fault_addr
        );
    }
    crate::proc::exit_current(ExitStatus::killed(reason, fault_addr));
}

fn kernel_fault(frame: &TrapFrame, what: &str) -> ! {
    super::disable_interrupts();
    let class = frame.esr >> 26;
    // SAFETY: we are about to panic; nobody will resume the interrupted code.
    unsafe {
        crate::console::emergency_print(format_args!(
            "\n!!! CPU EXCEPTION IN KERNEL MODE: {what}: {} !!!\n",
            class_name(class)
        ));
    }
    dump_frame(frame);
    panic!(
        "unhandled kernel-mode {} ({what}, esr {:#x}, far {:#x})",
        class_name(class),
        frame.esr,
        frame.far
    );
}

/// One interrupt per entry: the tick may switch threads, and the next interrupt
/// is taken when this one returns.
fn handle_irq() {
    let Some(intid) = gic::acknowledge() else { return };
    if intid == timer::intid() {
        timer::rearm();
        gic::end(intid);
        // The network device is polled from the boot CPU's tick: a frame that
        // arrived since the last tick wakes its waiters here, before the tick
        // decides who runs next. So are the USB controllers: a key pressed since
        // then is queued here.
        if super::percpu::index() == 0 {
            crate::dev::nic::poll_tick();
            crate::dev::xhci::poll_tick();
        }
        sched::timer_tick();
    } else if Some(intid) == serial::input_intid() {
        serial::drain_input();
        gic::end(intid);
    } else if intid == gic::SGI_RESCHEDULE {
        gic::end(intid);
        sched::reschedule_ipi();
    } else if intid == gic::SGI_HALT {
        gic::end(intid);
        // Another CPU panicked and is stopping the rest: stop here, touching
        // nothing.
        if crate::panic::in_progress() {
            super::halt_forever();
        }
    } else {
        gic::end(intid);
    }
}
