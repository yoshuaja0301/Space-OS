//! `deskapps` – the desktop's own apps (U01, ADR-0020): the terminal, the file
//! manager and the Agent Center, each a window of `spacedesk`.
//!
//! One program, three modes: the display server says which in its first message
//! and hands over the one capability that mode needs. The terminal and the Agent
//! Center each run their own `spaceshell` session, so a worker that crashes or
//! wedges belongs to a session, never to the window showing it -- and never to the
//! desktop.
#![no_std]
#![no_main]

extern crate alloc;

use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

use libspace::desk::{Window, recv};
use libspace::gfx::{GLYPH_H, Surface, contrast_x100, glyph_w};
use libspace::shell::Session;
use libspace::spaceabi::desk::{app, msg};
use libspace::spaceabi::handle::rights;
use libspace::spaceabi::input::{InputEvent, key};
use libspace::spaceabi::shell::{Progress, Reply, job, worker_state};
use libspace::spaceabi::syscall::DirEntry;
use libspace::{Handle, handle, kill_reason, println, sys};

const BG: u32 = 0x0E1522;
const FG: u32 = 0xD8E0EC;
const DIM: u32 = 0x8E9BB5;
const ACCENT: u32 = 0x4C8DFF;
const GOOD: u32 = 0x5BD68A;
const BAD: u32 = 0xFF6B6B;
const WARN: u32 = 0xFFD166;
/// The selected row in the window with the keyboard, and in one without it.
const SELECT: u32 = 0x22314D;
const SELECT_IDLE: u32 = 0x182235;
/// The Stop button while there is something to stop, and while there is not.
const STOP: u32 = BAD;
const ON_STOP: u32 = 0x000000;
const STOP_IDLE: u32 = 0x3A2530;
const ON_STOP_IDLE: u32 = 0xFFFFFF;

// Every text colour against everything it is drawn on, at 4.5:1 (WCAG AA) or
// better, checked when the program is compiled. Stop above all: it is the control
// that has to be found when something is going wrong.
const _: () = {
    let text_on_bg = [FG, DIM, ACCENT, GOOD, BAD, WARN];
    let mut i = 0;
    while i < text_on_bg.len() {
        assert!(contrast_x100(text_on_bg[i], BG) >= 450, "app text on the background is too faint to read");
        i += 1;
    }
    let text_on_select = [FG, DIM, WARN];
    let mut i = 0;
    while i < text_on_select.len() {
        assert!(contrast_x100(text_on_select[i], SELECT) >= 450, "text on the selection is too faint");
        assert!(
            contrast_x100(text_on_select[i], SELECT_IDLE) >= 450,
            "text on the idle selection is too faint"
        );
        i += 1;
    }
    assert!(contrast_x100(ON_STOP, STOP) >= 450, "the Stop label is too faint to read");
    assert!(contrast_x100(ON_STOP_IDLE, STOP_IDLE) >= 450, "the idle Stop label is too faint to read");
    assert!(contrast_x100(STOP, BG) >= 300, "the Stop button does not stand out from the window");
    assert!(contrast_x100(ACCENT, SELECT_IDLE) >= 300, "the progress bar does not stand out from its track");
};

const SHELL_QUOTA: u64 = 256;
const PAD: i32 = 10;

/// Start a `spaceshell` session with `cap` (which needs `SPAWN`; the session gets
/// `SPAWN | FS` of it). Returns the session and the shell's process.
fn start_session(cap: Handle) -> Result<(Session, Handle), String> {
    let (mine, theirs) = sys::channel_create().map_err(|e| format!("channel: {e}"))?;
    let shell = sys::spawn(cap, "bin/spaceshell", SHELL_QUOTA, Some(theirs)).map_err(|e| {
        sys::handle_close(mine).ok();
        format!("cannot start the session service: {e}")
    })?;
    // DUP: the session narrows it once more, to read-only files, for the inference
    // worker it starts.
    let root =
        sys::handle_dup(cap, rights::SPAWN | rights::FS | rights::TRANSFER | rights::DUP).map_err(|e| {
            sys::kill(shell).ok();
            format!("narrow the capability: {e}")
        })?;
    match Session::open(mine, root) {
        Ok(s) => Ok((s, shell)),
        Err(e) => {
            sys::kill(shell).ok();
            sys::wait(shell).ok();
            Err(format!("open the session: {e}"))
        }
    }
}

