//! 8253/8254 programmable interval timer, channel 0, rate generator.

use x86_64::instructions::port::Port;

const PIT_FREQ: u32 = 1_193_182;

pub fn init(hz: u32) {
    let divisor = (PIT_FREQ / hz).clamp(1, 65535) as u16;
    // SAFETY: standard PIT programming.
    unsafe {
        Port::<u8>::new(0x43).write(0x34); // channel 0, lo/hi byte, mode 2
        Port::<u8>::new(0x40).write(divisor as u8);
        Port::<u8>::new(0x40).write((divisor >> 8) as u8);
    }
    println!("[kernel] pit: {hz} Hz (divisor {divisor})");
}
