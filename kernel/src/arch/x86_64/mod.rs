pub(crate) use crate::acpi;
pub mod apic;
pub mod clock;
pub mod context;
pub mod cpu;
pub mod gdt;
pub mod idt;
pub mod mmu;
pub mod pci;
pub mod percpu;
pub mod pic;
pub mod pit;
pub mod power;
pub mod ps2;
pub mod rtc;
pub mod serial;
pub mod smp;
pub mod syscall;
pub mod trap;

use spaceabi::boot::BootInfo;
use x86_64::instructions::port::Port;

/// Entry point. `spaceboot` jumps here with `rdi = &BootInfo`, interrupts disabled, on
/// the boot stack. Re-align the stack for the SysV ABI and never return.
#[unsafe(naked)]
#[unsafe(no_mangle)]
pub extern "C" fn _start() -> ! {
    core::arch::naked_asm!(
        "and rsp, -16",
        "xor rbp, rbp",
        "call {kmain}",
        "ud2",
        kmain = sym crate::kmain,
    )
}

/// This CPU's tables and modes, before memory management: GDT and TSS, IDT, and
/// the FPU switched off.
pub fn init_cpu() {
    gdt::init();
    idt::init();
    cpu::init();
}

/// What needs the kernel's memory management but comes before devices. Nothing on
/// x86-64: PCI configuration space is reached by port.
pub fn init_after_mm(_bi: &BootInfo) {}

/// Interrupt controller, console input, the tick and system calls; after devices,
/// before the scheduler.
pub fn init_platform(_bi: &BootInfo, tick_hz: u32) {
    // Before the mask comes off IRQ 1: a byte the firmware left in the 8042 holds
    // the line high, and an edge-triggered PIC never delivers an interrupt for a
    // line that was already high.
    ps2::init();
    pic::init();
    println!(
        "[kernel] console input: keyboard (IRQ1){}",
        if serial::init_input() { " and COM2 serial (IRQ3)" } else { "; no COM2 UART" }
    );
    pit::init(tick_hz);
    syscall::init();
}

pub fn enable_interrupts() {
    x86_64::instructions::interrupts::enable();
}

pub fn disable_interrupts() {
    x86_64::instructions::interrupts::disable();
}

pub fn interrupts_enabled() -> bool {
    x86_64::instructions::interrupts::are_enabled()
}

pub fn hlt() {
    x86_64::instructions::hlt();
}

/// Enable interrupts and sleep until one arrives, with no window between the two
/// in which a wake-up could be missed (`sti; hlt`).
pub fn enable_interrupts_and_wait() {
    x86_64::instructions::interrupts::enable_and_hlt();
}

/// The stack this CPU enters the kernel on from user mode: the TSS for interrupts
/// and exceptions, the per-CPU block for `syscall`.
pub fn set_kernel_stack(top: u64) {
    gdt::set_kernel_stack(top);
    syscall::set_kernel_stack(top);
}

/// Order memory accesses against devices and their DMA: everything before is seen
/// by the device before anything after. x86-64 keeps stores in order, so these are
/// the ordinary fences.
pub fn dma_mb() {
    core::sync::atomic::fence(core::sync::atomic::Ordering::SeqCst);
}

/// Device-written memory read after this is at least as new as the flag read
/// before it.
pub fn dma_rmb() {
    core::sync::atomic::fence(core::sync::atomic::Ordering::Acquire);
}

/// Memory written before this reaches the device before what is written after.
pub fn dma_wmb() {
    core::sync::atomic::fence(core::sync::atomic::Ordering::Release);
}

/// This function's caller's frame pointer, for the panic backtrace.
#[inline(always)]
pub fn frame_pointer() -> u64 {
    let rbp: u64;
    // SAFETY: reading a register.
    unsafe { core::arch::asm!("mov {}, rbp", out(reg) rbp, options(nomem, nostack)) };
    rbp
}

/// Switch to the stack whose top is `top` and call `f` there, never to return.
pub fn run_on_stack(top: u64, f: extern "C" fn() -> !) -> ! {
    // SAFETY: `top` is the top of a mapped kernel stack; operands are pinned to
    // explicit registers so nothing in the sequence clobbers them.
    unsafe {
        core::arch::asm!(
            "mov rsp, rax",
            "xor ebp, ebp",
            "call rcx",
            "ud2",
            in("rax") top,
            in("rcx") f,
            options(noreturn)
        )
    }
}

/// The stack pointer right now.
#[inline(always)]
pub fn stack_pointer() -> u64 {
    let rsp: u64;
    // SAFETY: reading a register.
    unsafe { core::arch::asm!("mov {}, rsp", out(reg) rsp, options(nomem, nostack)) };
    rsp
}

pub fn halt_forever() -> ! {
    loop {
        disable_interrupts();
        hlt();
    }
}

/// Ask QEMU's `isa-debug-exit` device (iobase 0xf4) to terminate the VM.
/// QEMU exits with status `(code << 1) | 1`. A no-op on real hardware.
pub fn qemu_exit(code: u32) {
    // SAFETY: port 0xf4 is the debug-exit device in the pinned QEMU profile; writing
    // to an unclaimed port on other machines has no effect.
    unsafe { Port::<u32>::new(0xf4).write(code) };
}

pub fn read_cr2() -> u64 {
    x86_64::registers::control::Cr2::read_raw()
}