fn end_session(s: &Session, shell: Handle) {
    s.quit().ok();
    // A shell that does not go on its own after QUIT is stopped: closing a window
    // must never hang on what it was showing.
    let until = sys::ticks_ms() + 1000;
    while sys::wait_nonblocking(shell).is_err() && sys::ticks_ms() < until {
        sys::sleep_ms(10);
    }
    sys::kill(shell).ok();
    sys::wait(shell).ok();
    sys::handle_close(shell).ok();
}

fn describe_worker(r: &Reply) -> String {
    let job = r.text();
    match r.state {
        worker_state::IDLE => String::from("no worker has run"),
        worker_state::RUNNING => format!("worker '{job}' running"),
        worker_state::DONE => format!("worker '{job}' finished, exit code {}", r.value as i64),
        worker_state::CRASHED => format!("worker '{job}' crashed ({})", kill_reason::name(r.reason)),
        worker_state::STOPPED => format!("worker '{job}' stopped"),
        _ => String::from("worker state unknown"),
    }
}

/// What every app does with the window's plumbing; the app handles keys and draws.
trait App {
    fn title(&self) -> String;
    fn draw(&self, s: &mut Surface, focused: bool);
    fn key(&mut self, e: InputEvent) -> bool;
    fn describe(&self) -> String;
    /// Periodic work; true when the picture changed.
    fn tick(&mut self) -> bool {
        false
    }
    fn tick_ms(&self) -> u64 {
        250
    }
    fn close(&mut self) {}
}

fn run_app(chan: Handle, mut a: impl App, w: u32, h: u32) -> i32 {
    let mut win = match Window::create(chan, w, h, &a.title(), |s| a.draw(s, false)) {
        Ok(w) => w,
        Err(e) => {
            println!("[deskapps] no window: {e}");
            a.close();
            return 2;
        }
    };
    let redraw = |win: &mut Window, a: &dyn App| {
        let focused = win.focused;
        a.draw(&mut win.buf.surface(), focused);
        win.damage().ok();
    };
    let mut next_tick = sys::ticks_ms() + a.tick_ms();
    loop {
        let wait = next_tick.saturating_sub(sys::ticks_ms()).max(1);
        match win.next(wait) {
            Err(_) => break, // the desktop is gone
            Ok(None) => {}
            Ok(Some(m)) => match m.kind {
                msg::KEY => {
                    if a.key(m.event) {
                        redraw(&mut win, &a);
                    }
                }
                msg::FOCUS => redraw(&mut win, &a),
                msg::CONFIGURE => {
                    let focused = win.focused;
                    if let Err(e) = win.resize(m.w, m.h, |s| a.draw(s, focused)) {
                        println!("[deskapps] {}: cannot resize to {}x{}: {e}", a.title(), m.w, m.h);
                    }
                }
                msg::DESCRIBE => {
                    win.describe(&a.describe()).ok();
                }
                msg::CLOSE => break,
                _ => {}
            },
        }
        if sys::ticks_ms() >= next_tick {
            if a.tick() {
                redraw(&mut win, &a);
            }
            next_tick = sys::ticks_ms() + a.tick_ms();
        }
    }
    a.close();
    0
}

// ---- terminal ------------------------------------------------------------------------

struct Terminal {
    session: Session,
    shell: Handle,
    lines: Vec<String>,
    input: String,
}

impl Terminal {
    fn say(&mut self, line: &str) {
        println!("[deskterm] {line}");
        self.lines.push(line.to_string());
        if self.lines.len() > 200 {
            self.lines.remove(0);
        }
    }

