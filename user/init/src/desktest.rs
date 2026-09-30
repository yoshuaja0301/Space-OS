//! The desktop, driven as its operator (U01, ADR-0020): `bin/spacedesk` started
//! with a channel instead of the root capability, and every key it gets sent from
//! here -- through the same handler the keyboard's keys take.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use libspace::desk::recv;
use libspace::spaceabi::desk::{self, Msg, msg, window_flags};
use libspace::spaceabi::error::Error;
use libspace::spaceabi::handle::rights;
use libspace::spaceabi::input::{InputEvent, flags, key, kind};
use libspace::{ExitStatus, Handle, println, sha256, sys};

use crate::ROOT;

/// Pages for the display server: a back buffer the size of the screen, heap, code.
const DESK_QUOTA: u64 = 1400;
/// Longest an operator request may take; launching an app includes starting it.
const CALL_MS: u64 = 10_000;

pub struct Desk {
    pub process: Handle,
    op: Handle,
    pub w: u32,
    pub h: u32,
}

/// One window as the desktop reports it.
#[derive(Clone)]
pub struct Win {
    pub id: u32,
    pub x: i32,
    pub y: i32,
    pub w: u32,
    pub h: u32,
    pub flags: u32,
}

pub struct State {
    pub focus: u32,
    pub workspace: i32,
    pub frames: u64,
    pub windows: Vec<Win>,
}

impl State {
    pub fn win(&self, id: u32) -> Result<&Win, String> {
        self.windows.iter().find(|w| w.id == id).ok_or_else(|| format!("window {id} is gone"))
    }
}

/// A key press as the keyboard would deliver it.
pub fn press(code: u16, mods: u8) -> InputEvent {
    InputEvent { kind: kind::KEY, flags: flags::PRESSED | mods, key: code, ch: 0, time_ms: sys::ticks_ms() }
}

/// A key that types `c` (text needs no key code: the apps read the character).
pub fn typed(c: char) -> InputEvent {
    let code = if c == '\n' { key::ENTER } else { 0 };
    InputEvent { kind: kind::KEY, flags: flags::PRESSED, key: code, ch: c as u32, time_ms: sys::ticks_ms() }
}

impl Desk {
    /// Start the desktop and hand it what a desktop needs: the screen, the keys,
    /// and starting apps that list files. Nothing else -- no shutdown, no network.
    pub fn start() -> Result<Desk, String> {
        let (op, theirs) = sys::channel_create().map_err(|e| format!("channel: {e}"))?;
        let process = sys::spawn(ROOT, "bin/spacedesk", DESK_QUOTA, Some(theirs)).map_err(|e| {
            sys::handle_close(op).ok();
            format!("spawn spacedesk: {e}")
        })?;
        let d = Desk { process, op, w: 0, h: 0 };
        let cap = match sys::handle_dup(
            ROOT,
            rights::DISPLAY | rights::CONSOLE | rights::SPAWN | rights::FS | rights::TRANSFER | rights::DUP,
        ) {
            Ok(c) => c,
            Err(e) => {
                d.kill();
                return Err(format!("narrow the root: {e}"));
            }
        };
        let mut hello = Msg::new(msg::OP_HELLO);
        hello.value = desk::ABI_VERSION;
        if let Err(e) = sys::send(op, hello.as_bytes(), Some(cap)) {
            sys::handle_close(cap).ok();
            d.kill();
            return Err(format!("hello: {e}"));
        }
        match d.reply() {
            Ok(r) => Ok(Desk { w: r.w, h: r.h, ..d }),
            Err(e) => {
                d.kill();
                Err(format!("hello: {e}"))
            }
        }
    }

    fn reply(&self) -> Result<Msg, String> {
        match recv(self.op, CALL_MS) {
            Ok(Some((m, h))) => {
                if let Some(h) = h {
                    sys::handle_close(h).ok();
                }
                m.result().map_err(|e| format!("{e}"))?;
                Ok(m)
            }
            Ok(None) => Err(String::from("no answer")),
            Err(e) => Err(format!("{e}")),
        }
    }

    fn call(&self, m: &Msg) -> Result<Msg, String> {
        sys::send(self.op, m.as_bytes(), None).map_err(|e| format!("send: {e}"))?;
        self.reply()
    }

