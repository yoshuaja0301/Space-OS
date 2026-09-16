//! Console input: the bytes a person types, on their way to user space.
//!
//! Two sources feed one ring buffer, because a terminal should work the same way
//! whether someone is sitting at the machine or driving it over a serial line:
//!
//! * the PS/2 keyboard (IRQ 1), decoded from scan code set 1;
//! * the second UART, COM2 (IRQ 3), which is what the test harness types into.
//!
//! The kernel does no line editing and no echo. It delivers bytes; the session
//! service decides what a line is and what to show. That keeps the kernel out of
//! the business of terminal semantics, which is user space's to define.

use crate::sync::SpinLock;

/// Bytes buffered before the oldest are dropped. A person cannot outrun this, and a
/// harness that floods it would be sending faster than any terminal is meant to.
const CAPACITY: usize = 256;

struct Ring {
    buf: [u8; CAPACITY],
    head: usize,
    len: usize,
    /// Bytes dropped because nobody read fast enough. Reported once, so a full
    /// buffer is visible rather than silent.
    dropped: u32,
    reported: bool,
}

static RING: SpinLock<Ring> =
    SpinLock::new(Ring { buf: [0; CAPACITY], head: 0, len: 0, dropped: 0, reported: false });

/// Queue one byte. Called from interrupt context.
pub fn push(byte: u8) {
    let mut r = RING.lock();
    if r.len == CAPACITY {
        // Drop the oldest: a terminal that loses the start of a stale line is better
        // than one that ignores what is being typed now.
        r.head = (r.head + 1) % CAPACITY;
        r.len -= 1;
        r.dropped = r.dropped.saturating_add(1);
    }
    let tail = (r.head + r.len) % CAPACITY;
    r.buf[tail] = byte;
    r.len += 1;
}

/// Take up to `out.len()` buffered bytes. Returns how many were written; 0 means
/// nothing has been typed.
pub fn read(out: &mut [u8]) -> usize {
    let mut r = RING.lock();
    let n = r.len.min(out.len());
    for slot in out.iter_mut().take(n) {
        let head = r.head;
        *slot = r.buf[head];
        r.head = (head + 1) % CAPACITY;
        r.len -= 1;
    }
    if r.dropped > 0 && !r.reported {
        r.reported = true;
        let dropped = r.dropped;
        drop(r);
        println!("[kernel] console input: {dropped} byte(s) dropped, the buffer was full");
    }
    n
}

/// Translate one scan code (set 1) into a byte, tracking the shift keys.
///
/// Only the keys a command line needs are mapped. An unmapped key produces nothing
/// rather than a guess.
///
/// Extended keys (arrows, Insert, the keypad) arrive as `0xE0` followed by a second
/// byte. The second byte is deliberately dropped except for the two keypad keys that
/// mean something here, because several of those sequences carry a *fake* shift
/// (`0xE0 0x2A` / `0xE0 0x36`): decoding it as a real shift press would leave the
/// keyboard stuck in upper case after an arrow key.
pub fn scancode(code: u8) -> Option<u8> {
    const UNSHIFTED: [u8; 58] = *b"\0\x1b1234567890-=\x08\tqwertyuiop[]\n\0asdfghjkl;'`\0\\zxcvbnm,./\0*\0 ";
    const SHIFTED: [u8; 58] = *b"\0\x1b!@#$%^&*()_+\x08\tQWERTYUIOP{}\n\0ASDFGHJKL:\"~\0|ZXCVBNM<>?\0*\0 ";
    /// Keypad Enter and keypad `/`: the only extended keys worth a character.
    const EXTENDED_ENTER: u8 = 0x1C;
    const EXTENDED_SLASH: u8 = 0x35;
    static SHIFT: SpinLock<bool> = SpinLock::new(false);
    static EXTENDED: SpinLock<bool> = SpinLock::new(false);

    if code == 0xE0 {
        *EXTENDED.lock() = true;
        return None;
    }
    let was_extended = core::mem::replace(&mut *EXTENDED.lock(), false);
    // Bit 7 set means the key was released.
    let released = code & 0x80 != 0;
    let key = code & 0x7F;
    if was_extended {
        if released {
            return None;
        }
        return match key {
            EXTENDED_ENTER => Some(b'\n'),
            EXTENDED_SLASH => Some(b'/'),
            _ => None,
        };
    }
    if key == 0x2A || key == 0x36 {
        *SHIFT.lock() = !released;
        return None;
    }
    if released {
        return None;
    }
    let shifted = *SHIFT.lock();
    let table = if shifted { &SHIFTED } else { &UNSHIFTED };
    match table.get(key as usize).copied() {
        Some(0) | None => None,
        Some(b) => Some(b),
    }
}
