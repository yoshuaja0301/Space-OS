//! Exception and IRQ dispatch.
//!
//! * A fault raised in ring 3 kills the offending process (K02: "akses memori
//!   terlarang mematikan proses uji, bukan kernel").
//! * A fault raised in ring 0 is a kernel bug: dump the frame and panic.
//! * IRQ 0 (PIT) drives the scheduler tick; other IRQs are acknowledged and ignored.

use spaceabi::syscall::{ExitStatus, kill_reason};

use super::pic;
use crate::sched;

/// Register file as saved by `isr_common` (lowest address first).
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct TrapFrame {
    pub r15: u64,
    pub r14: u64,
    pub r13: u64,
    pub r12: u64,
    pub r11: u64,
    pub r10: u64,
    pub r9: u64,
    pub r8: u64,
    pub rbp: u64,
    pub rdi: u64,
    pub rsi: u64,
    pub rdx: u64,
    pub rcx: u64,
    pub rbx: u64,
    pub rax: u64,
    pub vector: u64,
    pub error_code: u64,
    pub rip: u64,
    pub cs: u64,
    pub rflags: u64,
    pub rsp: u64,
    pub ss: u64,
}

impl TrapFrame {
    pub fn is_user(&self) -> bool {
        self.cs & 3 == 3
    }
}

static NMI_COUNT: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

pub const IRQ_BASE: u64 = 32;
pub const IRQ_TIMER: u64 = IRQ_BASE;
pub const IRQ_KEYBOARD: u64 = IRQ_BASE + 1;

fn exception_name(v: u64) -> &'static str {
    match v {
        0 => "divide error",
        1 => "debug",
        2 => "non-maskable interrupt",
        3 => "breakpoint",
        4 => "overflow",
        5 => "bound range exceeded",
        6 => "invalid opcode",
        7 => "device not available",
        8 => "double fault",
        10 => "invalid TSS",
        11 => "segment not present",
        12 => "stack segment fault",
        13 => "general protection fault",
        14 => "page fault",
        16 => "x87 floating point",
        17 => "alignment check",
        18 => "machine check",
        19 => "SIMD floating point",
        20 => "virtualization",
        21 => "control protection",
        _ => "reserved exception",
    }
}

fn kill_reason_for(v: u64) -> u32 {
    match v {
        0 => kill_reason::DIVIDE_ERROR,
        1 => kill_reason::DEBUG,
        3 => kill_reason::BREAKPOINT,
        6 => kill_reason::INVALID_OPCODE,
        13 => kill_reason::GENERAL_PROTECTION,
        14 => kill_reason::PAGE_FAULT,
        _ => kill_reason::OTHER_EXCEPTION,
    }
}

pub fn dump_frame(f: &TrapFrame, cr2: u64) {
    // SAFETY: called from the panic path (interrupts off) or from a user-fault
    // report where the console lock is not held by this thread.
    unsafe {
        crate::console::emergency_print(format_args!(
            "  vector={} ({}) error={:#x} cr2={:#x}\n  rip={:#018x} cs={:#x} rflags={:#x} rsp={:#018x} ss={:#x}\n  rax={:#018x} rbx={:#018x} rcx={:#018x} rdx={:#018x}\n  rsi={:#018x} rdi={:#018x} rbp={:#018x} r8 ={:#018x}\n  r9 ={:#018x} r10={:#018x} r11={:#018x} r12={:#018x}\n  r13={:#018x} r14={:#018x} r15={:#018x}\n",
            f.vector,
            exception_name(f.vector),
            f.error_code,
            cr2,
            f.rip,
            f.cs,
            f.rflags,
            f.rsp,
            f.ss,
            f.rax,
            f.rbx,
            f.rcx,
            f.rdx,
            f.rsi,
            f.rdi,
            f.rbp,
            f.r8,
            f.r9,
            f.r10,
            f.r11,
            f.r12,
            f.r13,
            f.r14,
            f.r15
        ));
    }
}

#[unsafe(no_mangle)]
extern "C" fn x86_64_trap_handler(frame: &mut TrapFrame) {
    match frame.vector {
        0..=31 => handle_exception(frame),
        IRQ_BASE..=47 => handle_irq(frame),
        v => println!("[kernel] unexpected vector {v}, ignored"),
    }
    if frame.is_user() {
        crate::proc::check_pending_kill();
    }
}

const RFLAGS_TF: u64 = 1 << 8;

fn handle_exception(frame: &mut TrapFrame) {
    match frame.vector {
        // NMI is a platform event (SERR, watchdog, ...), never the fault of the code
        // it interrupted. It runs on its own IST stack; log it and resume.
        2 => {
            let n = NMI_COUNT.fetch_add(1, core::sync::atomic::Ordering::Relaxed) + 1;
            if n <= 8 {
                println!("[kernel] NMI #{n} at rip={:#x} (cs={:#x}); ignored", frame.rip, frame.cs);
            }
            return;
        }
        // #DB in kernel mode: user code set TF and executed `syscall`; the single-step
        // trap lands on the first kernel instruction (on the debug IST stack, so the
        // user stack is never touched). Resume; `sysret` restores the user's rflags
        // and the trap fires again in ring 3, where it terminates the process.
        1 if !frame.is_user() => {
            frame.rflags &= !RFLAGS_TF;
            return;
        }
        _ => {}
    }
    let cr2 = if frame.vector == 14 { super::read_cr2() } else { 0 };
    if frame.is_user() {
        let reason = kill_reason_for(frame.vector);
        let fault_addr = if frame.vector == 14 { cr2 } else { frame.rip };
        {
            // Scoped so the name String is freed before exit_current (which never returns).
            let (pid, name) = crate::proc::current_identity();
            println!(
                "[kernel] pid {pid} '{name}' killed: {} at rip={:#x} (error={:#x}, addr={:#x})",
                exception_name(frame.vector),
                frame.rip,
                frame.error_code,
                fault_addr
            );
        }
        crate::proc::exit_current(ExitStatus::killed(reason, fault_addr));
    }
    // Kernel-mode fault: this is always a bug (or deliberate fault injection).
    super::disable_interrupts();
    // SAFETY: we are about to panic; nobody will resume the interrupted code.
    unsafe {
        crate::console::emergency_print(format_args!(
            "\n!!! CPU EXCEPTION IN KERNEL MODE: {} !!!\n",
            exception_name(frame.vector)
        ));
    }
    dump_frame(frame, cr2);
    panic!("unhandled kernel-mode {} (vector {})", exception_name(frame.vector), frame.vector);
}

fn handle_irq(frame: &mut TrapFrame) {
    let irq = (frame.vector - IRQ_BASE) as u8;
    match frame.vector {
        IRQ_TIMER => {
            pic::eoi(irq);
            sched::timer_tick();
        }
        IRQ_KEYBOARD => {
            // Drain the controller so it keeps raising interrupts; input handling is
            // a Developer Preview item (Space Shell).
            // SAFETY: reading the PS/2 data port.
            let _scancode = unsafe { x86_64::instructions::port::Port::<u8>::new(0x60).read() };
            pic::eoi(irq);
        }
        _ => {
            if !pic::is_spurious(irq) {
                pic::eoi(irq);
            }
        }
    }
}