    pub fn launch(&self, app: &str) -> Result<u32, String> {
        self.call(&Msg::with_text(msg::OP_LAUNCH, app))
            .map(|r| r.id)
            .map_err(|e| format!("launch {app}: {e}"))
    }

    pub fn key(&self, e: InputEvent) -> Result<(), String> {
        let mut m = Msg::new(msg::OP_KEY);
        m.event = e;
        self.call(&m).map(|_| ())
    }

    pub fn keys(&self, text: &str) -> Result<(), String> {
        for c in text.chars() {
            self.key(typed(c))?;
        }
        Ok(())
    }

    pub fn state(&self) -> Result<State, String> {
        let r = self.call(&Msg::new(msg::OP_STATE))?;
        let mut windows = Vec::new();
        for _ in 0..r.value {
            let w = match recv(self.op, CALL_MS) {
                Ok(Some((m, _))) if m.kind == msg::OP_WINDOW => m,
                other => {
                    return Err(format!(
                        "expected a window, got {:?}",
                        other.map(|o| o.map(|(m, _)| m.kind))
                    ));
                }
            };
            windows.push(Win { id: w.id, x: w.x, y: w.y, w: w.w, h: w.h, flags: w.value });
        }
        Ok(State { focus: r.id, workspace: r.x, frames: r.value2, windows })
    }

    pub fn describe(&self, id: u32) -> Result<String, String> {
        let mut m = Msg::new(msg::OP_DESCRIBE);
        m.id = id;
        self.call(&m).map(|r| String::from(r.text()))
    }

    /// Ask window `id` to describe itself until it says `want`, for up to `ms`.
    pub fn await_text(&self, id: u32, want: &str, ms: u64) -> Result<String, String> {
        let deadline = sys::ticks_ms() + ms;
        loop {
            let t = self.describe(id)?;
            if t.contains(want) {
                return Ok(t);
            }
            if sys::ticks_ms() > deadline {
                return Err(format!("window {id} never said {want:?}; last: {t:?}"));
            }
            sys::sleep_ms(50);
        }
    }

    /// Give focus to window `id` with Alt+Tab, the way a person would.
    pub fn focus(&self, id: u32) -> Result<(), String> {
        for _ in 0..8 {
            if self.state()?.focus == id {
                return Ok(());
            }
            self.key(press(key::TAB, flags::ALT))?;
        }
        Err(format!("Alt+Tab never reached window {id}"))
    }

    pub fn quit(self) -> Result<ExitStatus, String> {
        let asked = self.call(&Msg::new(msg::OP_QUIT));
        let st = sys::wait(self.process).map_err(|e| format!("wait: {e}"));
        sys::handle_close(self.process).ok();
        sys::handle_close(self.op).ok();
        asked?;
        st
    }

    pub fn kill(&self) {
        sys::kill(self.process).ok();
        sys::wait(self.process).ok();
        sys::handle_close(self.process).ok();
        sys::handle_close(self.op).ok();
    }
}

/// The screen can be taken, and taken by one server at a time.
pub fn lease() -> Result<(), String> {
    let d = Desk::start()?;
    let checks = (|| {
        if (d.w, d.h) == (0, 0) {
            return Err(String::from("the desktop reports a 0x0 screen"));
        }
        match sys::display_open(ROOT) {
            Err(Error::Busy) => {}
            Ok(h) => {
                sys::handle_close(h).ok();
                return Err(String::from("a second lease on the screen was granted"));
            }
            Err(e) => return Err(format!("a second lease: {e}, expected Busy")),
        }
        let no_display =
            sys::handle_dup(ROOT, rights::ROOT_ALL & !rights::DISPLAY).map_err(|e| format!("dup: {e}"))?;
        let denied = sys::display_open(no_display);
        sys::handle_close(no_display).ok();
        if denied != Err(Error::Denied) {
            return Err(format!("a root without DISPLAY: {denied:?}"));
        }
        println!("[init] desktop: {}x{} screen leased to the desktop; a second lease is refused", d.w, d.h);
        Ok(())
    })();
    let st = d.quit();
    checks?;
    match st {
        Ok(st) if st.is_exited_with(0) => {}
        other => return Err(format!("the desktop ended with {other:?}")),
    }
    // Back with the console: the screen can be leased again.
    let again = sys::display_open(ROOT).map_err(|e| format!("lease after the desktop quit: {e}"))?;
    sys::handle_close(again).ok();
    Ok(())
}

