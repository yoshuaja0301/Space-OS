//! `spaceshell` – the supervising session service (requirement U01).
//!
//! The PRD asks that the desktop, the terminal, the file manager and the Stop
//! control keep working when the inference worker crashes. This is the process that
//! has to make that true: it owns the console session, the file listing and the
//! lifetime of the worker, and the worker runs in its own address space with its own
//! quota.
//!
//! The property is won by the loop, not by luck. The shell has exactly one blocking
//! point - none. It polls its control channel without blocking and reaps the worker
//! without blocking, so no state of the worker (computing, sleeping, or wedged in a
//! loop that never enters the kernel) can stop it from answering STOP. A supervisor
//! that called `wait` on its worker would be the bug this requirement is about.
#![no_std]
#![no_main]

extern crate alloc;

use alloc::string::String;

use libspace::spaceabi::error::Error;
use libspace::spaceabi::handle::rights;
use libspace::spaceabi::shell::{ABI_VERSION, Command, Reply, cmd, job, worker_state};
use libspace::spaceabi::syscall::{DIR_ENTRIES_MAX, DirEntry, ExitStatus, kill_reason};
use libspace::{Handle, handle, println, sys};

/// Quota for a job: code, stack and a little heap.
const WORKER_QUOTA: u64 = 128;
/// Idle pause between polls. Short enough that Stop feels immediate, long enough
/// that an idle session is not a busy loop.
const POLL_MS: u64 = 2;

struct Shell {
    control: Handle,
    /// Root capability narrowed to what a session needs: spawn jobs, list files.
    /// No shutdown, no kernel stats, no debug.
    root: Option<Handle>,
    worker: Option<Handle>,
    job: String,
    state: u32,
    last_code: i64,
    last_reason: u32,
    served: u32,
    /// What has been typed since the last newline.
    line: String,
    /// True once the terminal has announced itself, one way or the other.
    terminal: bool,
    /// False once a console read has been refused: a session without the console
    /// right should not keep asking every two milliseconds.
    console_ok: bool,
    /// Last byte was a carriage return, so a following line feed is the other half
    /// of one CRLF and not a second empty line.
    last_cr: bool,
}

fn as_bytes<T>(v: &T) -> &[u8] {
    // SAFETY: `T` is a `repr(C)` plain-data message.
    unsafe { core::slice::from_raw_parts(v as *const T as *const u8, core::mem::size_of::<T>()) }
}

fn state_name(s: u32) -> &'static str {
    match s {
        worker_state::IDLE => "idle",
        worker_state::RUNNING => "running",
        worker_state::DONE => "done",
        worker_state::CRASHED => "crashed",
        worker_state::STOPPED => "stopped",
        _ => "?",
    }
}

impl Shell {
    fn new(control: Handle) -> Shell {
        Shell {
            control,
            root: None,
            worker: None,
            job: String::new(),
            state: worker_state::IDLE,
            last_code: 0,
            last_reason: 0,
            served: 0,
            line: String::new(),
            terminal: false,
            console_ok: true,
            last_cr: false,
        }
    }

    /// One line of session UI. `SYS_LOG` reaches the serial console and the
    /// framebuffer text console, so this is what a user would see.
    fn paint(&self) {
        println!(
            "[shell] session: worker {} ({}), commands served {}",
            state_name(self.state),
            if self.job.is_empty() { "-" } else { self.job.as_str() },
            self.served
        );
    }

    fn reply(&self, mut r: Reply) {
        r.state = self.state;
        r.reason = self.last_reason;
        r.served = self.served;
        let _ = sys::send(self.control, as_bytes(&r), None);
    }

    fn reply_err(&self, e: Error) {
        self.reply(Reply { status: -(e as u32 as i32), ..Default::default() });
    }

    /// Reap the worker if it has ended. Never blocks: a wedged worker must not be
    /// able to hold the session.
    fn poll_worker(&mut self) {
        let Some(h) = self.worker else { return };
        match sys::wait_nonblocking(h) {
            Ok(st) => {
                self.finish(st);
                sys::handle_close(h).ok();
                self.worker = None;
            }
            Err(Error::WouldBlock) => {}
            Err(e) => {
                println!("[shell] worker handle is unusable ({e}); dropping it");
                sys::handle_close(h).ok();
                self.worker = None;
                self.state = worker_state::CRASHED;
                self.paint();
            }
        }
    }

