//! Keyboard events, as `SYS_INPUT_READ` delivers them.
//!
//! `SYS_CONSOLE_READ` gives a terminal the characters typed. A desktop needs more:
//! which key, pressed or released, with which modifiers held -- Alt+Tab types
//! nothing, and is still the most important thing pressed all day.

/// One key going down or up.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct InputEvent {
    /// One of [`kind`].
    pub kind: u8,
    /// [`flags`]: pressed or released, and the modifiers held once this key is
    /// counted.
    pub flags: u8,
    /// Which key ([`key`]); 0 for [`kind::LOST`].
    pub key: u16,
    /// The character a press types (shift applied), or 0: releases, keys that type
    /// nothing, and anything pressed with Ctrl, Alt or Super held type nothing. For
    /// [`kind::LOST`], how many events were dropped.
    pub ch: u32,
    /// Milliseconds since boot.
    pub time_ms: u64,
}

impl InputEvent {
    pub const fn pressed(&self) -> bool {
        self.flags & flags::PRESSED != 0
    }

    /// The modifier bits alone.
    pub const fn mods(&self) -> u8 {
        self.flags & (flags::SHIFT | flags::CTRL | flags::ALT | flags::SUPER)
    }

    /// A press of `key` with exactly the modifiers `mods`.
    pub const fn is(&self, key: u16, mods: u8) -> bool {
        self.kind == kind::KEY && self.pressed() && self.key == key && self.mods() == mods
    }

    pub fn as_bytes(&self) -> &[u8] {
        // SAFETY: `repr(C)` plain data without padding (1 + 1 + 2 + 4 + 8 bytes).
        unsafe { core::slice::from_raw_parts(self as *const Self as *const u8, core::mem::size_of::<Self>()) }
    }

    pub fn from_bytes(b: &[u8]) -> Option<InputEvent> {
        if b.len() != core::mem::size_of::<InputEvent>() {
            return None;
        }
        // SAFETY: exactly one `InputEvent` worth of bytes; every bit pattern is valid.
        Some(unsafe { core::ptr::read_unaligned(b.as_ptr() as *const InputEvent) })
    }
}

pub mod kind {
    pub const KEY: u8 = 1;
    /// Events were dropped because nobody read them in time; `ch` says how many. It
    /// comes before the events that survived, so a reader learns of the hole first.
    pub const LOST: u8 = 2;
}

pub mod flags {
    pub const PRESSED: u8 = 1;
    pub const SHIFT: u8 = 2;
    pub const CTRL: u8 = 4;
    pub const ALT: u8 = 8;
    pub const SUPER: u8 = 16;
}

/// Key codes: the scan code set 1 make code, and `EXT | second byte` for the keys
/// that arrive with an `0xE0` prefix. Only the keys a desktop uses are named.
pub mod key {
    pub const EXT: u16 = 0x100;
    pub const ESC: u16 = 0x01;
    pub const N1: u16 = 0x02;
    pub const N2: u16 = 0x03;
    pub const N3: u16 = 0x04;
    pub const N4: u16 = 0x05;
    pub const MINUS: u16 = 0x0C;
    pub const EQUAL: u16 = 0x0D;
    pub const BACKSPACE: u16 = 0x0E;
    pub const TAB: u16 = 0x0F;
    pub const Q: u16 = 0x10;
    pub const W: u16 = 0x11;
    pub const E: u16 = 0x12;
    pub const R: u16 = 0x13;
    pub const T: u16 = 0x14;
    pub const ENTER: u16 = 0x1C;
    pub const LCTRL: u16 = 0x1D;
    pub const A: u16 = 0x1E;
    pub const S: u16 = 0x1F;
    pub const D: u16 = 0x20;
    pub const F: u16 = 0x21;
    pub const H: u16 = 0x23;
    pub const LSHIFT: u16 = 0x2A;
    pub const Z: u16 = 0x2C;
    pub const C: u16 = 0x2E;
    pub const M: u16 = 0x32;
    pub const RSHIFT: u16 = 0x36;
    pub const LALT: u16 = 0x38;
    pub const SPACE: u16 = 0x39;
    pub const F1: u16 = 0x3B;
    pub const F2: u16 = 0x3C;
    pub const F3: u16 = 0x3D;
    pub const F4: u16 = 0x3E;
    pub const F5: u16 = 0x3F;
    pub const F9: u16 = 0x43;
    pub const F10: u16 = 0x44;
    pub const F11: u16 = 0x57;
    pub const F12: u16 = 0x58;
    pub const KP_ENTER: u16 = EXT | 0x1C;
    pub const RCTRL: u16 = EXT | 0x1D;
    pub const KP_SLASH: u16 = EXT | 0x35;
    pub const RALT: u16 = EXT | 0x38;
    pub const HOME: u16 = EXT | 0x47;
    pub const UP: u16 = EXT | 0x48;
    pub const PAGE_UP: u16 = EXT | 0x49;
    pub const LEFT: u16 = EXT | 0x4B;
    pub const RIGHT: u16 = EXT | 0x4D;
    pub const END: u16 = EXT | 0x4F;
    pub const DOWN: u16 = EXT | 0x50;
    pub const PAGE_DOWN: u16 = EXT | 0x51;
    pub const INSERT: u16 = EXT | 0x52;
    pub const DELETE: u16 = EXT | 0x53;
    pub const LSUPER: u16 = EXT | 0x5B;
    pub const RSUPER: u16 = EXT | 0x5C;
    pub const MENU: u16 = EXT | 0x5D;

    /// A short name for logs; `None` for keys without one here.
    pub fn name(k: u16) -> Option<&'static str> {
        Some(match k {
            ESC => "Esc",
            BACKSPACE => "Backspace",
            TAB => "Tab",
            ENTER | KP_ENTER => "Enter",
            SPACE => "Space",
            F1 => "F1",
            F2 => "F2",
            F3 => "F3",
            F4 => "F4",
            F5 => "F5",
            F9 => "F9",
            F10 => "F10",
            F11 => "F11",
            F12 => "F12",
            UP => "Up",
            DOWN => "Down",
            LEFT => "Left",
            RIGHT => "Right",
            HOME => "Home",
            END => "End",
            PAGE_UP => "PageUp",
            PAGE_DOWN => "PageDown",
            INSERT => "Insert",
            DELETE => "Delete",
            _ => return None,
        })
    }

    /// True for the modifier keys themselves.
    pub const fn is_modifier(k: u16) -> bool {
        matches!(k, LSHIFT | RSHIFT | LCTRL | RCTRL | LALT | RALT | LSUPER | RSUPER)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn events_are_sixteen_bytes_without_padding() {
        assert_eq!(core::mem::size_of::<InputEvent>(), 16);
        let e = InputEvent {
            kind: kind::KEY,
            flags: flags::PRESSED | flags::ALT,
            key: key::TAB,
            ch: 0,
            time_ms: 7,
        };
        assert_eq!(InputEvent::from_bytes(e.as_bytes()), Some(e));
        assert!(e.is(key::TAB, flags::ALT));
        assert!(!e.is(key::TAB, 0));
    }
}
