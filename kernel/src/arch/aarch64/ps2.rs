//! No PS/2 controller on an AArch64 machine: the keyboard injection that the
//! acceptance suite uses on x86-64 is not available.

pub fn inject(_scancode: u8) -> bool {
    false
}