    fn finish(&mut self, st: ExitStatus) {
        self.last_code = st.code as i64;
        self.last_reason = st.reason;
        self.state = if st.reason == kill_reason::SIGNAL {
            worker_state::STOPPED
        } else if st.reason != 0 {
            worker_state::CRASHED
        } else {
            worker_state::DONE
        };
        if self.state == worker_state::CRASHED {
            println!(
                "[shell] worker '{}' was killed: {} (fault at {:#x}); the session is unaffected",
                self.job,
                kill_reason::name(st.reason),
                st.fault_addr
            );
        } else {
            println!("[shell] worker '{}' ended: {}", self.job, state_name(self.state));
        }
        self.paint();
    }

    fn run(&mut self, name: &str) {
        if self.worker.is_some() {
            return self.reply_err(Error::WouldBlock);
        }
        match self.start_worker(name) {
            Ok(()) => self.reply(Reply::default()),
            Err(e) => self.reply_err(e),
        }
    }

    /// Spawn a job. Shared by the control protocol and the terminal, so both take
    /// exactly the same path.
    fn start_worker(&mut self, name: &str) -> Result<(), Error> {
        let root = self.root.ok_or(Error::Denied)?;
        let (mine, theirs) = sys::channel_create()?;
        let p = match sys::spawn(root, "bin/uiworker", WORKER_QUOTA, Some(theirs)) {
            Ok(p) => p,
            Err(e) => {
                sys::handle_close(mine).ok();
                // Spawn consumes the transferred handle only once it has taken it;
                // a refusal before that (no right, no memory) leaves it here. Closing
                // an already-consumed handle is a no-op, so close it either way.
                sys::handle_close(theirs).ok();
                return Err(e);
            }
        };
        if let Err(e) = sys::send(mine, name.as_bytes(), None) {
            sys::kill(p).ok();
            sys::handle_close(p).ok();
            sys::handle_close(mine).ok();
            return Err(e);
        }
        // The job channel has done its work; the worker reports through its exit
        // status, which the kernel keeps for us.
        sys::handle_close(mine).ok();
        self.worker = Some(p);
        self.job.clear();
        self.job.push_str(name);
        self.state = worker_state::RUNNING;
        self.last_code = 0;
        self.last_reason = 0;
        println!("[shell] started worker '{name}'");
        self.paint();
        Ok(())
    }

    fn stop(&mut self) {
        let Some(h) = self.worker else {
            return self.reply_err(Error::NotFound);
        };
        // Exactly the path the typed `stop` takes. A worker that finished on its own
        // a moment ago is not a failed Stop: it is a Stop with nothing left to do,
        // and the reply carries the state either way.
        self.stop_worker(h);
        self.reply(Reply::default());
    }

    /// Kill and reap the running job. `kill` works whatever the worker is doing:
    /// computing, blocked in a syscall, or spinning without ever entering the kernel
    /// again. Reaping here keeps Stop synchronous; the target is already dead, so it
    /// cannot block for long.
    fn stop_worker(&mut self, h: Handle) {
        sys::kill(h).ok();
        match sys::wait(h) {
            Ok(st) => self.finish(st),
            Err(e) => {
                println!("[shell] stop: wait failed ({e})");
                self.state = worker_state::STOPPED;
            }
        }
        sys::handle_close(h).ok();
        self.worker = None;
    }

    fn list(&mut self, path: &str) {
        let Some(root) = self.root else {
            return self.reply_err(Error::Denied);
        };
        let mut entries = [DirEntry::default(); DIR_ENTRIES_MAX];
        let n = match sys::fs_list(root, path, &mut entries) {
            Ok(n) => n,
            Err(e) => return self.reply_err(e),
        };
        let mut text = String::new();
        for e in entries.iter().take(n) {
            if !text.is_empty() {
                text.push(' ');
            }
            text.push_str(e.name());
            if e.is_dir != 0 {
                text.push('/');
            }
        }
        println!("[shell] files in {path}: {n} entr{} [{text}]", if n == 1 { "y" } else { "ies" });
        let mut r = Reply { value: n as u64, ..Default::default() };
        r.set_text(&text);
        self.reply(r);
    }

    fn status(&mut self) {
        let mut r = Reply { value: self.last_code as u64, ..Default::default() };
        r.set_text(if self.job.is_empty() { "-" } else { self.job.as_str() });
        self.reply(r);
    }

