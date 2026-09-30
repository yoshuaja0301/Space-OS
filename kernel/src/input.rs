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

use spaceabi::input::{InputEvent, flags, key, kind};

use crate::sync::SpinLock;

/// Bytes buffered before the oldest are dropped. A person cannot outrun this, and a
/// harness that floods it would be sending faster than any terminal is meant to.
const CAPACITY: usize = 256;

struct Ring {
    buf: [u8; CAPACITY],
    head: usize,
    len: usize,
    /// Bytes dropped because nobody read fast enough, since the last time a reader
    /// was told. Counted per episode rather than latched once: a second overflow is
    /// exactly as damaging to the line being typed as the first one, so it has to be
    /// exactly as visible.
    dropped: u32,
}

static RING: SpinLock<Ring> = SpinLock::new(Ring { buf: [0; CAPACITY], head: 0, len: 0, dropped: 0 });

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
    n
}

/// Take the number of bytes dropped since the last call, and forget them.
///
/// The reader asks before it takes any byte, so that "the stream has a hole in it"
/// arrives *before* the bytes on the far side of the hole: a session can then throw
/// away the half-line it had assembled instead of running a command nobody typed.
pub fn take_dropped() -> u32 {
    core::mem::take(&mut RING.lock().dropped)
}

/// What one scan code meant: the key event, if it completed one, and the byte it
/// types for a terminal, if any.
pub struct Decoded {
    pub event: Option<InputEvent>,
    pub ch: Option<u8>,
}

/// The decoder's memory between scan codes: the `0xE0` prefix, and which
/// modifiers are down. Each modifier key is tracked on its own side: releasing one
/// while the other is still held must not drop the modifier.
struct KeyState {
    extended: bool,
    shift_left: bool,
    shift_right: bool,
    ctrl_left: bool,
    ctrl_right: bool,
    alt_left: bool,
    alt_right: bool,
    super_left: bool,
    super_right: bool,
}

static KEYS: SpinLock<KeyState> = SpinLock::new(KeyState {
    extended: false,
    shift_left: false,
    shift_right: false,
    ctrl_left: false,
    ctrl_right: false,
    alt_left: false,
    alt_right: false,
    super_left: false,
    super_right: false,
});

impl KeyState {
    fn mods(&self) -> u8 {
        let mut m = 0;
        if self.shift_left || self.shift_right {
            m |= flags::SHIFT;
        }
        if self.ctrl_left || self.ctrl_right {
            m |= flags::CTRL;
        }
        if self.alt_left || self.alt_right {
            m |= flags::ALT;
        }
        if self.super_left || self.super_right {
            m |= flags::SUPER;
        }
        m
    }
}

/// Decode one scan code (set 1).
///
/// Every key that goes down or up becomes an event for a desktop, with the
/// modifiers held. A press also types a byte for a terminal when the key has one
/// and no Ctrl, Alt or Super is held: those make shortcuts, not text.
///
/// Extended keys (arrows, the right-hand modifiers, Super, the keypad) arrive as
/// `0xE0` then a second byte. Several of those sequences carry a *fake* shift
/// (`0xE0 0x2A` / `0xE0 0x36`); counting it as a real shift would leave the keyboard
/// stuck in upper case after an arrow key, so it is dropped.
pub fn decode(code: u8) -> Decoded {
    const UNSHIFTED: [u8; 58] = *b"\0\x1b1234567890-=\x08\tqwertyuiop[]\n\0asdfghjkl;'`\0\\zxcvbnm,./\0*\0 ";
    const SHIFTED: [u8; 58] = *b"\0\x1b!@#$%^&*()_+\x08\tQWERTYUIOP{}\n\0ASDFGHJKL:\"~\0|ZXCVBNM<>?\0*\0 ";
    let nothing = Decoded { event: None, ch: None };
    let mut k = KEYS.lock();
    if code == 0xE0 {
        k.extended = true;
        return nothing;
    }
    let extended = core::mem::replace(&mut k.extended, false);
    // Bit 7 set means the key was released.
    let pressed = code & 0x80 == 0;
    let make = code & 0x7F;
    if extended && (make == 0x2A || make == 0x36) {
        return nothing; // a fake shift
    }
    let keycode = if extended { key::EXT | make as u16 } else { make as u16 };
    match keycode {
        key::LSHIFT => k.shift_left = pressed,
        key::RSHIFT => k.shift_right = pressed,
        key::LCTRL => k.ctrl_left = pressed,
        key::RCTRL => k.ctrl_right = pressed,
        key::LALT => k.alt_left = pressed,
        key::RALT => k.alt_right = pressed,
        key::LSUPER => k.super_left = pressed,
        key::RSUPER => k.super_right = pressed,
        _ => {}
    }
    let mods = k.mods();
    drop(k);
    let ch = if !pressed || mods & (flags::CTRL | flags::ALT | flags::SUPER) != 0 {
        None
    } else if extended {
        match keycode {
            key::KP_ENTER => Some(b'\n'),
            key::KP_SLASH => Some(b'/'),
            _ => None,
        }
    } else {
        let table = if mods & flags::SHIFT != 0 { &SHIFTED } else { &UNSHIFTED };
        match table.get(make as usize).copied() {
            Some(0) | None => None,
            Some(b) => Some(b),
        }
    };
    let event = InputEvent {
        kind: kind::KEY,
        flags: mods | if pressed { flags::PRESSED } else { 0 },
        key: keycode,
        ch: ch.map_or(0, u32::from),
        time_ms: crate::sched::uptime_ms(),
    };
    Decoded { event: Some(event), ch }
}

/// Translate one scan code into the byte it types, if any. The terminal's half of
/// [`decode`]; the kernel selftest drives it with real sequences.
pub fn scancode(code: u8) -> Option<u8> {
    decode(code).ch
}

/// Events buffered for a desktop before the oldest are dropped.
const EVENTS: usize = 128;

struct EventRing {
    buf: [InputEvent; EVENTS],
    head: usize,
    len: usize,
    dropped: u32,
}

const NO_EVENT: InputEvent = InputEvent { kind: 0, flags: 0, key: 0, ch: 0, time_ms: 0 };

static EVENT_RING: SpinLock<EventRing> =
    SpinLock::new(EventRing { buf: [NO_EVENT; EVENTS], head: 0, len: 0, dropped: 0 });

/// Queue one key event. Called from interrupt context.
pub fn push_event(e: InputEvent) {
    let mut r = EVENT_RING.lock();
    if r.len == EVENTS {
        r.head = (r.head + 1) % EVENTS;
        r.len -= 1;
        r.dropped = r.dropped.saturating_add(1);
    }
    let tail = (r.head + r.len) % EVENTS;
    r.buf[tail] = e;
    r.len += 1;
}

/// Take up to `out.len()` events, oldest first. When events were dropped since the
/// last call, the first one returned says how many ([`kind::LOST`]).
pub fn read_events(out: &mut [InputEvent]) -> usize {
    let mut r = EVENT_RING.lock();
    let mut n = 0;
    if r.dropped > 0 && !out.is_empty() {
        out[0] = InputEvent {
            kind: kind::LOST,
            flags: 0,
            key: 0,
            ch: core::mem::take(&mut r.dropped),
            time_ms: crate::sched::uptime_ms(),
        };
        n = 1;
    }
    while n < out.len() && r.len > 0 {
        let head = r.head;
        out[n] = r.buf[head];
        r.head = (head + 1) % EVENTS;
        r.len -= 1;
        n += 1;
    }
    n
}