    fn run_line(&mut self, line: &str) {
        self.lines.push(format!("space> {line}"));
        println!("[deskterm] $ {line}");
        let mut words = line.split_whitespace();
        let cmd = words.next().unwrap_or("");
        let arg = words.next();
        match cmd {
            "" => {}
            "help" => {
                self.say("commands: help, status, ls [path], run <ok|crash|hang|slow>, stop, clear, exit")
            }
            "status" => match self.session.status() {
                Ok(r) => {
                    let text =
                        format!("{}; the session has served {} commands", describe_worker(&r), r.served);
                    self.say(&text);
                }
                Err(e) => self.say(&format!("status: {e}")),
            },
            "ls" => {
                let path = arg.unwrap_or("/spaceos");
                match self.session.list(path).and_then(|r| r.result().map(|n| (n, r))) {
                    Ok((n, r)) => {
                        self.say(&format!("{path}: {n} entries"));
                        let names: Vec<String> = r.text().split(' ').map(|s| s.to_string()).collect();
                        let mut row = String::new();
                        for name in names {
                            if row.len() + name.len() + 2 > 90 {
                                self.say(&row);
                                row.clear();
                            }
                            row.push_str(&name);
                            row.push_str("  ");
                        }
                        if !row.is_empty() {
                            self.say(&row);
                        }
                    }
                    Err(e) => self.say(&format!("ls {path}: {e}")),
                }
            }
            "run" => match arg {
                Some(j @ (job::OK | job::CRASH | job::HANG | job::SLOW)) => {
                    match self.session.run(j).and_then(|r| r.result()) {
                        Ok(_) => self.say(&format!("started worker '{j}'")),
                        Err(e) => self.say(&format!("run {j}: {e}")),
                    }
                }
                _ => self.say("run needs a job: ok, crash, hang or slow"),
            },
            "stop" => match self.session.stop().and_then(|r| r.result()) {
                // The answer to STOP says it happened; STATUS says what it ended.
                Ok(_) => match self.session.status() {
                    Ok(r) => self.say(&format!("Stop: {}", describe_worker(&r))),
                    Err(e) => self.say(&format!("Stop: done, but status: {e}")),
                },
                Err(e) => self.say(&format!("stop: {e}")),
            },
            "clear" => self.lines.clear(),
            "exit" => self.say("closing (Alt+F4 closes any window)"),
            other => self.say(&format!("unknown command '{other}'; try 'help'")),
        }
    }
}

impl App for Terminal {
    fn title(&self) -> String {
        String::from("Terminal")
    }

    fn draw(&self, s: &mut Surface, focused: bool) {
        s.fill(0, 0, s.w, s.h, BG);
        let rows = ((s.h - 2 * PAD) / (GLYPH_H + 2)).max(1) as usize;
        let shown = rows.saturating_sub(1);
        let start = self.lines.len().saturating_sub(shown);
        let mut y = PAD;
        for line in &self.lines[start..] {
            let color = if line.starts_with("space> ") { DIM } else { FG };
            s.text(PAD, y, line, color);
            y += GLYPH_H + 2;
        }
        let prompt = format!("space> {}", self.input);
        let w = s.text(PAD, y, &prompt, FG);
        if focused {
            s.fill(PAD + w, y, glyph_w(), GLYPH_H, ACCENT);
        }
    }

    fn key(&mut self, e: InputEvent) -> bool {
        match e.key {
            key::ENTER | key::KP_ENTER => {
                let line = core::mem::take(&mut self.input);
                if line.trim() == "exit" {
                    return false;
                }
                self.run_line(line.trim());
                true
            }
            key::BACKSPACE => self.input.pop().is_some(),
            _ => match char::from_u32(e.ch) {
                Some(c) if !c.is_control() && self.input.len() < 80 => {
                    self.input.push(c);
                    true
                }
                _ => false,
            },
        }
    }

    fn describe(&self) -> String {
        let tail: Vec<&str> = self.lines.iter().rev().take(2).map(|s| s.as_str()).collect();
        let mut t = String::from("terminal: ");
        for (i, l) in tail.iter().rev().enumerate() {
            if i > 0 {
                t.push_str(" | ");
            }
            t.push_str(l);
        }
        t
    }