    /// Handle one command. Returns false when the session should end.
    fn handle(&mut self, c: &Command, transferred: Option<Handle>) -> bool {
        self.served = self.served.saturating_add(1);
        // Only HELLO carries a capability. A handle attached to anything else would
        // sit in this process's table forever, so it is closed on arrival.
        let transferred = match (c.kind, transferred) {
            (cmd::HELLO, t) => t,
            (_, Some(h)) => {
                sys::handle_close(h).ok();
                None
            }
            (_, None) => None,
        };
        if c.kind != cmd::HELLO && self.root.is_none() {
            self.reply_err(Error::Denied);
            return true;
        }
        match c.kind {
            cmd::HELLO => {
                if c.abi_version != ABI_VERSION {
                    if let Some(h) = transferred {
                        sys::handle_close(h).ok();
                    }
                    println!(
                        "[shell] rejecting session ABI v{} (this shell speaks v{ABI_VERSION})",
                        c.abi_version
                    );
                    self.reply_err(Error::Invalid);
                    return true;
                }
                match transferred {
                    Some(h) => {
                        if let Some(old) = self.root.replace(h) {
                            sys::handle_close(old).ok();
                        }
                        // A new capability is a new answer to "can this session read
                        // the console": look again rather than stay latched off.
                        self.console_ok = true;
                        self.terminal = false;
                        println!("[shell] session open, ABI v{ABI_VERSION}");
                        self.paint();
                        self.reply(Reply { value: ABI_VERSION as u64, ..Default::default() });
                    }
                    None => self.reply_err(Error::Denied),
                }
            }
            cmd::STATUS => self.status(),
            cmd::LIST => self.list(c.text()),
            cmd::RUN => self.run(c.text()),
            cmd::STOP => self.stop(),
            cmd::QUIT => {
                self.reply(Reply::default());
                return false;
            }
            _ => self.reply_err(Error::NoSys),
        }
        true
    }

    /// Show the prompt. Written with `print!` so the cursor stays on the line the
    /// person is typing on.
    fn prompt(&self) {
        libspace::print!("space> ");
    }

    /// Feed one typed byte to the line editor. Returns false when the session
    /// should end (the person typed `quit`).
    fn typed(&mut self, byte: u8) -> bool {
        let was_cr = core::mem::replace(&mut self.last_cr, byte == b'\r');
        match byte {
            b'\n' if was_cr => true,
            b'\r' | b'\n' => {
                println!();
                let mut line = core::mem::take(&mut self.line);
                let ok = self.command(line.trim());
                line.clear();
                self.line = line;
                if ok {
                    self.prompt();
                }
                ok
            }
            // Backspace and delete: erase one character, and erase it on screen too.
            0x08 | 0x7F => {
                if self.line.pop().is_some() {
                    libspace::print!("\u{8} \u{8}");
                }
                true
            }
            b if (0x20..0x7F).contains(&b) => {
                if self.line.len() < 96 {
                    self.line.push(b as char);
                    // SAFETY-free echo: one printable ASCII byte.
                    libspace::print!("{}", b as char);
                }
                true
            }
            // Anything else (escape sequences, control keys) is not a character this
            // terminal knows; ignoring it is better than pretending.
            _ => true,
        }
    }

    /// Run one typed command. Returns false only for `quit`.
    fn command(&mut self, line: &str) -> bool {
        if line.is_empty() {
            return true;
        }
        self.served = self.served.saturating_add(1);
        let (verb, rest) = match line.split_once(' ') {
            Some((v, r)) => (v, r.trim()),
            None => (line, ""),
        };
        match verb {
            "help" => println!(
                "[shell] commands: help, status, ls [path], run <{}|{}|{}|{}>, stop, quit",
                job::OK,
                job::CRASH,
                job::HANG,
                job::SLOW
            ),
            "status" => println!(
                "[shell] worker {} ({}), last exit code {}, commands served {}",
                state_name(self.state),
                if self.job.is_empty() { "-" } else { self.job.as_str() },
                self.last_code,
                self.served
            ),
            "ls" => {
                let path = if rest.is_empty() { "/spaceos" } else { rest };
                if let Err(e) = self.list_to_console(path) {
                    println!("[shell] ls {path}: {e}");
                }
            }
            "run" => {
                if rest.is_empty() {
                    println!("[shell] run needs a job name; try 'help'");
                } else if self.worker.is_some() {
                    println!("[shell] a worker is already running; 'stop' it first");
                } else if let Err(e) = self.start_worker(rest) {
                    println!("[shell] run {rest}: {e}");
                }
            }
            "stop" => match self.worker {
                Some(h) => self.stop_worker(h),
                None => println!("[shell] nothing to stop"),
            },
            "quit" => {
                println!("[shell] closing the session");
                return false;
            }
            other => println!("[shell] unknown command {other:?}; try 'help'"),
        }
        true
    }