/// Everything a person does with windows, from the keyboard.
pub fn windows() -> Result<(), String> {
    let d = Desk::start()?;
    let r = (|| {
        let term = d.launch("terminal")?;
        let files = d.launch("files")?;
        let s = d.state()?;
        if s.windows.len() != 2 || s.focus != files {
            return Err(format!("{} windows, focus {} after opening two", s.windows.len(), s.focus));
        }
        // Alt+Tab moves the keyboard to the other window.
        d.key(press(key::TAB, flags::ALT))?;
        if d.state()?.focus != term {
            return Err(String::from("Alt+Tab did not give the terminal the keyboard"));
        }
        // Alt+Right moves it one step; Alt+Shift+Down makes it one step taller, and
        // the app answers with a buffer of the new size.
        let before = d.state()?.win(term)?.clone();
        d.key(press(key::RIGHT, flags::ALT))?;
        d.key(press(key::DOWN, flags::ALT | flags::SHIFT))?;
        let after = d.state()?.win(term)?.clone();
        if after.x != before.x + 32 || after.h != before.h + 32 {
            return Err(format!(
                "after a move and a resize: {},{} {}x{} (was {},{} {}x{})",
                after.x, after.y, after.w, after.h, before.x, before.y, before.w, before.h
            ));
        }
        // Minimize: the other window gets the keyboard.
        d.key(press(key::M, flags::SUPER))?;
        let s = d.state()?;
        if s.win(term)?.flags & window_flags::MINIMIZED == 0 || s.focus != files {
            return Err(String::from("Super+M did not minimize the terminal and focus the files"));
        }
        // Another workspace is empty; coming back finds the windows where they were.
        d.key(press(key::RIGHT, flags::CTRL | flags::ALT))?;
        let s = d.state()?;
        if s.workspace != 1 || s.focus != 0 {
            return Err(format!("workspace {} focus {} after Ctrl+Alt+Right", s.workspace, s.focus));
        }
        d.key(press(key::LEFT, flags::CTRL | flags::ALT))?;
        if d.state()?.workspace != 0 {
            return Err(String::from("Ctrl+Alt+Left did not come back"));
        }
        // Alt+Tab brings the minimized terminal back.
        d.focus(term)?;
        if d.state()?.win(term)?.flags & window_flags::MINIMIZED != 0 {
            return Err(String::from("the terminal came back minimized"));
        }
        // Alt+F4 closes the file manager: its app goes, and so does the window.
        d.focus(files)?;
        d.key(press(key::F4, flags::ALT))?;
        let deadline = sys::ticks_ms() + 3000;
        while d.state()?.windows.iter().any(|w| w.id == files) {
            if sys::ticks_ms() > deadline {
                return Err(String::from("Alt+F4 did not close the file manager"));
            }
            sys::sleep_ms(50);
        }
        let s = d.state()?;
        println!(
            "[init] desktop: focus, move, resize, minimize, workspaces and close all worked; {} frames presented",
            s.frames
        );
        Ok(())
    })();
    let st = d.quit();
    r?;
    match st {
        Ok(st) if st.is_exited_with(0) => Ok(()),
        other => Err(format!("the desktop ended with {other:?}")),
    }
}