    fn close(&mut self) {
        end_session(&self.session, self.shell);
    }
}

// ---- file manager ----------------------------------------------------------------

struct Files {
    root: Handle,
    path: String,
    entries: Vec<DirEntry>,
    selected: usize,
    error: Option<String>,
}

impl Files {
    fn load(&mut self) {
        let mut buf = [DirEntry::default(); 64];
        match sys::fs_list(self.root, &self.path, &mut buf) {
            Ok(n) => {
                // The volume's own "." and ".." are not something to open here:
                // Backspace goes up.
                self.entries =
                    buf[..n].iter().filter(|e| e.name() != "." && e.name() != "..").copied().collect();
                self.error = None;
                println!("[deskfiles] {}: {n} entries", self.path);
            }
            Err(e) => {
                self.entries.clear();
                self.error = Some(format!("{e}"));
                println!("[deskfiles] {}: {e}", self.path);
            }
        }
        self.selected = 0;
    }

    fn open(&mut self) {
        let Some(e) = self.entries.get(self.selected) else { return };
        if e.is_dir == 0 {
            return;
        }
        let name = e.name().to_string();
        if self.path.ends_with('/') {
            self.path.push_str(&name);
        } else {
            self.path = format!("{}/{name}", self.path);
        }
        self.load();
    }

    fn up(&mut self) {
        match self.path.rfind('/') {
            Some(0) | None => self.path = String::from("/"),
            Some(i) => self.path.truncate(i),
        }
        self.load();
    }
}

impl App for Files {
    fn title(&self) -> String {
        String::from("Files")
    }

    fn draw(&self, s: &mut Surface, focused: bool) {
        s.fill(0, 0, s.w, s.h, BG);
        s.text(PAD, PAD, &self.path, ACCENT);
        s.text(PAD, PAD + GLYPH_H + 2, "Up/Down select, Enter open, Backspace up", DIM);
        let mut y = PAD + 2 * (GLYPH_H + 2) + 6;
        if let Some(e) = &self.error {
            s.text(PAD, y, &format!("cannot list: {e}"), BAD);
        }
        let gw = glyph_w();
        for (i, e) in self.entries.iter().enumerate() {
            if y + GLYPH_H > s.h - PAD {
                break;
            }
            if i == self.selected {
                s.fill(
                    PAD - 4,
                    y - 1,
                    s.w - 2 * PAD + 8,
                    GLYPH_H + 2,
                    if focused { SELECT } else { SELECT_IDLE },
                );
            }
            if e.is_dir != 0 {
                s.text(PAD, y, &format!("{}/", e.name()), WARN);
                s.text(s.w - PAD - 6 * gw, y, "folder", DIM);
            } else {
                s.text(PAD, y, e.name(), FG);
                let size = format!("{} B", e.size);
                s.text(s.w - PAD - size.len() as i32 * gw, y, &size, DIM);
            }
            y += GLYPH_H + 2;
        }
    }

    fn key(&mut self, e: InputEvent) -> bool {
        match e.key {
            key::UP if self.selected > 0 => self.selected -= 1,
            key::DOWN if self.selected + 1 < self.entries.len() => self.selected += 1,
            key::HOME => self.selected = 0,
            key::END => self.selected = self.entries.len().saturating_sub(1),
            key::ENTER | key::KP_ENTER => self.open(),
            key::BACKSPACE => self.up(),
            _ => return false,
        }
        true
    }

    fn describe(&self) -> String {
        match self.entries.get(self.selected) {
            Some(e) => format!(
                "files: {}, {} entries, selected {}{} ({} bytes)",
                self.path,
                self.entries.len(),
                e.name(),
                if e.is_dir != 0 { "/" } else { "" },
                e.size
            ),
            None => format!("files: {}, empty", self.path),
        }
    }
}

// ---- Agent Center --------------------------------------------------------------------

