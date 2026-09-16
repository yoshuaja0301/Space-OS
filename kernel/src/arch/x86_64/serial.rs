//! 16550 UARTs: COM1 (0x3F8) carries the boot and crash log out; COM2 (0x2F8)
//! carries console input in.
//!
//! They are separate ports on purpose. The log is written from anywhere, including
//! a panic handler, and must never be held up by input; input arrives on an
//! interrupt and must never be interleaved into the log stream.

use x86_64::instructions::port::Port;

const COM1: u16 = 0x3F8;
const COM2: u16 = 0x2F8;

/// Set when the UART answered the scratch-register probe. Without a UART every
/// write would otherwise spin on a status register that never changes.
static PRESENT: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

pub fn init() {
    // Probe the scratch register (16550): if it does not hold what we wrote there
    // is no UART at COM1 and the console stays framebuffer-only.
    let probe_ok = probe(COM1);
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

/// Probe a UART by writing to its scratch register and reading it back.
fn probe(base: u16) -> bool {
    // SAFETY: standard 16550 scratch register.
    unsafe {
        let mut scratch = Port::<u8>::new(base + 7);
        scratch.write(0x5A);
        let a = scratch.read();
        scratch.write(0xA5);
        let b = scratch.read();
        a == 0x5A && b == 0xA5
    }
}

static INPUT_PRESENT: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

/// Bring COM2 up for receiving, with the receive interrupt enabled (IRQ 3).
///
/// A machine without a second UART simply has no serial console input; the PS/2
/// keyboard still works and everything else is unaffected.
pub fn init_input() -> bool {
    if !probe(COM2) {
        return false;
    }
    // SAFETY: standard PC COM2 register programming.
    unsafe {
        Port::<u8>::new(COM2 + 1).write(0x00); // interrupts off while configuring
        Port::<u8>::new(COM2 + 3).write(0x80); // DLAB on
        Port::<u8>::new(COM2).write(0x01); // 115200 baud
        Port::<u8>::new(COM2 + 1).write(0x00);
        Port::<u8>::new(COM2 + 3).write(0x03); // 8N1, DLAB off
        Port::<u8>::new(COM2 + 2).write(0xC7); // FIFO on, clear, 14-byte threshold
        Port::<u8>::new(COM2 + 4).write(0x0B); // DTR, RTS, OUT2 (OUT2 gates the IRQ)
        Port::<u8>::new(COM2 + 1).write(0x01); // receive-data-available interrupt
    }
    INPUT_PRESENT.store(true, core::sync::atomic::Ordering::Relaxed);
    true
}

/// Drain everything COM2 has received into the console input buffer.
///
/// The loop must run until the UART reports empty. The 8259 is edge triggered, so a
/// handler that returns with the receive interrupt still asserted never sees another
/// edge: console input would stop for good, silently. The bound below caps how long
/// one interrupt may spend here — a sender faster than this loop, or a device that
/// is lying, both trip it — and hitting it *pauses* the port rather than retiring it:
/// see [`resume_input`].
pub fn drain_input() {
    if !INPUT_PRESENT.load(core::sync::atomic::Ordering::Relaxed) {
        return;
    }
    let mut overrun = false;
    // SAFETY: COM2 line status and receive registers.
    unsafe {
        let mut lsr = Port::<u8>::new(COM2 + 5);
        let mut rx = Port::<u8>::new(COM2);
        for _ in 0..DRAIN_LIMIT {
            let status = lsr.read();
            // Bit 1 is the overrun flag: the UART itself dropped a byte before we
            // read it. Worth saying once; there is nothing to recover.
            overrun |= status & 0x02 != 0;
            if status & 0x01 == 0 {
                if overrun && !OVERRUN_REPORTED.swap(true, core::sync::atomic::Ordering::Relaxed) {
                    println!("[kernel] console input: COM2 overrun, at least one byte was lost");
                }
                return;
            }
            crate::input::push(rx.read());
        }
        // Still "data ready" after that many reads. Mask the receive interrupt so the
        // line drops and this handler can return: an interrupt that never ends is
        // worse than input that pauses. The port is not written off -- `resume_input`
        // brings it back as soon as user space reads, and re-enabling the interrupt
        // with bytes still waiting is itself the edge that restarts delivery.
        Port::<u8>::new(COM2 + 1).write(0x00);
    }
    if !INPUT_STALLED.swap(true, core::sync::atomic::Ordering::Relaxed) {
        println!("[kernel] console input: COM2 sent more than one interrupt can take; pausing it");
    }
}

/// Re-enable COM2's receive interrupt if [`drain_input`] had to pause it.
///
/// Called when user space reads the console: whatever filled the ring has been taken,
/// so there is somewhere to put what comes next. Writing IER with data still waiting
/// re-asserts the UART's interrupt line, which is a fresh edge, so delivery resumes
/// without anyone polling.
pub fn resume_input() {
    if !INPUT_STALLED.swap(false, core::sync::atomic::Ordering::Relaxed) {
        return;
    }
    // SAFETY: COM2 interrupt enable register.
    unsafe { Port::<u8>::new(COM2 + 1).write(0x01) };
}

/// Reads one interrupt will take from the UART before pausing it. A 16550 FIFO holds
/// 16 bytes; the rest of the headroom is for a backend that refills the FIFO from
/// inside the read, which is what a host chardev handing over a paste does.
const DRAIN_LIMIT: usize = 4096;

/// COM2's receive interrupt is masked because one drain hit [`DRAIN_LIMIT`]. It is a
/// pause, not a verdict: the next console read lifts it.
static INPUT_STALLED: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

static OVERRUN_REPORTED: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

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