/// U01 itself: the inference worker crashes, another wedges and is stopped, and
/// the desktop, the terminal and the file manager answer throughout.
pub fn worker_crash() -> Result<(), String> {
    let d = Desk::start()?;
    let r = (|| {
        let term = d.launch("terminal")?;
        let files = d.launch("files")?;
        let agent = d.launch("agent")?;
        let frames0 = d.state()?.frames;
        // The worker crashes.
        d.focus(agent)?;
        d.keys("2")?;
        let t = d.await_text(agent, "crashed (page fault)", 5000)?;
        println!("[init] desktop: {t}");
        // The terminal answers a command typed after the crash.
        d.focus(term)?;
        d.keys("status\n")?;
        let t = d.await_text(term, "the session has served", 5000)?;
        println!("[init] desktop: {t}");
        // The file manager still moves through the volume.
        d.focus(files)?;
        d.key(press(key::DOWN, 0))?;
        let t = d.await_text(files, "selected", 3000)?;
        println!("[init] desktop: {t}");
        // A worker that never returns, and Stop.
        d.focus(agent)?;
        d.keys("3")?;
        d.await_text(agent, "'hang' running", 5000)?;
        let t0 = sys::ticks_ms();
        d.keys("s")?;
        let t = d.await_text(agent, "'hang' stopped", 3000)?;
        let stop_ms = sys::ticks_ms() - t0;
        println!("[init] desktop: {t} ({stop_ms} ms after Stop was pressed)");
        // PRD §9: a worker stops within 2 seconds on the CPU backend.
        if stop_ms > 2000 {
            return Err(format!("Stop took {stop_ms} ms"));
        }
        let s = d.state()?;
        if s.frames <= frames0 || s.windows.len() != 3 {
            return Err(format!("{} windows, frames {} -> {}", s.windows.len(), frames0, s.frames));
        }
        println!(
            "[init] desktop: {} frames presented while the workers crashed and hung",
            s.frames - frames0
        );
        Ok(())
    })();
    let st = d.quit();
    r?;
    match st {
        Ok(st) if st.is_exited_with(0) => Ok(()),
        other => Err(format!("the desktop ended with {other:?}")),
    }
}

/// `done`, `total` and `matched` out of an Agent Center description
/// ("... 57/128 tokens, 57 matching the baseline ...").
fn tokens(t: &str) -> Option<(u32, u32, u32)> {
    let (head, rest) = t.split_once(" tokens, ")?;
    let (done, total) = head.rsplit(' ').next()?.split_once('/')?;
    let matched = rest.split(' ').next()?;
    Some((done.parse().ok()?, total.parse().ok()?, matched.parse().ok()?))
}

/// PRD §9, the first end-to-end scenario, in the desktop: offline, the model on the
/// guest disk writes text in the Agent Center; Stop ends the worker between two
/// compute steps; the terminal still answers.
pub fn inference() -> Result<(), String> {
    let d = Desk::start()?;
    let r = (|| {
        let term = d.launch("terminal")?;
        let agent = d.launch("agent")?;
        d.focus(agent)?;
        d.keys("5")?;
        let deadline = sys::ticks_ms() + 60_000;
        let t = loop {
            let t = d.describe(agent)?;
            if tokens(&t).is_some_and(|(done, _, _)| done >= 8) {
                break t;
            }
            if !t.contains("running") && tokens(&t).is_some() {
                return Err(format!("the worker ended before writing 8 tokens: {t:?}"));
            }
            if sys::ticks_ms() > deadline {
                return Err(format!("no 8 tokens after 60 s: {t:?}"));
            }
            sys::sleep_ms(50);
        };
        println!("[init] desktop: {t}");
        let t0 = sys::ticks_ms();
        d.keys("s")?;
        let t = d.await_text(agent, "stopped between two steps", 5000)?;
        let stop_ms = sys::ticks_ms() - t0;
        println!("[init] desktop: {t} ({stop_ms} ms after Stop was pressed)");
        let (done, total, matched) = tokens(&t).ok_or_else(|| format!("no token count in {t:?}"))?;
        if done == 0 || done >= total || matched != done {
            return Err(format!("after Stop: {done}/{total} tokens, {matched} matching the baseline"));
        }
        // PRD §9: a worker stops within 2 seconds on the CPU backend.
        if stop_ms > 2000 {
            return Err(format!("Stop took {stop_ms} ms"));
        }
        // The terminal answers a command typed after the worker was stopped.
        d.focus(term)?;
        d.keys("status\n")?;
        let t = d.await_text(term, "the session has served", 5000)?;
        println!("[init] desktop: {t}");
        Ok(())
    })();
    let st = d.quit();
    r?;
    match st {
        Ok(st) if st.is_exited_with(0) => Ok(()),
        other => Err(format!("the desktop ended with {other:?}")),
    }
}