struct Agent {
    session: Session,
    shell: Handle,
    status: Reply,
    progress: Progress,
    last: String,
    note: String,
    /// The first token of this run has been announced in the log.
    first_told: bool,
}

/// Height of one row of the Agent Center, and the width of its labels in characters.
const ROW_H: i32 = GLYPH_H + 4;
const LABEL_CHARS: i32 = 9;

/// One labelled row of the Agent Center at `y`, which moves to the next row.
fn row(s: &mut Surface, y: &mut i32, label: &str, text: &str, color: u32) {
    s.text(PAD, *y, label, DIM);
    s.text(PAD + LABEL_CHARS * glyph_w(), *y, text, color);
    *y += ROW_H;
}

/// What a job is, what it may touch and what it costs, as the Agent Center shows it
/// (PRD §6: plan, permissions, resources, cloud cost, preview of changes).
struct JobFacts {
    task: &'static str,
    plan: &'static str,
    access: &'static str,
    cost: &'static str,
    changes: &'static str,
}

fn job_facts(job: &str) -> JobFacts {
    match job {
        job::INFER => JobFacts {
            task: "generate text with the local model",
            plan: "verify the model's sha256, write 128 tokens, compare with the baseline",
            access: "reads files; one compute connection; no network, no writing, no spawning",
            cost: "none: the model runs here, nothing goes to a cloud",
            changes: "none: the job only reads",
        },
        "" | "-" => JobFacts {
            task: "no job yet",
            plan: "5 runs the model; 1-4 run test workers that finish, crash, hang or sleep",
            access: "-",
            cost: "-",
            changes: "-",
        },
        _ => JobFacts {
            task: "a test worker",
            plan: "put the worker into one state an inference job can reach",
            access: "nothing: no files, no network, no spawning",
            cost: "none",
            changes: "none",
        },
    }
}

impl Agent {
    fn job(&self) -> &str {
        self.status.text()
    }

    fn is_infer(&self) -> bool {
        self.job() == job::INFER
    }

    /// One line for the log and for automation: what the worker is doing, without a
    /// count that changes with every token (the window and [`App::describe`] have those).
    fn state_line(&self) -> String {
        let r = &self.status;
        let p = &self.progress;
        if self.is_infer() {
            match r.state {
                worker_state::DONE if p.done > 0 => {
                    return format!(
                        "worker 'infer' finished: {} tokens, {} matching the baseline",
                        p.done, p.matched
                    );
                }
                worker_state::STOPPED if p.cooperative != 0 => {
                    return format!(
                        "worker 'infer' stopped between two steps after {} of {} tokens (Stop took {} ms)",
                        p.done, p.total, p.stop_ms
                    );
                }
                worker_state::STOPPED => {
                    return format!("worker 'infer' was killed by Stop after {} ms", p.stop_ms);
                }
                _ => {}
            }
        }
        describe_worker(r)
    }

    /// The state row: short, because the progress row beside it has the counts.
    fn state_short(&self) -> String {
        let r = &self.status;
        let p = &self.progress;
        match r.state {
            worker_state::IDLE => String::from("no worker has run"),
            worker_state::RUNNING => String::from("running"),
            worker_state::DONE if self.is_infer() => {
                format!("finished, {} of {} match the baseline", p.matched, p.done)
            }
            worker_state::DONE => format!("finished, exit code {}", r.value as i64),
            worker_state::CRASHED => format!("crashed ({})", kill_reason::name(r.reason)),
            worker_state::STOPPED if self.is_infer() && p.cooperative != 0 => {
                format!("stopped between two steps, {} ms after Stop", p.stop_ms)
            }
            worker_state::STOPPED if self.is_infer() => format!("killed by Stop after {} ms", p.stop_ms),
            worker_state::STOPPED => String::from("stopped"),
            _ => String::from("unknown"),
        }
    }

