//! `spacedesk` – the display server and window manager (U01, ADR-0020).
//!
//! It holds the screen (`SYS_DISPLAY_OPEN`) and the keyboard (`SYS_INPUT_READ`),
//! and gives neither away: every window is a memory object its client draws into
//! and this process maps read-only, and keys go to the one window that has the
//! keyboard. Window management -- focus, move, resize, minimize, close,
//! workspaces, launching apps -- is all on the keyboard.
//!
//! Started as `init=bin/spacedesk` it is the session: it opens a terminal, and the
//! machine shuts down when the person asks it to (Ctrl+Alt+Delete). Started by
//! another process, its bootstrap channel is its operator (`spaceabi::desk`,
//! `OP_*`): the tests drive it from there, through the same code a key press
//! takes.
#![no_std]
#![no_main]

extern crate alloc;

use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

use libspace::gfx::{GLYPH_H, Surface, contrast_x100, glyph_w};
use libspace::spaceabi::boot::fb_format;
use libspace::spaceabi::desk::{self, Msg, WORKSPACES, app, msg, window_flags};
use libspace::spaceabi::error::Error;
use libspace::spaceabi::handle::{kind as hkind, rights};
use libspace::spaceabi::input::{InputEvent, flags, key, kind};
use libspace::spaceabi::syscall::{DisplayInfo, INPUT_READ_MAX};
use libspace::{Handle, exit_kind, handle, kill_reason, println, sys};

/// Heap for the window list, titles and messages; the screen itself is mapped.
const HEAP_PAGES: usize = 64;
/// Pages for an app: code, heap, stack, and two window buffers during a resize.
const APP_QUOTA: u64 = 1536;
const MAX_WINDOWS: usize = 12;
const MAX_STARTING: usize = 4;
const TOP_BAR: i32 = 28;
const DOCK_H: i32 = 48;
const TITLE_H: i32 = 26;
/// Pixels a window moves or grows per key press.
const STEP: i32 = 32;
/// An app must have its window within this long of being started.
const START_MS: u64 = 5000;
/// At most one composition per this many milliseconds.
const FRAME_MS: u64 = 16;
/// Longest the loop waits for a message; the keyboard is polled at least this often.
const POLL_MS: u64 = 10;
/// Messages taken from one client per turn: a client that floods cannot starve
/// the others or the keyboard.
const MSGS_PER_TURN: usize = 16;
/// How long a window has to answer a request to describe itself.
const DESCRIBE_MS: u64 = 2000;

struct Palette {
    bg_top: u32,
    bg_bottom: u32,
    star: u32,
    bar: u32,
    bar_text: u32,
    accent: u32,
    /// Text on something filled with `accent`: the current workspace, the dock tile
    /// of the window with the keyboard.
    on_accent: u32,
    border: u32,
    title_focus: u32,
    title: u32,
    title_text_focus: u32,
    title_text: u32,
    dock: u32,
    tile: u32,
    tile_text: u32,
    dim_text: u32,
    shadow: u32,
}

const NORMAL: Palette = Palette {
    bg_top: 0x0B1220,
    bg_bottom: 0x1C2D4F,
    star: 0x9FB4D8,
    bar: 0x0A0F1A,
    bar_text: 0xD8E0EC,
    accent: 0x4C8DFF,
    on_accent: 0x06101F,
    border: 0x2A3A58,
    title_focus: 0x22314D,
    title: 0x161F31,
    title_text_focus: 0xFFFFFF,
    title_text: 0x8E9BB5,
    dock: 0x121A2A,
    tile: 0x1F2B42,
    tile_text: 0xD8E0EC,
    dim_text: 0x8E9BB5,
    shadow: 0x03060C,
};

/// High contrast (Super+H): black, white, and a focus that cannot be missed.
const CONTRAST: Palette = Palette {
    bg_top: 0x000000,
    bg_bottom: 0x000000,
    star: 0x000000,
    bar: 0x000000,
    bar_text: 0xFFFFFF,
    accent: 0xFFD400,
    on_accent: 0x000000,
    border: 0xFFFFFF,
    title_focus: 0x000000,
    title: 0x000000,
    title_text_focus: 0xFFD400,
    title_text: 0xFFFFFF,
    dock: 0x000000,
    tile: 0x000000,
    tile_text: 0xFFFFFF,
    dim_text: 0xBBBBBB,
    shadow: 0x000000,
};

/// Hold a palette to a contrast floor for each pair of colours drawn together.
macro_rules! readable {
    ($p:ident, $min:expr, $($fg:ident on $bg:ident),+ $(,)?) => {
        $(assert!(
            contrast_x100($p.$fg, $p.$bg) >= $min,
            concat!(stringify!($p), ": ", stringify!($fg), " on ", stringify!($bg), " is too faint to read")
        );)+
    };
}

// Every text colour against everything it is drawn on -- 4.5:1 (WCAG AA) in the
// normal theme, 7:1 (AAA) in high contrast -- and the focus frame at 3:1 against
// what surrounds it. Checked when the desktop is compiled: a colour change that
// makes something unreadable does not build.
const _: () = {
    readable!(
        NORMAL, 450,
        dim_text on bg_top, dim_text on bg_bottom, title_text_focus on title_focus, title_text on title,
        bar_text on bar, on_accent on accent, tile_text on tile, dim_text on tile, dim_text on dock,
    );
    readable!(
        CONTRAST, 700,
        dim_text on bg_top, dim_text on bg_bottom, title_text_focus on title_focus, title_text on title,
        bar_text on bar, on_accent on accent, tile_text on tile, dim_text on tile, dim_text on dock,
    );
    readable!(NORMAL, 300, accent on bg_top, accent on bg_bottom, accent on title_focus, accent on dock);
    readable!(CONTRAST, 300, accent on bg_top, accent on bg_bottom, accent on title_focus, accent on dock);
};

