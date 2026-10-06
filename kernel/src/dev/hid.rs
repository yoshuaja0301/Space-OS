//! USB keyboards in the boot protocol (HID 1.11 appendix B), turned into the scan
//! codes the PS/2 keyboard sends.
//!
//! A boot-protocol report is eight bytes: the eight modifier keys as bits, a reserved
//! byte, and up to six keys held down, as usage IDs (HID Usage Tables §10). The
//! keyboard reports what is held, not what changed, so each report is compared with
//! the last one: a key that appeared went down, one that disappeared came up. Every
//! change becomes the scan code set 1 sequence a PS/2 keyboard would have sent for
//! it and goes through [`crate::input::decode`] -- one decoder, one set of modifier
//! rules, the same events and bytes whichever keyboard the person types on.

use crate::input;

/// The report of a keyboard that has more keys down than it can say (usage 1,
/// ErrorRollOver, in every key slot): nothing in it is reliable, so it is ignored
/// and the keys stay as they were.
const ROLLOVER: u8 = 0x01;

/// Scan code set 1 make codes by usage ID, from 0x04 (A) to 0x65 (Application);
/// `0xE0__` is an extended key, 0 a usage with no set 1 code here.
const KEYS: [u16; 0x66] = [
    0, 0, 0, 0, // reserved, ErrorRollOver, POSTFail, ErrorUndefined
    0x1E, 0x30, 0x2E, 0x20, 0x12, 0x21, 0x22, 0x23, 0x17, 0x24, 0x25, 0x26, 0x32, // a-m
    0x31, 0x18, 0x19, 0x10, 0x13, 0x1F, 0x14, 0x16, 0x2F, 0x11, 0x2D, 0x15, 0x2C, // n-z
    0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0A, 0x0B, // 1-9, 0
    0x1C, 0x01, 0x0E, 0x0F, 0x39, // Enter, Escape, Backspace, Tab, Space
    0x0C, 0x0D, 0x1A, 0x1B, 0x2B, 0x2B, 0x27, 0x28, 0x29, 0x33, 0x34, 0x35, // - = [ ] \ # ; ' ` , . /
    0x3A, // Caps Lock
    0x3B, 0x3C, 0x3D, 0x3E, 0x3F, 0x40, 0x41, 0x42, 0x43, 0x44, 0x57, 0x58, // F1-F12
    0xE037, 0x46, 0, // Print Screen, Scroll Lock, Pause (no make/break pair)
    0xE052, 0xE047, 0xE049, 0xE053, 0xE04F, 0xE051, // Insert Home PageUp Delete End PageDown
    0xE04D, 0xE04B, 0xE050, 0xE048, // Right Left Down Up
    0x45, 0xE035, 0x37, 0x4A, 0x4E, 0xE01C, // Num Lock, keypad / * - + Enter
    0x4F, 0x50, 0x51, 0x4B, 0x4C, 0x4D, 0x47, 0x48, 0x49, 0x52, 0x53, // keypad 1-9, 0, .
    0x56, 0xE05D, // the key left of Z on ISO keyboards, Application
];

/// The modifier bits of byte 0, from bit 0: left Ctrl, Shift, Alt, GUI, then right.
const MODIFIERS: [u16; 8] = [0x1D, 0x2A, 0x38, 0xE05B, 0xE01D, 0x36, 0xE038, 0xE05C];

/// Scan code set 1 for usage `usage`, if it has one.
pub fn scan_code(usage: u8) -> Option<u16> {
    match usage {
        0xE0..=0xE7 => Some(MODIFIERS[usize::from(usage - 0xE0)]),
        _ => KEYS.get(usize::from(usage)).copied().filter(|&c| c != 0),
    }
}

/// Send one key's make (`down`) or break code through the keyboard decoder.
fn key(code: u16, down: bool) {
    if code & 0xFF00 == 0xE000 {
        deliver(0xE0);
    }
    let make = (code & 0x7F) as u8;
    deliver(if down { make } else { make | 0x80 });
}

fn deliver(byte: u8) {
    let d = input::decode(byte);
    if let Some(e) = d.event {
        input::push_event(e);
    }
    if let Some(b) = d.ch {
        input::push(b);
    }
}

/// One keyboard's last report.
#[derive(Clone, Copy, Default)]
pub struct Keyboard {
    modifiers: u8,
    keys: [u8; 6],
}

impl Keyboard {
    /// Take a new boot-protocol report: what came up first, then what went down,
    /// so that a key moved under a held modifier types with the modifier.
    pub fn report(&mut self, r: &[u8]) {
        if r.len() < 8 || r[2..8].iter().all(|&k| k == ROLLOVER) {
            return;
        }
        let keys: [u8; 6] = [r[2], r[3], r[4], r[5], r[6], r[7]];
        let held = |set: &[u8; 6], k: u8| k > ROLLOVER && set.contains(&k);
        for &k in &self.keys {
            if k > ROLLOVER
                && !held(&keys, k)
                && let Some(code) = scan_code(k)
            {
                key(code, false);
            }
        }
        let (old, new) = (self.modifiers, r[0]);
        for (bit, &code) in MODIFIERS.iter().enumerate() {
            let mask = 1u8 << bit;
            if old & mask != new & mask {
                key(code, new & mask != 0);
            }
        }
        for &k in &keys {
            if k > ROLLOVER
                && !held(&self.keys, k)
                && let Some(code) = scan_code(k)
            {
                key(code, true);
            }
        }
        self.modifiers = new;
        self.keys = keys;
    }

    /// The keyboard went away: everything it held comes up.
    pub fn release_all(&mut self) {
        self.report(&[0; 8]);
    }
}
