//! The desktop contract (ADR-0020), version 0.
//!
//! Three parties talk here, each over its own channel:
//!
//! * a **client** and the display server `spacedesk`: the client draws its window
//!   into a memory object of its own and hands it over; the server maps it
//!   read-only and composes it with the others. Nothing else is shared, so a
//!   client cannot touch another window or the screen, and the server never writes
//!   into a client's memory;
//! * the **server** and its clients the other way: which window has the keyboard,
//!   the keys typed into it, a new size, a request to close, a request to describe
//!   itself;
//! * an **operator** (whoever started the server) and the server: launch an app,
//!   type a key as though from the keyboard, read the state of every window, ask a
//!   window what it shows, quit. That is the automation API the PRD asks the
//!   controls to have (§6, accessibility), and it is how the tests drive the
//!   desktop without a person.
//!
//! Every message is one fixed-size [`Msg`].

use crate::input::InputEvent;

pub const ABI_VERSION: u32 = 0;

/// Bytes of the text field: a title, a description, an app name.
pub const TEXT_MAX: usize = 180;

/// Smallest and largest window content a client may have.
pub const WINDOW_MIN_W: u32 = 160;
pub const WINDOW_MIN_H: u32 = 96;
pub const WINDOW_MAX_W: u32 = 1920;
pub const WINDOW_MAX_H: u32 = 1200;

/// Pixels in a window buffer: one `u32` each, `0x00RRGGBB`, rows packed
/// (`w * h * 4` bytes, row after row).
pub const BYTES_PER_PIXEL: u32 = 4;

pub mod msg {
    // ---- client -> server ----
    /// A window: `w`, `h`, title in `text`; carries the buffer (a memory object of at
    /// least `w * h * 4` bytes). Answered with [`CREATED`].
    pub const CREATE: u32 = 1;
    /// The rectangle `x, y, w, h` of the buffer changed.
    pub const DAMAGE: u32 = 2;
    /// A new title in `text`.
    pub const TITLE: u32 = 3;
    /// The answer to [`CONFIGURE`]: redrawn at `w`, `h`; carries the new buffer.
    pub const RESIZED: u32 = 4;
    /// The answer to [`DESCRIBE`]: what the window shows, in words, in `text`.
    pub const DESCRIPTION: u32 = 5;
    /// Show `text`, a path, in the app `value` (only [`super::app::FILES`]): a folder
    /// is opened, a file is selected in its folder. The server starts a new window
    /// for it; the asking app gets no answer and no capability.
    pub const OPEN: u32 = 6;

    // ---- server -> client ----
    /// First message to an app the server started: `value` is which app
    /// ([`super::app`]), `text` where it starts (a path, for the file manager); may
    /// carry the capability the app needs.
    pub const HELLO: u32 = 16;
    /// The answer to [`CREATE`]: `id` is the window, or `status` says why not.
    pub const CREATED: u32 = 17;
    /// Draw at `w`, `h` from now on, and answer [`RESIZED`].
    pub const CONFIGURE: u32 = 18;
    /// A key typed while this window had the keyboard (`event`).
    pub const KEY: u32 = 19;
    /// `value` 1: this window has the keyboard now; 0: it lost it.
    pub const FOCUS: u32 = 20;
    /// The person asked to close the window. A client that does not go within
    /// [`super::CLOSE_GRACE_MS`] is stopped.
    pub const CLOSE: u32 = 21;
    /// Say what the window shows ([`DESCRIPTION`]).
    pub const DESCRIBE: u32 = 22;

    // ---- operator <-> server ----
    /// Version check (`value` = [`super::ABI_VERSION`]); the reply's `w`, `h` are the
    /// screen's.
    pub const OP_HELLO: u32 = 32;
    /// Start the app named in `text`; the reply's `id` is its window once it has one.
    pub const OP_LAUNCH: u32 = 33;
    /// Handle `event` exactly as though it came from the keyboard.
    pub const OP_KEY: u32 = 34;
    /// The reply's `value` is the window count, `id` the focused window (0: none),
    /// `x` the workspace, `value2` frames presented; one [`OP_WINDOW`] message per
    /// window follows.
    pub const OP_STATE: u32 = 35;
    /// Ask window `id` to describe itself; the reply's `text` is its answer.
    pub const OP_DESCRIBE: u32 = 36;
    /// Close every window, give the screen back, exit 0.
    pub const OP_QUIT: u32 = 37;
    /// The answer to an operator request: `status` 0 or a negated error.
    pub const OP_REPLY: u32 = 48;
    /// One window: `id`, geometry, `value` = [`super::window_flags`], `value2` =
    /// workspace, `text` = title.
    pub const OP_WINDOW: u32 = 49;
}