/// The screen: the framebuffer, and the buffer frames are composed in.
struct Screen {
    info: DisplayInfo,
    /// Pixel (0, 0) of the mapped framebuffer.
    fb: *mut u8,
    back: *mut u32,
    w: i32,
    h: i32,
}

impl Screen {
    fn open(root: Handle) -> Result<Screen, String> {
        let display = sys::display_open(root).map_err(|e| format!("display_open: {e}"))?;
        let info = sys::display_info(display).map_err(|e| format!("display_info: {e}"))?;
        let base = sys::vmo_map(display, false).map_err(|e| format!("map the screen: {e}"))?;
        // The mapping holds the lease from here; the handle is no longer needed.
        sys::handle_close(display).ok();
        let (w, h) = (info.width as i32, info.height as i32);
        let back = sys::mem_map(w as usize * h as usize * 4).map_err(|e| format!("back buffer: {e}"))?;
        // SAFETY: `offset` is inside the mapping the kernel just made (`size` bytes).
        let fb = unsafe { base.add(info.offset as usize) };
        Ok(Screen { info, fb, back: back as *mut u32, w, h })
    }

    fn surface(&mut self) -> Surface<'_> {
        // SAFETY: `w * h` pixels mapped read-write by `open`, for this process alone.
        let px = unsafe { core::slice::from_raw_parts_mut(self.back, self.w as usize * self.h as usize) };
        Surface::new(px, self.w, self.h, self.w as usize)
    }

    /// Copy the composed frame to the framebuffer, converting to its byte order.
    fn present(&mut self) {
        let stride = self.info.stride as usize;
        let rgbx = self.info.format == fb_format::RGBX;
        for y in 0..self.h as usize {
            // SAFETY: row `y` of the back buffer (`w` pixels) and of the framebuffer
            // (`stride >= w` pixels), both mapped.
            unsafe {
                let src = core::slice::from_raw_parts(self.back.add(y * self.w as usize), self.w as usize);
                let dst = self.fb.add(y * stride * 4) as *mut u32;
                if rgbx {
                    for (x, &p) in src.iter().enumerate() {
                        let v = ((p & 0xFF) << 16) | (p & 0xFF00) | ((p >> 16) & 0xFF);
                        dst.add(x).write_volatile(v);
                    }
                } else {
                    core::ptr::copy_nonoverlapping(src.as_ptr(), dst, src.len());
                }
            }
        }
    }
}

/// A window's pixels as the server sees them: the client's memory object, mapped
/// read-only here.
struct Buffer {
    ptr: *const u32,
    w: u32,
    h: u32,
    bytes: usize,
}

impl Buffer {
    /// Map `handle` if it really is `w` by `h` pixels; the handle is closed either
    /// way (the mapping keeps the object alive).
    fn take(handle: Handle, w: u32, h: u32) -> Result<Buffer, Error> {
        let r = (|| {
            if !(desk::WINDOW_MIN_W..=desk::WINDOW_MAX_W).contains(&w)
                || !(desk::WINDOW_MIN_H..=desk::WINDOW_MAX_H).contains(&h)
            {
                return Err(Error::Invalid);
            }
            let info = sys::handle_info(handle)?;
            if info.kind != hkind::MEMORY {
                return Err(Error::Denied);
            }
            let bytes = w as usize * h as usize * 4;
            if sys::vmo_size(handle)? < bytes {
                return Err(Error::MsgSize);
            }
            let ptr = sys::vmo_map(handle, true)?;
            Ok(Buffer { ptr: ptr as *const u32, w, h, bytes: sys::vmo_size(handle)? })
        })();
        sys::handle_close(handle).ok();
        r
    }

    fn pixels(&self) -> &[u32] {
        // SAFETY: mapped read-only by `take`, `w * h` pixels at least, until drop.
        unsafe { core::slice::from_raw_parts(self.ptr, self.w as usize * self.h as usize) }
    }
}

impl Drop for Buffer {
    fn drop(&mut self) {
        sys::mem_unmap(self.ptr as *mut u8, self.bytes).ok();
    }
}

struct Window {
    id: u32,
    app: u32,
    chan: Handle,
    /// The client process, when this server started it.
    process: Option<Handle>,
    title: String,
    /// Top-left corner of the content (the title bar sits above it).
    x: i32,
    y: i32,
    /// Size of the content; differs from the buffer's only while a resize is being
    /// answered.
    w: u32,
    h: u32,
    buf: Buffer,
    workspace: u32,
    minimized: bool,
    /// When a request to close runs out.
    close_by: Option<u64>,
}

/// An app started but without a window yet.
struct Starting {
    app: u32,
    chan: Handle,
    process: Handle,
    deadline: u64,
    for_operator: bool,
}

/// An operator's request that waits for a client.
enum Pending {
    Launch,
    Describe { id: u32, until: u64 },
}

struct Desk {
    root: Handle,
    standalone: bool,
    screen: Screen,
    /// Bottom to top.
    windows: Vec<Window>,
    starting: Vec<Starting>,
    focus: Option<u32>,
    workspace: u32,
    next_id: u32,
    frames: u64,
    dirty: bool,
    last_frame: u64,
    operator: Option<Handle>,
    pending: Option<Pending>,
    contrast: bool,
    quit: bool,
}

fn now() -> u64 {
    sys::ticks_ms()
}

fn describe_exit(st: libspace::ExitStatus) -> String {
    if st.kind == exit_kind::EXITED {
        format!("exited with code {}", st.code)
    } else {
        format!("killed ({})", kill_reason::name(st.reason))
    }
}

