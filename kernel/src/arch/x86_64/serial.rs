//! 16550 UART on COM1 (0x3F8): the boot log and crash log channel.

use x86_64::instructions::port::Port;

const COM1: u16 = 0x3F8;

/// Set when the UART answered the scratch-register probe. Without a UART every
/// write would otherwise spin on a status register that never changes.
static PRESENT: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

pub fn init() {
    // Probe the scratch register (16550): if it does not hold what we wrote there
    // is no UART at COM1 and the console stays framebuffer-only.
    // SAFETY: standard COM1 scratch register.
    let probe_ok = unsafe {
        let mut scratch = Port::<u8>::new(COM1 + 7);
        scratch.write(0x5A);
        let a = scratch.read();
        scratch.write(0xA5);
        let b = scratch.read();
        a == 0x5A && b == 0xA5
    };
    PRESENT.store(probe_ok, core::sync::atomic::Ordering::Relaxed);
    if !probe_ok {
        return;
    }
    // SAFETY: standard PC COM1 register programming.
    unsafe {
        Port::<u8>::new(COM1 + 1).write(0x00); // disable interrupts
        Port::<u8>::new(COM1 + 3).write(0x80); // DLAB on
        Port::<u8>::new(COM1).write(0x01); // 115200 baud (divisor 1)
        Port::<u8>::new(COM1 + 1).write(0x00);
        Port::<u8>::new(COM1 + 3).write(0x03); // 8N1, DLAB off
        Port::<u8>::new(COM1 + 2).write(0xC7); // FIFO on, clear, 14-byte threshold
        Port::<u8>::new(COM1 + 4).write(0x0B); // DTR, RTS, OUT2
    }
}

fn write_byte(b: u8) {
    // SAFETY: COM1 line status / transmit registers.
    unsafe {
        let mut lsr = Port::<u8>::new(COM1 + 5);
        let mut spins = 0u32;
        while lsr.read() & 0x20 == 0 {
            spins += 1;
            if spins > 1_000_000 {
                return; // no UART present; do not hang the kernel
            }
        }
        Port::<u8>::new(COM1).write(b);
    }
}

pub fn write_str(s: &str) {
    if !PRESENT.load(core::sync::atomic::Ordering::Relaxed) {
        return;
    }
    for b in s.bytes() {
        if b == b'\n' {
            write_byte(b'\r');
        }
        write_byte(b);
    }
}