    fn refresh(&mut self) -> bool {
        let mut changed = false;
        match self.session.status() {
            Ok(r) => {
                changed |= r.served != self.status.served || r.state != self.status.state;
                self.status = r;
            }
            Err(e) => {
                self.note = format!("session: {e}");
                return true;
            }
        }
        if let Ok(p) = self.session.progress() {
            changed |= p.done != self.progress.done || p.stop_ms != self.progress.stop_ms;
            self.progress = p;
        }
        if self.is_infer() && self.progress.done > 0 && !self.first_told {
            self.first_told = true;
            println!("[deskagent] worker 'infer' wrote its first token after {} ms", self.progress.ttft_ms);
        }
        let text = self.state_line();
        if text != self.last {
            println!("[deskagent] {text}");
            self.last = text;
            changed = true;
        }
        changed
    }

    fn start(&mut self, j: &str) {
        self.note = match self.session.run(j).and_then(|r| r.result()) {
            Ok(_) => format!("started '{j}'"),
            Err(e) => format!("cannot start '{j}': {e}"),
        };
        self.first_told = false;
        println!("[deskagent] {}", self.note);
        self.refresh();
    }

    fn stop(&mut self) {
        let t0 = sys::ticks_ms();
        let stopped = self.session.stop().and_then(|r| r.result());
        let ms = sys::ticks_ms() - t0;
        // The answer to STOP says it happened; STATUS says what it ended.
        self.refresh();
        self.note = match stopped {
            Ok(_) => {
                println!("[deskagent] Stop: {} ({ms} ms)", self.last);
                format!("Stop answered in {ms} ms")
            }
            Err(e) => {
                println!("[deskagent] Stop: {e}");
                format!("Stop: {e}")
            }
        };
    }
}

impl App for Agent {
    fn title(&self) -> String {
        String::from("Agent Center")
    }

    fn draw(&self, s: &mut Surface, _focused: bool) {
        s.fill(0, 0, s.w, s.h, BG);
        let r = &self.status;
        let p = &self.progress;
        let color = match r.state {
            worker_state::RUNNING => GOOD,
            worker_state::CRASHED => BAD,
            worker_state::STOPPED => WARN,
            _ => FG,
        };
        let gw = glyph_w();
        let line = ROW_H;
        let label_w = LABEL_CHARS * gw;
        let facts = job_facts(self.job());
        let mut y = PAD;
        s.text(PAD, y, "Inference worker", ACCENT);
        y += line + 4;
        row(s, &mut y, "task", facts.task, FG);
        row(s, &mut y, "plan", facts.plan, DIM);
        row(s, &mut y, "state", &self.state_short(), color);
        // Progress: a bar, and the numbers beside it.
        if self.is_infer() && p.total > 0 {
            let bar_w = 16 * gw;
            let done_w = (bar_w as u64 * p.done.min(p.total) as u64 / p.total as u64) as i32;
            s.fill(PAD + label_w, y + 2, bar_w, GLYPH_H - 4, SELECT_IDLE);
            s.fill(PAD + label_w, y + 2, done_w, GLYPH_H - 4, ACCENT);
            s.text(PAD, y, "progress", DIM);
            s.text(PAD + label_w + bar_w + gw, y, &format!("{}/{} tokens", p.done, p.total), FG);
            y += line;
            let gen_ms = p.elapsed_ms.saturating_sub(p.ttft_ms) as u64;
            let per_1000s =
                if p.done > 1 && gen_ms > 0 { (p.done as u64 - 1) * 1_000_000 / gen_ms } else { 0 };
            row(
                s,
                &mut y,
                "speed",
                &format!(
                    "first token {} ms, {}.{} tokens/s",
                    p.ttft_ms,
                    per_1000s / 1000,
                    per_1000s % 1000 / 100
                ),
                FG,
            );
            row(
                s,
                &mut y,
                "memory",
                &format!("worker {} KiB of {} KiB", p.used_pages * 4, p.quota_pages * 4),
                FG,
            );
        } else {
            y += 3 * line;
        }
        row(s, &mut y, "access", facts.access, FG);
        row(s, &mut y, "cost", facts.cost, FG);
        row(s, &mut y, "changes", facts.changes, FG);
        if self.is_infer() && p.text_len > 0 {
            row(s, &mut y, "output", &format!("\"{}\"", p.text()), FG);
        } else {
            y += line;
        }
        row(s, &mut y, "session", &format!("alive, {} commands served; {}", r.served, self.note), DIM);
        // The controls.
        let by = s.h - PAD - GLYPH_H - 4;
        let mut x = PAD;
        for label in ["5 infer", "1 ok", "2 crash", "3 hang", "4 slow"] {
            let w = (label.len() as i32 + 2) * gw;
            s.frame(x, by - 4, w, GLYPH_H + 8, 1, 0x2A3A58);
            s.text(x + gw, by, label, FG);
            x += w + 8;
        }
        let stop = "S  Stop";
        let w = (stop.len() as i32 + 2) * gw;
        let running = r.state == worker_state::RUNNING;
        s.fill(x, by - 4, w, GLYPH_H + 8, if running { STOP } else { STOP_IDLE });
        s.text(x + gw, by, stop, if running { ON_STOP } else { ON_STOP_IDLE });
    }