fn key_combo(e: &InputEvent) -> String {
    let mut s = String::new();
    for (bit, name) in
        [(flags::CTRL, "Ctrl+"), (flags::ALT, "Alt+"), (flags::SUPER, "Super+"), (flags::SHIFT, "Shift+")]
    {
        if e.flags & bit != 0 {
            s.push_str(name);
        }
    }
    // Letters and digits by where they sit on the keyboard (set 1 make codes):
    // with a modifier held they type nothing, so `ch` cannot name them.
    const ROWS: [(u16, &str); 4] =
        [(0x02, "1234567890"), (0x10, "QWERTYUIOP"), (0x1E, "ASDFGHJKL"), (0x2C, "ZXCVBNM")];
    let placed = ROWS.iter().find_map(|&(first, keys)| {
        let i = e.key.checked_sub(first)? as usize;
        keys.as_bytes().get(i).map(|&b| b as char)
    });
    match (key::name(e.key), placed) {
        (Some(n), _) => s.push_str(n),
        (None, Some(c)) => s.push(c),
        (None, None) => s.push_str(&format!("key {:#x}", e.key)),
    }
    s
}

impl Desk {
    fn palette(&self) -> &'static Palette {
        if self.contrast { &CONTRAST } else { &NORMAL }
    }

    fn index(&self, id: u32) -> Option<usize> {
        self.windows.iter().position(|w| w.id == id)
    }

    fn visible(&self, w: &Window) -> bool {
        w.workspace == self.workspace && !w.minimized
    }

    // ---- focus ------------------------------------------------------------------

    fn set_focus(&mut self, id: Option<u32>) {
        if self.focus == id {
            return;
        }
        if let Some(old) = self.focus.and_then(|f| self.index(f)) {
            let mut m = Msg::new(msg::FOCUS);
            m.value = 0;
            sys::send(self.windows[old].chan, m.as_bytes(), None).ok();
        }
        self.focus = id;
        if let Some(i) = id.and_then(|f| self.index(f)) {
            // Focus raises: the window with the keyboard is the one on top.
            let w = self.windows.remove(i);
            self.windows.push(w);
            let top = self.windows.len() - 1;
            let mut m = Msg::new(msg::FOCUS);
            m.value = 1;
            sys::send(self.windows[top].chan, m.as_bytes(), None).ok();
            println!("[desk] focus: '{}' (window {})", self.windows[top].title, self.windows[top].id);
        }
        self.dirty = true;
    }

    /// Give the keyboard to the topmost visible window, if any.
    fn refocus(&mut self) {
        let top = self.windows.iter().rev().find(|w| self.visible(w)).map(|w| w.id);
        if top.is_none() && self.focus.is_some() {
            println!("[desk] focus: none");
        }
        if top.is_none() {
            self.focus = None;
            self.dirty = true;
            return;
        }
        self.set_focus(top);
    }

    /// Alt+Tab: the next window on this workspace, minimized ones included (they
    /// come back when chosen).
    fn cycle(&mut self, back: bool) {
        let mut ids: Vec<u32> =
            self.windows.iter().filter(|w| w.workspace == self.workspace).map(|w| w.id).collect();
        if ids.is_empty() {
            return;
        }
        ids.sort_unstable();
        let at = self.focus.and_then(|f| ids.iter().position(|&i| i == f));
        let next = match (at, back) {
            (None, _) => ids[0],
            (Some(i), false) => ids[(i + 1) % ids.len()],
            (Some(i), true) => ids[(i + ids.len() - 1) % ids.len()],
        };
        if let Some(i) = self.index(next)
            && self.windows[i].minimized
        {
            self.windows[i].minimized = false;
            println!("[desk] restored '{}'", self.windows[i].title);
        }
        self.set_focus(Some(next));
    }

    // ---- window actions --------------------------------------------------------

    fn focused_index(&self) -> Option<usize> {
        self.focus.and_then(|f| self.index(f))
    }

    fn minimize(&mut self) {
        let Some(i) = self.focused_index() else { return };
        self.windows[i].minimized = true;
        println!("[desk] minimized '{}'", self.windows[i].title);
        self.focus = None;
        let mut m = Msg::new(msg::FOCUS);
        m.value = 0;
        sys::send(self.windows[i].chan, m.as_bytes(), None).ok();
        self.refocus();
    }

    fn close(&mut self) {
        let Some(i) = self.focused_index() else { return };
        let w = &mut self.windows[i];
        if w.close_by.is_none() {
            w.close_by = Some(now() + desk::CLOSE_GRACE_MS);
            sys::send(w.chan, Msg::new(msg::CLOSE).as_bytes(), None).ok();
            println!("[desk] asked '{}' to close", w.title);
        }
    }

    fn move_by(&mut self, dx: i32, dy: i32) {
        let (sw, sh) = (self.screen.w, self.screen.h);
        let Some(i) = self.focused_index() else { return };
        let w = &mut self.windows[i];
        // The title bar stays on the screen: a window can always be brought back.
        w.x = (w.x + dx).clamp(80 - w.w as i32, sw - 80);
        w.y = (w.y + dy).clamp(TOP_BAR + TITLE_H, sh - DOCK_H);
        println!("[desk] moved '{}' to {},{}", w.title, w.x, w.y);
        self.dirty = true;
    }

    fn resize_by(&mut self, dw: i32, dh: i32) {
        let (sw, sh) = (self.screen.w, self.screen.h);
        let Some(i) = self.focused_index() else { return };
        let w = &mut self.windows[i];
        let max_w = (sw as u32).min(desk::WINDOW_MAX_W);
        let max_h = ((sh - TOP_BAR - TITLE_H - DOCK_H) as u32).min(desk::WINDOW_MAX_H);
        let nw = (w.w as i32 + dw).clamp(desk::WINDOW_MIN_W as i32, max_w as i32) as u32;
        let nh = (w.h as i32 + dh).clamp(desk::WINDOW_MIN_H as i32, max_h as i32) as u32;
        if (nw, nh) == (w.w, w.h) {
            return;
        }
        w.w = nw;
        w.h = nh;
        let mut m = Msg::new(msg::CONFIGURE);
        m.w = nw;
        m.h = nh;
        sys::send(w.chan, m.as_bytes(), None).ok();
        println!("[desk] resized '{}' to {nw}x{nh}", w.title);
        self.dirty = true;
    }

    fn switch_workspace(&mut self, to: u32, take_focused: bool) {
        if to == self.workspace {
            return;
        }
        if take_focused && let Some(i) = self.focused_index() {
            self.windows[i].workspace = to;
            println!("[desk] moved '{}' to workspace {}", self.windows[i].title, to + 1);
        }
        let carried = if take_focused { self.focus } else { None };
        self.workspace = to;
        println!("[desk] workspace {}", to + 1);
        self.dirty = true;
        match carried {
            Some(id) => {
                self.focus = None;
                self.set_focus(Some(id));
            }
            None => {
                self.focus = None;
                self.refocus();
            }
        }
    }

    // ---- keys ------------------------------------------------------------------

    /// One key, from the keyboard or the operator: the window manager's shortcuts
    /// first, everything else to the window with the keyboard.
    fn key(&mut self, e: InputEvent) {
        if e.kind == kind::LOST {
            println!("[desk] {} key event(s) were lost", e.ch);
            return;
        }
        if e.kind != kind::KEY || !e.pressed() || key::is_modifier(e.key) {
            return;
        }
        let m = e.mods();
        const ALT: u8 = flags::ALT;
        const SHIFT: u8 = flags::SHIFT;
        const CTRL: u8 = flags::CTRL;
        const SUPER: u8 = flags::SUPER;
        let ws = self.workspace;
        let shortcut = match (e.key, m) {
            (key::TAB, ALT) => {
                self.cycle(false);
                true
            }
            (key::TAB, x) if x == ALT | SHIFT => {
                self.cycle(true);
                true
            }
            (key::M, SUPER) | (key::F9, ALT) => {
                self.minimize();
                true
            }
            (key::Q, SUPER) | (key::F4, ALT) => {
                self.close();
                true
            }
            (key::LEFT, ALT) => {
                self.move_by(-STEP, 0);
                true
            }
            (key::RIGHT, ALT) => {
                self.move_by(STEP, 0);
                true
            }
            (key::UP, ALT) => {
                self.move_by(0, -STEP);
                true
            }
            (key::DOWN, ALT) => {
                self.move_by(0, STEP);
                true
            }
            (key::LEFT, x) if x == ALT | SHIFT => {
                self.resize_by(-STEP, 0);
                true
            }
            (key::RIGHT, x) if x == ALT | SHIFT => {
                self.resize_by(STEP, 0);
                true
            }
            (key::UP, x) if x == ALT | SHIFT => {
                self.resize_by(0, -STEP);
                true
            }
            (key::DOWN, x) if x == ALT | SHIFT => {
                self.resize_by(0, STEP);
                true
            }
            (key::LEFT, x) if x == CTRL | ALT => {
                self.switch_workspace((ws + WORKSPACES - 1) % WORKSPACES, false);
                true
            }
            (key::RIGHT, x) if x == CTRL | ALT => {
                self.switch_workspace((ws + 1) % WORKSPACES, false);
                true
            }
            (key::LEFT, x) if x == CTRL | ALT | SHIFT => {
                self.switch_workspace((ws + WORKSPACES - 1) % WORKSPACES, true);
                true
            }
            (key::RIGHT, x) if x == CTRL | ALT | SHIFT => {
                self.switch_workspace((ws + 1) % WORKSPACES, true);
                true
            }
            (key::ENTER, SUPER) | (key::F1, ALT) => {
                self.launch(app::TERMINAL, false).ok();
                true
            }
            (key::E, SUPER) | (key::F2, ALT) => {
                self.launch(app::FILES, false).ok();
                true
            }
            (key::A, SUPER) | (key::F3, ALT) => {
                self.launch(app::AGENT, false).ok();
                true
            }
            (key::H, SUPER) => {
                self.contrast = !self.contrast;
                println!("[desk] high contrast {}", if self.contrast { "on" } else { "off" });
                self.dirty = true;
                true
            }
            (key::DELETE, x) if x == CTRL | ALT && self.standalone => {
                println!("[desk] shutting down at the person's request");
                self.quit = true;
                true
            }
            _ => false,
        };
        if shortcut {
            println!("[desk] shortcut {}", key_combo(&e));
            return;
        }
        if let Some(i) = self.focused_index() {
            let mut out = Msg::new(msg::KEY);
            out.event = e;
            // A window that stopped reading loses keys, not the desktop.
            sys::send(self.windows[i].chan, out.as_bytes(), None).ok();
        }
    }

    // ---- apps --------------------------------------------------------------------

    /// Start an app; its window comes when it asks for one.
    fn launch(&mut self, which: u32, for_operator: bool) -> Result<(), Error> {
        if self.windows.len() + self.starting.len() >= MAX_WINDOWS || self.starting.len() >= MAX_STARTING {
            println!("[desk] not starting {}: too many windows", app::name(which));
            return Err(Error::Busy);
        }
        // Each app gets what it needs and no more: the terminal and the Agent Center
        // run a session (they spawn it and list files through it); the file manager
        // reads the volume.
        let need = match which {
            // DUP: they narrow it once more for the session they start.
            app::TERMINAL | app::AGENT => rights::SPAWN | rights::FS | rights::TRANSFER | rights::DUP,
            app::FILES => rights::FS | rights::TRANSFER,
            _ => return Err(Error::Invalid),
        };
        let cap = sys::handle_dup(self.root, need)?;
        let (mine, theirs) = match sys::channel_create() {
            Ok(c) => c,
            Err(e) => {
                sys::handle_close(cap).ok();
                return Err(e);
            }
        };
        let process = match sys::spawn(self.root, "bin/deskapps", APP_QUOTA, Some(theirs)) {
            Ok(p) => p,
            Err(e) => {
                for h in [cap, mine, theirs] {
                    sys::handle_close(h).ok();
                }
                println!("[desk] cannot start {}: {e}", app::name(which));
                return Err(e);
            }
        };
        let mut hello = Msg::new(msg::HELLO);
        hello.value = which;
        if let Err(e) = sys::send(mine, hello.as_bytes(), Some(cap)) {
            sys::handle_close(cap).ok();
            sys::kill(process).ok();
            sys::wait(process).ok();
            for h in [mine, process] {
                sys::handle_close(h).ok();
            }
            return Err(e);
        }
        println!("[desk] starting {}", app::name(which));
        self.starting.push(Starting {
            app: which,
            chan: mine,
            process,
            deadline: now() + START_MS,
            for_operator,
        });
        Ok(())
    }

    /// A client asks for a window.
    fn create(&mut self, s: Starting, m: &Msg, carried: Option<Handle>) -> Result<(), Error> {
        let Some(handle) = carried else {
            let reply = Msg::error(msg::CREATED, Error::Invalid);
            sys::send(s.chan, reply.as_bytes(), None).ok();
            self.drop_starting(s, "asked for a window without a buffer");
            return Err(Error::Invalid);
        };
        let buf = match Buffer::take(handle, m.w, m.h) {
            Ok(b) => b,
            Err(e) => {
                sys::send(s.chan, Msg::error(msg::CREATED, e).as_bytes(), None).ok();
                self.drop_starting(s, &format!("its window buffer was refused ({e})"));
                return Err(e);
            }
        };
        self.next_id += 1;
        let id = self.next_id;
        let (x, y) = self.place(m.w as i32, m.h as i32);
        let title = if m.text().is_empty() { app::name(s.app).to_string() } else { m.text().to_string() };
        let mut reply = Msg::new(msg::CREATED);
        reply.id = id;
        sys::send(s.chan, reply.as_bytes(), None).ok();
        println!("[desk] window {id} '{title}' ({}) opened at {x},{y}, {}x{}", app::name(s.app), m.w, m.h);
        if s.for_operator
            && let (Some(op), Some(Pending::Launch)) = (self.operator, &self.pending)
        {
            let mut r = Msg::new(msg::OP_REPLY);
            r.id = id;
            sys::send(op, r.as_bytes(), None).ok();
            self.pending = None;
        }
        self.windows.push(Window {
            id,
            app: s.app,
            chan: s.chan,
            process: Some(s.process),
            title,
            x,
            y,
            w: m.w,
            h: m.h,
            buf,
            workspace: self.workspace,
            minimized: false,
            close_by: None,
        });
        self.set_focus(Some(id));
        self.dirty = true;
        Ok(())
    }

    /// Where a new `w` by `h` window covers the least of the windows already on this
    /// workspace: candidates on a coarse grid, first one wins a tie.
    fn place(&self, w: i32, h: i32) -> (i32, i32) {
        let (x_max, y_min) = ((self.screen.w - w - 20).max(20), TOP_BAR + TITLE_H + 16);
        let y_max = (self.screen.h - DOCK_H - h - 16).max(y_min);
        let mut best = (20, y_min);
        let mut best_cover = i64::MAX;
        let mut y = y_min;
        while y <= y_max {
            let mut x = 20;
            while x <= x_max {
                let cover: i64 = self
                    .windows
                    .iter()
                    .filter(|o| self.visible(o))
                    .map(|o| {
                        let (ox0, oy0) = (o.x - 8, o.y - TITLE_H - 8);
                        let (ox1, oy1) = (o.x + o.w as i32 + 8, o.y + o.h as i32 + 8);
                        let ix = (x + w).min(ox1) - x.max(ox0);
                        let iy = (y + h).min(oy1) - (y - TITLE_H).max(oy0);
                        if ix > 0 && iy > 0 { ix as i64 * iy as i64 } else { 0 }
                    })
                    .sum();
                if cover < best_cover {
                    best_cover = cover;
                    best = (x, y);
                }
                x += 40;
            }
            y += 40;
        }
        best
    }

    fn drop_starting(&mut self, s: Starting, why: &str) {
        println!("[desk] {} did not start: {why}", app::name(s.app));
        sys::kill(s.process).ok();
        sys::wait(s.process).ok();
        for h in [s.chan, s.process] {
            sys::handle_close(h).ok();
        }
        if s.for_operator
            && let (Some(op), Some(Pending::Launch)) = (self.operator, &self.pending)
        {
            sys::send(op, Msg::error(msg::OP_REPLY, Error::Unreachable).as_bytes(), None).ok();
            self.pending = None;
        }
    }

    /// Take a window away: its client is gone, or has to go.
    fn remove(&mut self, i: usize, why: &str) {
        let w = self.windows.remove(i);
        println!("[desk] window {} '{}' closed: {why}", w.id, w.title);
        if let Some(p) = w.process {
            // Nothing is left to wait for once the window is gone.
            sys::kill(p).ok();
            sys::wait(p).ok();
            sys::handle_close(p).ok();
        }
        sys::handle_close(w.chan).ok();
        if let Some(Pending::Describe { id, .. }) = self.pending
            && id == w.id
        {
            self.answer_describe(Err(Error::PeerClosed));
        }
        if self.focus == Some(w.id) {
            self.focus = None;
            self.refocus();
        }
        self.dirty = true;
        drop(w); // unmaps the buffer
    }

    fn answer_describe(&mut self, r: Result<String, Error>) {
        if let Some(op) = self.operator {
            let m = match r {
                Ok(text) => Msg::with_text(msg::OP_REPLY, &text),
                Err(e) => Msg::error(msg::OP_REPLY, e),
            };
            sys::send(op, m.as_bytes(), None).ok();
        }
        self.pending = None;
    }

    // ---- messages ------------------------------------------------------------------

    fn serve_starting(&mut self) {
        let mut i = 0;
        while i < self.starting.len() {
            let mut buf = [0u8; core::mem::size_of::<Msg>()];
            match sys::recv(self.starting[i].chan, &mut buf, true) {
                Ok((n, carried)) => {
                    let s = self.starting.remove(i);
                    match Msg::from_bytes(&buf[..n]) {
                        Some(m) if m.kind == msg::CREATE => {
                            self.create(s, &m, carried).ok();
                        }
                        _ => {
                            if let Some(h) = carried {
                                sys::handle_close(h).ok();
                            }
                            self.drop_starting(s, "its first message was not a window");
                        }
                    }
                }
                Err(Error::WouldBlock) => {
                    if now() > self.starting[i].deadline {
                        let s = self.starting.remove(i);
                        self.drop_starting(s, "no window in time");
                    } else {
                        i += 1;
                    }
                }
                Err(_) => {
                    let s = self.starting.remove(i);
                    let why = match sys::wait_nonblocking(s.process) {
                        Ok(st) => format!("it {}", describe_exit(st)),
                        Err(_) => String::from("it closed its channel"),
                    };
                    self.drop_starting(s, &why);
                }
            }
        }
    }

    /// Everything the clients sent since last time, a bounded amount from each.
    fn serve_windows(&mut self) {
        let mut i = 0;
        while i < self.windows.len() {
            let mut gone = None;
            for _ in 0..MSGS_PER_TURN {
                let mut buf = [0u8; core::mem::size_of::<Msg>()];
                match sys::recv(self.windows[i].chan, &mut buf, true) {
                    Ok((n, carried)) => {
                        let Some(m) = Msg::from_bytes(&buf[..n]) else {
                            if let Some(h) = carried {
                                sys::handle_close(h).ok();
                            }
                            continue;
                        };
                        self.window_msg(i, &m, carried);
                    }
                    Err(Error::WouldBlock) => break,
                    Err(_) => {
                        gone = Some(String::from("the app closed its window"));
                        break;
                    }
                }
            }
            if gone.is_none()
                && let Some(p) = self.windows[i].process
                && let Ok(st) = sys::wait_nonblocking(p)
            {
                gone = Some(format!("the app {}", describe_exit(st)));
            }
            if gone.is_none()
                && let Some(t) = self.windows[i].close_by
                && now() > t
            {
                gone = Some(String::from("it did not close when asked, so it was stopped"));
            }
            match gone {
                Some(why) => self.remove(i, &why),
                None => i += 1,
            }
        }
    }

    fn window_msg(&mut self, i: usize, m: &Msg, carried: Option<Handle>) {
        match m.kind {
            msg::DAMAGE => self.dirty = true,
            msg::TITLE => {
                self.windows[i].title = m.text().to_string();
                self.dirty = true;
            }
            msg::RESIZED => {
                let Some(h) = carried else { return };
                match Buffer::take(h, m.w, m.h) {
                    Ok(b) => {
                        let w = &mut self.windows[i];
                        w.buf = b; // the old buffer is unmapped here
                        w.w = m.w;
                        w.h = m.h;
                        self.dirty = true;
                    }
                    Err(e) => {
                        println!("[desk] '{}' sent a buffer that was refused: {e}", self.windows[i].title)
                    }
                }
                return;
            }
            msg::DESCRIPTION => {
                if let Some(Pending::Describe { id, .. }) = self.pending
                    && id == self.windows[i].id
                {
                    self.answer_describe(Ok(m.text().to_string()));
                }
            }
            _ => {}
        }
        if let Some(h) = carried {
            sys::handle_close(h).ok();
        }
    }

    fn serve_operator(&mut self) {
        let Some(op) = self.operator else { return };
        for _ in 0..MSGS_PER_TURN {
            let mut buf = [0u8; core::mem::size_of::<Msg>()];
            match sys::recv(op, &mut buf, true) {
                Ok((n, carried)) => {
                    if let Some(h) = carried {
                        sys::handle_close(h).ok();
                    }
                    match Msg::from_bytes(&buf[..n]) {
                        Some(m) => self.op_msg(op, &m),
                        None => {
                            sys::send(op, Msg::error(msg::OP_REPLY, Error::Invalid).as_bytes(), None).ok();
                        }
                    }
                }
                Err(Error::WouldBlock) => return,
                Err(_) => {
                    println!("[desk] the operator went away; closing");
                    self.operator = None;
                    self.quit = true;
                    return;
                }
            }
        }
    }

    fn op_msg(&mut self, op: Handle, m: &Msg) {
        let reply = |r: Msg| {
            sys::send(op, r.as_bytes(), None).ok();
        };
        match m.kind {
            msg::OP_LAUNCH => {
                if self.pending.is_some() {
                    return reply(Msg::error(msg::OP_REPLY, Error::WouldBlock));
                }
                match app::by_name(m.text()) {
                    None => reply(Msg::error(msg::OP_REPLY, Error::NotFound)),
                    Some(a) => match self.launch(a, true) {
                        Ok(()) => self.pending = Some(Pending::Launch),
                        Err(e) => reply(Msg::error(msg::OP_REPLY, e)),
                    },
                }
            }
            msg::OP_KEY => {
                self.key(m.event);
                reply(Msg::new(msg::OP_REPLY));
            }
            msg::OP_STATE => {
                let mut r = Msg::new(msg::OP_REPLY);
                r.value = self.windows.len() as u32;
                r.id = self.focus.unwrap_or(0);
                r.x = self.workspace as i32;
                r.value2 = self.frames;
                r.w = self.screen.w as u32;
                r.h = self.screen.h as u32;
                reply(r);
                for w in &self.windows {
                    let mut wm = Msg::with_text(msg::OP_WINDOW, &w.title);
                    wm.id = w.id;
                    wm.x = w.x;
                    wm.y = w.y;
                    wm.w = w.w;
                    wm.h = w.h;
                    wm.value = (if self.focus == Some(w.id) { window_flags::FOCUSED } else { 0 })
                        | (if w.minimized { window_flags::MINIMIZED } else { 0 });
                    wm.value2 = w.workspace as u64;
                    wm.status = w.app as i32;
                    reply(wm);
                }
            }
            msg::OP_DESCRIBE => {
                if self.pending.is_some() {
                    return reply(Msg::error(msg::OP_REPLY, Error::WouldBlock));
                }
                match self.index(m.id) {
                    None => reply(Msg::error(msg::OP_REPLY, Error::NotFound)),
                    Some(i) => {
                        sys::send(self.windows[i].chan, Msg::new(msg::DESCRIBE).as_bytes(), None).ok();
                        self.pending = Some(Pending::Describe { id: m.id, until: now() + DESCRIBE_MS });
                    }
                }
            }
            msg::OP_QUIT => {
                self.quit = true;
                reply(Msg::new(msg::OP_REPLY));
            }
            _ => reply(Msg::error(msg::OP_REPLY, Error::NoSys)),
        }
    }

    // ---- drawing ---------------------------------------------------------------------

    fn compose(&mut self) {
        let pal = self.palette();
        let gw = glyph_w();
        let focus = self.focus;
        let workspace = self.workspace;
        let (sw, sh) = (self.screen.w, self.screen.h);
        let clock = sys::clock_realtime_ms().ok().map(|ms| {
            let day = ms / 1000 % 86_400;
            format!("{:02}:{:02} UTC", day / 3600, day / 60 % 60)
        });
        let windows = core::mem::take(&mut self.windows);
        let mut s = self.screen.surface();

        // The sky: a gradient, and a few stars at fixed places.
        for y in 0..sh {
            let t = (y * 255 / sh.max(1)) as u32;
            let c = libspace::gfx::blend(pal.bg_bottom, pal.bg_top, t);
            s.fill(0, y, sw, 1, c);
        }
        let mut seed: u32 = 0x5EED_0F05;
        for _ in 0..140 {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            let x = (seed % sw.max(1) as u32) as i32;
            let y = TOP_BAR + ((seed >> 11) % (sh - TOP_BAR - DOCK_H).max(1) as u32) as i32;
            s.fill(x, y, 1 + (seed >> 29) as i32 % 2, 1, pal.star);
        }
        let mark = "Space OS Developer Preview";
        s.text(sw - mark.len() as i32 * gw - 16, sh - DOCK_H - 28, mark, pal.dim_text);

        for w in windows.iter().filter(|w| w.workspace == workspace && !w.minimized) {
            let focused = focus == Some(w.id);
            let (x, y, ww, wh) = (w.x, w.y, w.w as i32, w.h as i32);
            s.shade(x - 1 + 6, y - TITLE_H - 1 + 6, ww + 2, wh + TITLE_H + 2, pal.shadow, 110);
            s.fill(x, y - TITLE_H, ww, TITLE_H, if focused { pal.title_focus } else { pal.title });
            let title_color = if focused { pal.title_text_focus } else { pal.title_text };
            let max_chars = ((ww - 60) / gw).max(0) as usize;
            let title: String = w.title.chars().take(max_chars).collect();
            s.text(x + 10, y - TITLE_H + (TITLE_H - GLYPH_H) / 2, &title, title_color);
            s.text(x + ww - 3 * gw - 10, y - TITLE_H + (TITLE_H - GLYPH_H) / 2, "_ x", title_color);
            let px = w.buf.pixels();
            let (bw, bh) = (w.buf.w as i32, w.buf.h as i32);
            s.blit(px, bw.min(ww), bh.min(wh), w.buf.w as usize, x, y);
            if bw < ww {
                s.fill(x + bw, y, ww - bw, wh, pal.title);
            }
            if bh < wh {
                s.fill(x, y + bh, ww, wh - bh, pal.title);
            }
            let t = if focused { 2 } else { 1 };
            s.frame(
                x - t,
                y - TITLE_H - t,
                ww + 2 * t,
                wh + TITLE_H + 2 * t,
                t,
                if focused { pal.accent } else { pal.border },
            );
        }

        // Top bar: the name, the workspaces, the time.
        s.fill(0, 0, sw, TOP_BAR, pal.bar);
        s.text(12, (TOP_BAR - GLYPH_H) / 2, "Space OS", pal.bar_text);
        let boxes_w = WORKSPACES as i32 * 30;
        for n in 0..WORKSPACES as i32 {
            let bx = (sw - boxes_w) / 2 + n * 30;
            if n as u32 == workspace {
                s.fill(bx, 5, 24, TOP_BAR - 10, pal.accent);
            } else {
                s.frame(bx, 5, 24, TOP_BAR - 10, 1, pal.border);
            }
            let label = [b'1' + n as u8];
            s.text(
                bx + (24 - gw) / 2,
                (TOP_BAR - GLYPH_H) / 2,
                core::str::from_utf8(&label).unwrap_or("?"),
                if n as u32 == workspace { pal.on_accent } else { pal.bar_text },
            );
        }
        if let Some(c) = &clock {
            s.text(sw - c.len() as i32 * gw - 12, (TOP_BAR - GLYPH_H) / 2, c, pal.bar_text);
        }

        // Dock: one tile per window on this workspace, then how to open more.
        let dy = sh - DOCK_H;
        s.fill(0, dy, sw, DOCK_H, pal.dock);
        s.fill(0, dy, sw, 1, pal.border);
        let mut tx = 12;
        for w in windows.iter().filter(|w| w.workspace == workspace) {
            let focused = focus == Some(w.id);
            // Minimized says so in words as well as in a dimmer colour: a state shown
            // only by colour is lost on anyone who cannot tell the colours apart.
            let mut label: String = if w.minimized { String::from("_ ") } else { String::new() };
            label.extend(w.title.chars().take(18));
            let tw = (label.len() as i32 + 2) * gw;
            s.fill(tx, dy + 8, tw, DOCK_H - 16, if focused { pal.accent } else { pal.tile });
            let color = if focused {
                pal.on_accent
            } else if w.minimized {
                pal.dim_text
            } else {
                pal.tile_text
            };
            s.text(tx + gw, dy + (DOCK_H - GLYPH_H) / 2, &label, color);
            tx += tw + 8;
        }
        let hint = "Super+Enter terminal  Super+E files  Super+A agent  Alt+Tab switch";
        s.text(sw - hint.len() as i32 * gw - 12, dy + (DOCK_H - GLYPH_H) / 2, hint, pal.dim_text);

        self.windows = windows;
        self.screen.present();
        self.frames += 1;
        self.dirty = false;
        self.last_frame = now();
    }

    // ---- the loop ------------------------------------------------------------------

    fn handles_to_wait(&self) -> ([Handle; 32], usize) {
        let mut set = [handle::INVALID; 32];
        let mut n = 0;
        let chans = self
            .operator
            .iter()
            .copied()
            .chain(self.starting.iter().map(|s| s.chan))
            .chain(self.windows.iter().map(|w| w.chan));
        for h in chans {
            if n == set.len() {
                break;
            }
            set[n] = h;
            n += 1;
        }
        (set, n)
    }

    fn run(&mut self) {
        let mut events = [InputEvent::default(); INPUT_READ_MAX];
        while !self.quit {
            loop {
                let n = sys::input_read(self.root, &mut events).unwrap_or(0);
                for e in events.iter().take(n).copied() {
                    self.key(e);
                }
                if n < events.len() {
                    break;
                }
            }
            self.serve_operator();
            self.serve_starting();
            self.serve_windows();
            if let Some(Pending::Describe { until, .. }) = self.pending
                && now() > until
            {
                self.answer_describe(Err(Error::TimedOut));
            }
            if self.dirty && now() >= self.last_frame + FRAME_MS {
                self.compose();
            }
            let (set, n) = self.handles_to_wait();
            let wait = if self.dirty { FRAME_MS.min(POLL_MS) } else { POLL_MS };
            if n == 0 {
                sys::sleep_ms(wait);
            } else {
                sys::wait_any(&set[..n], wait).ok();
            }
        }
        self.shut_down();
    }

    /// Ask every window to close, give them the grace period, stop the rest.
    fn shut_down(&mut self) {
        for w in &mut self.windows {
            sys::send(w.chan, Msg::new(msg::CLOSE).as_bytes(), None).ok();
            w.close_by = Some(now() + desk::CLOSE_GRACE_MS);
        }
        let until = now() + desk::CLOSE_GRACE_MS;
        while !self.windows.is_empty() && now() <= until + 100 {
            self.serve_windows();
            sys::sleep_ms(POLL_MS);
        }
        while !self.windows.is_empty() {
            self.remove(0, "the desktop is closing");
        }
        while let Some(s) = self.starting.pop() {
            self.drop_starting(s, "the desktop is closing");
        }
        println!("[desk] closed after {} frames", self.frames);
    }
}