/// Which app a client started by the server is (`HELLO.value`).
pub mod app {
    pub const TERMINAL: u32 = 1;
    pub const FILES: u32 = 2;
    pub const AGENT: u32 = 3;
    /// Search the indexed documents (SpaceLink) and act on what is found.
    pub const COMMAND: u32 = 4;

    /// The name the operator launches it by.
    pub fn by_name(name: &str) -> Option<u32> {
        match name {
            "terminal" => Some(TERMINAL),
            "files" => Some(FILES),
            "agent" => Some(AGENT),
            "command" => Some(COMMAND),
            _ => None,
        }
    }

    pub fn name(app: u32) -> &'static str {
        match app {
            TERMINAL => "terminal",
            FILES => "files",
            AGENT => "agent",
            COMMAND => "command",
            _ => "?",
        }
    }
}

pub mod window_flags {
    pub const FOCUSED: u32 = 1;
    pub const MINIMIZED: u32 = 2;
}

/// How long a client has to go after [`msg::CLOSE`] before it is stopped.
pub const CLOSE_GRACE_MS: u64 = 2000;

/// Workspaces the server keeps.
pub const WORKSPACES: u32 = 4;

/// Every message on every desktop channel.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct Msg {
    pub kind: u32,
    /// Replies: 0, or a negated [`crate::error::Error`].
    pub status: i32,
    pub id: u32,
    pub x: i32,
    pub y: i32,
    pub w: u32,
    pub h: u32,
    pub value: u32,
    pub value2: u64,
    pub event: InputEvent,
    pub text_len: u32,
    pub text: [u8; TEXT_MAX],
}

impl Default for Msg {
    fn default() -> Self {
        Msg {
            kind: 0,
            status: 0,
            id: 0,
            x: 0,
            y: 0,
            w: 0,
            h: 0,
            value: 0,
            value2: 0,
            event: InputEvent::default(),
            text_len: 0,
            text: [0; TEXT_MAX],
        }
    }
}

impl Msg {
    pub fn new(kind: u32) -> Msg {
        Msg { kind, ..Default::default() }
    }

    pub fn with_text(kind: u32, text: &str) -> Msg {
        let mut m = Msg::new(kind);
        m.set_text(text);
        m
    }

    /// Set the text, cut at a character boundary if it is longer than [`TEXT_MAX`].
    pub fn set_text(&mut self, s: &str) {
        let mut n = s.len().min(TEXT_MAX);
        while !s.is_char_boundary(n) {
            n -= 1;
        }
        self.text[..n].copy_from_slice(&s.as_bytes()[..n]);
        self.text_len = n as u32;
    }

    pub fn text(&self) -> &str {
        let n = (self.text_len as usize).min(TEXT_MAX);
        core::str::from_utf8(&self.text[..n]).unwrap_or("")
    }

    pub fn error(kind: u32, e: crate::error::Error) -> Msg {
        Msg { kind, status: -(e as i32), ..Default::default() }
    }

    pub fn result(&self) -> Result<(), crate::error::Error> {
        if self.status == 0 {
            Ok(())
        } else {
            Err(crate::error::Error::from_code((-self.status) as u32).unwrap_or(crate::error::Error::Invalid))
        }
    }

    pub fn as_bytes(&self) -> &[u8] {
        // SAFETY: `repr(C)` plain data with no padding (checked by the test below).
        unsafe { core::slice::from_raw_parts(self as *const Self as *const u8, core::mem::size_of::<Self>()) }
    }

    pub fn from_bytes(b: &[u8]) -> Option<Msg> {
        if b.len() != core::mem::size_of::<Msg>() {
            return None;
        }
        // SAFETY: exactly one `Msg` worth of bytes; every bit pattern is a valid `Msg`.
        Some(unsafe { core::ptr::read_unaligned(b.as_ptr() as *const Msg) })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_message_fits_a_channel_message_and_has_no_padding() {
        let size = core::mem::size_of::<Msg>();
        assert!(size <= crate::syscall::MSG_MAX);
        // 8 words, a u64, a 16-byte event, a u32 and the text: nothing implicit.
        assert_eq!(size, 8 * 4 + 8 + 16 + 4 + TEXT_MAX);
        let mut m = Msg::with_text(msg::TITLE, "Terminal — ü");
        m.id = 7;
        let back = Msg::from_bytes(m.as_bytes()).expect("round trip");
        assert_eq!((back.kind, back.id, back.text()), (msg::TITLE, 7, "Terminal — ü"));
    }

    #[test]
    fn long_text_is_cut_at_a_character_boundary() {
        let s = "é".repeat(100); // 200 bytes
        let m = Msg::with_text(msg::TITLE, &s);
        assert!(m.text().len() <= TEXT_MAX);
        assert!(m.text().chars().all(|c| c == 'é'));
    }
}
