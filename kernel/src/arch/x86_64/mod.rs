pub mod context;
pub mod gdt;
pub mod idt;
pub mod pic;
pub mod pit;
pub mod ps2;
pub mod serial;
pub mod syscall;
pub mod trap;

use x86_64::instructions::port::Port;

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