fn fail(standalone: bool, root: Handle, what: &str) -> i32 {
    println!("[desk] {what}");
    if standalone {
        // pid 1: nothing else would ever shut the machine down.
        let _ = sys::shutdown(root, 1);
    }
    1
}

#[unsafe(no_mangle)]
pub extern "C" fn space_main() -> i32 {
    libspace::heap::set_pages(HEAP_PAGES);
    let boot = handle::BOOTSTRAP;
    let standalone = sys::handle_info(boot).map(|i| i.kind == hkind::ROOT).unwrap_or(false);
    // Operated: the first message is the operator's HELLO, carrying the root
    // capability the desktop runs with.
    let (root, operator) = if standalone {
        (boot, None)
    } else {
        match libspace::desk::recv(boot, 5000) {
            Ok(Some((m, Some(root)))) if m.kind == msg::OP_HELLO && m.value == desk::ABI_VERSION => {
                (root, Some(boot))
            }
            other => {
                println!(
                    "[desk] no operator HELLO with a capability: {:?}",
                    other.map(|o| o.map(|(m, h)| (m.kind, h)))
                );
                return 1;
            }
        }
    };
    let screen = match Screen::open(root) {
        Ok(s) => s,
        Err(e) => {
            if let Some(op) = operator {
                let err = if e.contains("busy") { Error::Busy } else { Error::NotFound };
                sys::send(op, Msg::error(msg::OP_REPLY, err).as_bytes(), None).ok();
            }
            return fail(standalone, root, &format!("cannot take the screen: {e}"));
        }
    };
    println!(
        "[desk] Space OS desktop on a {}x{} screen ({})",
        screen.w,
        screen.h,
        if screen.info.format == fb_format::BGRX { "BGRX" } else { "RGBX" }
    );
    let mut d = Desk {
        root,
        standalone,
        screen,
        windows: Vec::new(),
        starting: Vec::new(),
        focus: None,
        workspace: 0,
        next_id: 0,
        frames: 0,
        dirty: true,
        last_frame: 0,
        operator,
        pending: None,
        contrast: false,
        quit: false,
    };
    if let Some(op) = operator {
        let mut r = Msg::new(msg::OP_REPLY);
        r.w = d.screen.w as u32;
        r.h = d.screen.h as u32;
        sys::send(op, r.as_bytes(), None).ok();
    }
    d.compose();
    if standalone {
        d.launch(app::TERMINAL, false).ok();
        println!("[desk] desktop ready; Super+Enter opens a terminal, Ctrl+Alt+Delete shuts down");
    }
    d.run();
    if standalone {
        let _ = sys::shutdown(root, 0);
    }
    0
}