    fn list_to_console(&mut self, path: &str) -> Result<(), Error> {
        let root = self.root.ok_or(Error::Denied)?;
        let mut entries = [DirEntry::default(); DIR_ENTRIES_MAX];
        let n = sys::fs_list(root, path, &mut entries)?;
        println!("[shell] {path}: {n} entr{}", if n == 1 { "y" } else { "ies" });
        for e in entries.iter().take(n) {
            println!("[shell]   {:<12} {:>8} {}", e.name(), e.size, if e.is_dir != 0 { "dir" } else { "" });
        }
        Ok(())
    }

    fn shutdown(&mut self) {
        if let Some(h) = self.worker.take() {
            sys::kill(h).ok();
            sys::wait(h).ok();
            sys::handle_close(h).ok();
        }
        if let Some(h) = self.root.take() {
            sys::handle_close(h).ok();
        }
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn space_main() -> i32 {
    println!("[shell] Space OS session service, ABI v{ABI_VERSION}");
    let mut sh = Shell::new(handle::BOOTSTRAP);
    let mut buf = [0u8; core::mem::size_of::<Command>()];
    let mut typed = [0u8; 64];
    loop {
        sh.poll_worker();
        // The terminal only exists once a capability arrives that can read it.
        let mut busy = false;
        if let (Some(root), true) = (sh.root, sh.console_ok) {
            // Announce the terminal only after a read has proven the capability is
            // really there: a session without the console right has no terminal, and
            // saying otherwise would be a lie in the log.
            let first = sys::console_read(root, &mut typed);
            if !sh.terminal {
                sh.terminal = true;
                match &first {
                    Ok(_) => {
                        println!("[shell] terminal ready on the console; type 'help'");
                        sh.prompt();
                    }
                    Err(e) => println!("[shell] no console capability ({e}); channel control only"),
                }
            }
            match first {
                Ok(0) => {}
                Ok(n) => {
                    busy = true;
                    let mut alive = true;
                    for byte in typed.iter().take(n) {
                        if !sh.typed(*byte) {
                            alive = false;
                            break;
                        }
                    }
                    if !alive {
                        break;
                    }
                }
                // Refused once is refused for good: this session is driven by its
                // channel only, and retrying every poll would be pure waste.
                Err(_) => sh.console_ok = false,
            }
        }
        // Both inputs are served on every pass. Handling the console and looping
        // straight back would let a stream of keystrokes starve the control channel,
        // which is the failure this loop exists to avoid.
        match sys::recv(sh.control, &mut buf, true) {
            Ok((n, transferred)) if n == buf.len() => {
                // SAFETY: a front end sends exactly one `Command`.
                let c: Command = unsafe { core::ptr::read_unaligned(buf.as_ptr() as *const Command) };
                if !sh.handle(&c, transferred) {
                    break;
                }
            }
            Ok((n, transferred)) => {
                if let Some(h) = transferred {
                    sys::handle_close(h).ok();
                }
                println!("[shell] malformed command of {n} bytes");
                sh.served = sh.served.saturating_add(1);
                sh.reply_err(Error::MsgSize);
            }
            // Only idle when neither input had anything: a pause here would add
            // latency to every keystroke.
            Err(Error::WouldBlock) => {
                if !busy {
                    sys::sleep_ms(POLL_MS)
                }
            }
            Err(Error::PeerClosed) => {
                println!("[shell] front end disconnected; closing the session");
                break;
            }
            Err(e) => {
                println!("[shell] receive failed: {e}");
                break;
            }
        }
    }
    sh.shutdown();
    println!("[shell] session closed after {} commands", sh.served);
    0
}

/// Kept so the linker never drops the rights constants the service documents it
/// needs; the front end narrows the root handle to exactly these.
pub const NEEDED_ROOT_RIGHTS: u32 = rights::SPAWN | rights::FS | rights::CONSOLE;