/// The result the Command Center has selected, as it describes it: path, offset,
/// length and the first four bytes of the digest, in hex.
fn selected_result(t: &str) -> Option<(String, u64, usize, String)> {
    let rest = t.split_once("; selected ")?.1;
    let mut w = rest.split_whitespace();
    let _index = w.next()?;
    let path = w.next()?;
    if w.next()? != "bytes" {
        return None;
    }
    let (offset, len) = w.next()?.split_once('+')?;
    if w.next()? != "sha" {
        return None;
    }
    let sha = w.next()?.trim_end_matches(';');
    Some((String::from(path), offset.parse().ok()?, len.parse().ok()?, String::from(sha)))
}

/// The Command Center (PRD §6): SpaceLink search in the desktop, where every result
/// came from -- checked here the way any caller can check SpaceLink, by reading the
/// bytes back -- a context bundle for the query, and "show in Files".
pub fn command() -> Result<(), String> {
    let d = Desk::start()?;
    let r = (|| {
        let cmd = d.launch("command")?;
        d.focus(cmd)?;
        d.keys("channel\n")?;
        let t = d.await_text(cmd, "results for 'channel'; selected 1 ", 5000)?;
        println!("[init] desktop: {t}");
        let (path, offset, len, sha) =
            selected_result(&t).ok_or_else(|| format!("no selected result in {t:?}"))?;
        // IPC.TXT says "channel" three times; no other document comes close.
        if path != "/spaceos/docs/IPC.TXT" {
            return Err(format!("the best result for 'channel' is {path}"));
        }
        if len == 0 || len > 1024 {
            return Err(format!("a chunk of {len} bytes"));
        }
        let f = sys::fs_open(ROOT, &path).map_err(|e| format!("open {path}: {e}"))?;
        let mut buf = alloc::vec![0u8; len];
        let n = sys::fs_read(f, offset, &mut buf).map_err(|e| format!("read {path}: {e}"));
        sys::handle_close(f).ok();
        if n? != len {
            return Err(format!("{path} has fewer than {len} bytes at {offset}"));
        }
        let d8 = sha256::digest(&buf);
        let on_disk = format!("{:02x}{:02x}{:02x}{:02x}", d8[0], d8[1], d8[2], d8[3]);
        if on_disk != sha {
            return Err(format!(
                "the Command Center shows sha {sha} for {path} {offset}+{len}; the disk says {on_disk}"
            ));
        }
        // A context bundle for the same query.
        d.key(press(key::B, flags::CTRL))?;
        let t = d.await_text(cmd, "; bundle ", 5000)?;
        println!("[init] desktop: {}", t.split_once("; bundle ").map(|(_, b)| b).unwrap_or(&t));
        // "Show in Files": the desktop opens a file manager there, with the file selected.
        let before: Vec<u32> = d.state()?.windows.iter().map(|w| w.id).collect();
        d.key(press(key::O, flags::CTRL))?;
        let deadline = sys::ticks_ms() + 5000;
        let files = loop {
            if let Some(w) = d.state()?.windows.iter().find(|w| !before.contains(&w.id)) {
                break w.id;
            }
            if sys::ticks_ms() > deadline {
                return Err(String::from("Ctrl+O opened no file manager"));
            }
            sys::sleep_ms(50);
        };
        let t = d.await_text(files, "selected IPC.TXT", 5000)?;
        println!("[init] desktop: {t}");
        Ok(())
    })();
    let st = d.quit();
    r?;
    match st {
        Ok(st) if st.is_exited_with(0) => Ok(()),
        other => Err(format!("the desktop ended with {other:?}")),
    }
}

/// A desktop that is killed takes nothing with it: the console gets the screen
/// back, and the next desktop can have it.
pub fn killed() -> Result<(), String> {
    let d = Desk::start()?;
    d.launch("terminal")?;
    d.kill();
    // Its apps see the desktop go and leave on their own; the screen comes back as
    // soon as the last mapping of it is gone, which is at the desktop's exit.
    let again = sys::display_open(ROOT).map_err(|e| format!("lease after the desktop was killed: {e}"))?;
    sys::handle_close(again).ok();
    let d = Desk::start()?;
    let t = d.launch("terminal");
    let st = d.quit();
    t?;
    match st {
        Ok(st) if st.is_exited_with(0) => {}
        other => return Err(format!("the second desktop ended with {other:?}")),
    }
    println!("[init] desktop: killed with a window open, the screen came back, and a new desktop started");
    Ok(())
}