    fn key(&mut self, e: InputEvent) -> bool {
        match char::from_u32(e.ch) {
            Some('5') => self.start(job::INFER),
            Some('1') => self.start(job::OK),
            Some('2') => self.start(job::CRASH),
            Some('3') => self.start(job::HANG),
            Some('4') => self.start(job::SLOW),
            Some('s') | Some('S') => self.stop(),
            _ if e.key == key::ENTER => self.stop(),
            _ => return false,
        }
        true
    }

    fn describe(&self) -> String {
        // Short enough for one message (180 bytes): the speed is on the screen.
        let p = &self.progress;
        let tokens = if self.is_infer() {
            format!("; {}/{} tokens, {} matching", p.done, p.total, p.matched)
        } else {
            String::new()
        };
        format!(
            "agent: {}{tokens}; Stop {}; {} commands served",
            self.last,
            if self.status.state == worker_state::RUNNING { "ready" } else { "has nothing to stop" },
            self.status.served
        )
    }

    fn tick(&mut self) -> bool {
        self.refresh()
    }

    fn tick_ms(&self) -> u64 {
        // While a worker runs, the progress is worth watching closely.
        if self.status.state == worker_state::RUNNING { 100 } else { 250 }
    }

    fn close(&mut self) {
        // A worker must not outlive the window that started it.
        if self.status.state == worker_state::RUNNING {
            self.session.stop().ok();
        }
        end_session(&self.session, self.shell);
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn space_main() -> i32 {
    let chan = handle::BOOTSTRAP;
    let (hello, cap) = match recv(chan, 5000) {
        Ok(Some((m, Some(cap)))) if m.kind == msg::HELLO => (m, cap),
        _ => {
            println!("[deskapps] no HELLO from the desktop");
            return 1;
        }
    };
    match hello.value {
        app::TERMINAL => match start_session(cap) {
            Ok((session, shell)) => {
                let mut t = Terminal { session, shell, lines: Vec::new(), input: String::new() };
                t.say("Space OS terminal; type 'help'");
                run_app(chan, t, 720, 400)
            }
            Err(e) => {
                println!("[deskterm] {e}");
                1
            }
        },
        app::FILES => {
            let mut f = Files {
                root: cap,
                path: String::from("/spaceos"),
                entries: Vec::new(),
                selected: 0,
                error: None,
            };
            f.load();
            run_app(chan, f, 460, 360)
        }
        app::AGENT => match start_session(cap) {
            Ok((session, shell)) => {
                let mut a = Agent {
                    session,
                    shell,
                    status: Reply::default(),
                    progress: Progress::default(),
                    last: String::new(),
                    note: String::from("5 runs the model, 1-4 test workers, S stops"),
                    first_told: false,
                };
                a.refresh();
                run_app(chan, a, 640, 330)
            }
            Err(e) => {
                println!("[deskagent] {e}");
                1
            }
        },
        other => {
            println!("[deskapps] unknown app {other}");
            1
        }
    }
}
