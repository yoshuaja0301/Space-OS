//! PS/2 controller (8042): just enough to make the keyboard's interrupt usable.
//!
//! The kernel does not program the controller — the firmware has already put it in
//! translated scan-code-set-1 mode, which is what [`crate::input::scancode`] decodes.
//! What the kernel must do is make sure the controller is *quiet* before IRQ 1 is
//! unmasked, and quiet again before each interrupt returns.

use x86_64::instructions::port::Port;

const DATA: u16 = 0x60;
const STATUS: u16 = 0x64;
/// Bit 0 of the status port: a byte is waiting in the output buffer.
const OUTPUT_FULL: u8 = 0x01;
/// Bit 1 of the status port: the controller has not taken the last byte written to
/// it yet.
const INPUT_FULL: u8 = 0x02;
/// Controller command: place the following byte in the output buffer as though the
/// keyboard had sent it, raising IRQ 1 with it.
const CMD_WRITE_OUTPUT: u8 = 0xD2;
/// Bit 5: the waiting byte came from the auxiliary device (the mouse), not the
/// keyboard. It still has to be read — it is what blocks the keyboard's own byte —
/// but decoding it as a scan code would type characters nobody pressed.
const FROM_AUX: u8 = 0x20;
/// Reads one call will take before deciding the controller is not going quiet. The
/// 8042 output buffer holds a single byte, so anything past a handful means the
/// device is producing faster than this loop can drain — or lying.
const DRAIN_LIMIT: usize = 64;

/// Drain whatever the firmware left behind, before the PIC unmasks IRQ 1.
///
/// This is the same hazard the COM2 path documents, one step earlier: the 8259 is
/// edge triggered, and a byte still sitting in the 8042 output buffer holds IRQ 1
/// asserted. A line that is *already* high when the mask is lifted never produces
/// the edge that delivers the first interrupt, so the keyboard would be deaf for the
/// whole boot with nothing to show for it. OVMF uses the PS/2 keyboard for its own
/// console, so assuming it left nothing behind is an assumption, not a fact.
pub fn init() {
    let left = drain();
    if left > 0 {
        println!("[kernel] ps/2: dropped {left} byte(s) the firmware left in the controller");
    }
}

/// Read bytes until the output buffer is empty. Returns how many were taken.
pub fn drain() -> usize {
    let mut taken = 0;
    // SAFETY: the 8042 status and data ports.
    unsafe {
        let mut status = Port::<u8>::new(STATUS);
        let mut data = Port::<u8>::new(DATA);
        for _ in 0..DRAIN_LIMIT {
            if status.read() & OUTPUT_FULL == 0 {
                return taken;
            }
            let _ = data.read();
            taken += 1;
        }
    }
    taken
}

/// Take every scan code the controller has, decode each one, and queue what it means.
///
/// The loop matters for the same reason as the drain above: returning with a byte
/// still in the output buffer leaves IRQ 1 asserted, and the next key press produces
/// no edge. One read per interrupt is enough only while keys arrive slowly enough to
/// never overlap, which is not a property the kernel gets to assume.
pub fn read_scancodes() {
    // SAFETY: the 8042 status and data ports.
    unsafe {
        let mut status = Port::<u8>::new(STATUS);
        let mut data = Port::<u8>::new(DATA);
        for _ in 0..DRAIN_LIMIT {
            let st = status.read();
            if st & OUTPUT_FULL == 0 {
                return;
            }
            let byte = data.read();
            if st & FROM_AUX != 0 {
                continue;
            }
            if let Some(b) = crate::input::scancode(byte) {
                crate::input::push(b);
            }
        }
    }
}

/// Hand the CPU one scan code as though a key had been pressed.
///
/// This is the only way software can exercise the path a real key press takes --
/// controller, IRQ 1, the drain above, the decoder, the input ring -- and without it
/// nothing in an automated run proves that path works at all on a machine nobody is
/// sitting at. Returns false if the controller never accepted the command.
pub fn inject(scancode: u8) -> bool {
    // SAFETY: the 8042 command and data ports.
    unsafe {
        let mut status = Port::<u8>::new(STATUS);
        let mut port = Port::<u8>::new(STATUS);
        let mut data = Port::<u8>::new(DATA);
        if !wait_writable(&mut status) {
            return false;
        }
        port.write(CMD_WRITE_OUTPUT);
        if !wait_writable(&mut status) {
            return false;
        }
        data.write(scancode);
    }
    true
}

/// Spin until the controller has taken what was last written to it. Bounded: a
/// controller that never drains its input buffer must not hang the kernel.
fn wait_writable(status: &mut Port<u8>) -> bool {
    for _ in 0..100_000 {
        // SAFETY: reading the 8042 status port has no side effects.
        if unsafe { status.read() } & INPUT_FULL == 0 {
            return true;
        }
    }
    false
}
