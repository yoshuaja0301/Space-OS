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

use alloc::format;
use alloc::string::String;

use libspace::spaceabi::error::Error;
use libspace::spaceabi::handle::rights;
use libspace::spaceabi::shell::{
    ABI_VERSION, Command, JobReport, Progress, Reply, cmd, job, worker, worker_state,
};
use libspace::spaceabi::syscall::{DIR_ENTRIES_MAX, DirEntry, ExitStatus, kill_reason};
use libspace::{Handle, exit_kind, handle, println, sys};

/// Quota for a test job: code, stack and a little heap.
const WORKER_QUOTA: u64 = 128;
/// Quota for the inference worker (the runtime; the model lives in compute buffers).
const INFER_QUOTA: u64 = 512;
/// Quota for the inference worker's compute service, which holds the model.
const COMPUTE_QUOTA: u64 = 4096;
/// Idle pause between polls. Short enough that Stop feels immediate, long enough
/// that an idle session is not a busy loop.
const POLL_MS: u64 = 2;
/// Where `cmd::CHECK` looks for a file to read: the top of the data volume.
const CHECK_DIR: &str = "/spaceos";
/// How long the check's finishing program may take before the check gives up.
const CHECK_DEADLINE_MS: u64 = 10_000;

struct Shell {
    control: Handle,
    /// Root capability narrowed to what a session needs: spawn jobs, list files.
    /// No shutdown, no kernel stats, no debug.
    root: Option<Handle>,
    worker: Option<Handle>,
    /// The compute service an inference worker runs on; ended with the worker.
    helper: Option<Handle>,
    /// The inference worker's bootstrap channel: its reports arrive here, and Stop
    /// is asked for here before anything is killed.
    job_chan: Option<Handle>,
    /// What the inference job has reported, and how the last Stop went.
    progress: Progress,
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
            helper: None,
            job_chan: None,
            progress: Progress::default(),
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
        self.drain_reports();
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

    /// Take every report the inference worker has sent. Never blocks.
    fn drain_reports(&mut self) {
        let Some(ch) = self.job_chan else { return };
        let mut buf = [0u8; core::mem::size_of::<JobReport>()];
        // Until nothing more is queued, or the worker is gone: either way, done here.
        while let Ok((n, transferred)) = sys::recv(ch, &mut buf, true) {
            if let Some(h) = transferred {
                sys::handle_close(h).ok();
            }
            if n == buf.len() {
                // SAFETY: the worker sends exactly one `JobReport` per message.
                let r: JobReport = unsafe { core::ptr::read_unaligned(buf.as_ptr() as *const JobReport) };
                self.absorb(&r);
            }
        }
    }

    fn absorb(&mut self, r: &JobReport) {
        let p = &mut self.progress;
        p.done = r.done;
        p.matched = r.matched;
        p.total = r.total;
        p.ttft_ms = r.ttft_ms;
        p.elapsed_ms = r.elapsed_ms;
        p.used_pages = r.used_pages;
        let text = r.text().as_bytes();
        let n = text.len().min(p.text.len());
        p.text[..n].copy_from_slice(&text[..n]);
        p.text_len = n as u32;
        match r.kind {
            worker::report::DONE => println!(
                "[shell] worker '{}': {} tokens, {} matching the baseline, first after {} ms",
                self.job, r.done, r.matched, r.ttft_ms
            ),
            worker::report::STOPPED => println!(
                "[shell] worker '{}' stopped between two steps after {} of {} tokens",
                self.job, r.done, r.total
            ),
            worker::report::FAILED => println!("[shell] worker '{}' gave up: {}", self.job, r.text()),
            _ => {}
        }
    }

    fn finish(&mut self, st: ExitStatus) {
        // The worker's last words may still be queued behind its exit.
        self.drain_reports();
        self.last_code = st.code as i64;
        self.last_reason = st.reason;
        self.state = if st.reason == kill_reason::SIGNAL {
            worker_state::STOPPED
        } else if st.reason != 0 {
            worker_state::CRASHED
        } else if self.job == job::INFER && st.code == worker::EXIT_STOPPED {
            worker_state::STOPPED
        } else {
            worker_state::DONE
        };
        // An inference worker's compute service ends with it.
        if let Some(c) = self.helper.take() {
            sys::kill(c).ok();
            sys::wait(c).ok();
            sys::handle_close(c).ok();
        }
        if let Some(ch) = self.job_chan.take() {
            sys::handle_close(ch).ok();
        }
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
        if name == job::INFER {
            return self.start_inference();
        }
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
        self.started(p, name, WORKER_QUOTA);
        Ok(())
    }

    fn started(&mut self, p: Handle, name: &str, quota: u64) {
        self.worker = Some(p);
        self.job.clear();
        self.job.push_str(name);
        self.state = worker_state::RUNNING;
        self.last_code = 0;
        self.last_reason = 0;
        self.progress = Progress { quota_pages: quota as u32, ..Default::default() };
        println!("[shell] started worker '{name}'");
        self.paint();
    }

    /// The real job: `bin/spaceai` in session mode on a compute service of its own.
    /// The worker is given exactly three things - this channel (to report, and to be
    /// asked to stop), a compute connection, and read access to the file system. It
    /// cannot spawn, write, reach the network, or see any other process.
    fn start_inference(&mut self) -> Result<(), Error> {
        let root = self.root.ok_or(Error::Denied)?;
        let close = |hs: &[Handle]| {
            for h in hs {
                sys::handle_close(*h).ok();
            }
        };
        let end = |p: Handle| {
            sys::kill(p).ok();
            sys::wait(p).ok();
            sys::handle_close(p).ok();
        };
        // Handles that go to a child are closed here only when handing them over
        // failed. Spawn and send consume a handle once they have taken it, and a
        // consumed handle's number is free for the next handle this process gets -
        // so closing it after a success could close something else entirely. On the
        // failure paths nothing new is created in between, which makes closing a
        // consumed handle there a harmless no-op.
        let fs = sys::handle_dup(root, rights::FS | rights::TRANSFER)?;
        let (client, server) = sys::channel_create().inspect_err(|_| close(&[fs]))?;
        let compute = sys::spawn(root, "bin/spacecompute", COMPUTE_QUOTA, Some(server))
            .inspect_err(|_| close(&[fs, client, server]))?;
        let (mine, theirs) = sys::channel_create().inspect_err(|_| {
            end(compute);
            close(&[fs, client]);
        })?;
        let ai = sys::spawn(root, "bin/spaceai", INFER_QUOTA, Some(theirs)).inspect_err(|_| {
            end(compute);
            close(&[fs, client, mine, theirs]);
        })?;
        // The session first: from its very first message the worker knows it reports
        // here and can be asked to stop.
        let sent = sys::send(mine, worker::SESSION, None)
            .and_then(|_| sys::send(mine, b"compute", Some(client)))
            .and_then(|_| sys::send(mine, b"fs", Some(fs)));
        if let Err(e) = sent {
            end(ai);
            end(compute);
            close(&[fs, client, mine]);
            return Err(e);
        }
        self.helper = Some(compute);
        self.job_chan = Some(mine);
        self.started(ai, job::INFER, INFER_QUOTA);
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

    /// End the running job, and reap it before answering, so Stop is synchronous.
    ///
    /// A worker that can be asked (the inference worker) is asked first, and has
    /// [`worker::STOP_GRACE_MS`] to end between two compute steps: nothing is left
    /// half-done. Anything else - a test job, or a worker that does not answer in
    /// time because it is wedged - is killed. `kill` works whatever the worker is
    /// doing: computing, blocked in a syscall, or spinning without ever entering the
    /// kernel again; the target is dead by then, so the wait cannot block for long.
    fn stop_worker(&mut self, h: Handle) {
        let t0 = sys::ticks_ms();
        let mut ended = None;
        if let Some(ch) = self.job_chan
            && sys::send(ch, worker::STOP, None).is_ok()
        {
            let deadline = t0 + worker::STOP_GRACE_MS;
            loop {
                self.drain_reports();
                match sys::wait_nonblocking(h) {
                    Ok(st) => {
                        ended = Some(st);
                        break;
                    }
                    Err(Error::WouldBlock) if sys::ticks_ms() < deadline => sys::sleep_ms(1),
                    Err(_) => break,
                }
            }
        }
        let cooperative = ended.is_some();
        let st = match ended {
            Some(st) => Ok(st),
            None => {
                sys::kill(h).ok();
                sys::wait(h)
            }
        };
        match st {
            Ok(st) => self.finish(st),
            Err(e) => {
                println!("[shell] stop: wait failed ({e})");
                self.state = worker_state::STOPPED;
            }
        }
        sys::handle_close(h).ok();
        self.worker = None;
        let ms = sys::ticks_ms() - t0;
        self.progress.cooperative = cooperative as u32;
        self.progress.stop_ms = ms as u32;
        println!(
            "[shell] Stop: worker '{}' {} after {ms} ms",
            self.job,
            if cooperative { "ended between two steps" } else { "was killed" }
        );
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

    /// `cmd::CHECK` (PRD v0.2 §7.4): read a file, run a program to its end and stop
    /// another, then say so as the boot's status. Its programs are its own, so a
    /// check can run beside the session's worker and leaves its state alone.
    fn check(&mut self) {
        let (usable, text) = match self.usable() {
            Ok(text) => (true, text),
            Err(text) => (false, text),
        };
        if usable {
            println!("[status] OS usable: {text}");
        } else {
            println!("[status] OS not usable: {text}");
        }
        let mut r = Reply { value: usable as u64, ..Default::default() };
        r.set_text(&text);
        self.reply(r);
    }

    fn usable(&self) -> Result<String, String> {
        let root = self.root.ok_or_else(|| String::from("the session holds no capability"))?;
        let t0 = sys::ticks_ms();
        // A file: the first one with something in it at the top of the data volume.
        let mut entries = [DirEntry::default(); DIR_ENTRIES_MAX];
        let n = sys::fs_list(root, CHECK_DIR, &mut entries)
            .map_err(|e| format!("cannot read files: listing {CHECK_DIR}: {e}"))?;
        let file = entries
            .iter()
            .take(n)
            .find(|e| e.is_dir == 0 && e.size > 0)
            .ok_or_else(|| format!("cannot read files: nothing to read in {CHECK_DIR}"))?;
        let path = format!("{CHECK_DIR}/{}", file.name());
        let f = sys::fs_open(root, &path).map_err(|e| format!("cannot read files: opening {path}: {e}"))?;
        let mut buf = [0u8; 512];
        let read = sys::fs_read(f, 0, &mut buf);
        sys::handle_close(f).ok();
        let read = read.map_err(|e| format!("cannot read files: reading {path}: {e}"))?;
        if read == 0 {
            return Err(format!("cannot read files: {path} reads as empty"));
        }
        // A program run to its end, with the exit code it chose.
        let p = check_job(root, job::OK)?;
        let deadline = sys::ticks_ms() + CHECK_DEADLINE_MS;
        let ended = loop {
            match sys::wait_nonblocking(p) {
                Ok(st) => break Ok(st),
                Err(Error::WouldBlock) if sys::ticks_ms() < deadline => sys::sleep_ms(POLL_MS),
                Err(e) => break Err(e),
            }
        };
        if ended.is_err() {
            sys::kill(p).ok();
            sys::wait(p).ok();
        }
        sys::handle_close(p).ok();
        match ended {
            Ok(st) if st.kind == exit_kind::EXITED && st.code == 0 => {}
            Ok(st) => return Err(format!("cannot run programs: bin/uiworker ended {st:?}")),
            Err(Error::WouldBlock) => {
                return Err(format!(
                    "cannot run programs: bin/uiworker did not finish in {CHECK_DEADLINE_MS} ms"
                ));
            }
            Err(e) => return Err(format!("cannot run programs: waiting for bin/uiworker: {e}")),
        }
        // And one stopped while it runs: it spins and never asks the kernel for
        // anything, so only the stop can end it.
        let p = check_job(root, job::HANG)?;
        sys::sleep_ms(POLL_MS);
        let early = sys::wait_nonblocking(p);
        let killed = sys::kill(p);
        let st = sys::wait(p);
        sys::handle_close(p).ok();
        match early {
            Err(Error::WouldBlock) => {}
            Ok(st) => return Err(format!("cannot stop programs: bin/uiworker ended by itself ({st:?})")),
            Err(e) => return Err(format!("cannot stop programs: watching bin/uiworker: {e}")),
        }
        killed.map_err(|e| format!("cannot stop programs: {e}"))?;
        let st = st.map_err(|e| format!("cannot stop programs: reaping bin/uiworker: {e}"))?;
        if st.reason != kill_reason::SIGNAL {
            return Err(format!("cannot stop programs: bin/uiworker ended {st:?}, not by the stop"));
        }
        Ok(format!(
            "read {read} bytes of {path}, ran bin/uiworker to its end and stopped another while it ran, in {} ms",
            sys::ticks_ms() - t0
        ))
    }

    fn send_progress(&mut self) {
        self.drain_reports();
        let p = Progress { status: 0, state: self.state, served: self.served, ..self.progress };
        let _ = sys::send(self.control, as_bytes(&p), None);
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
            cmd::PROGRESS => self.send_progress(),
            cmd::CHECK => self.check(),
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

    /// Forget the partly typed line. Used when the kernel reports that buffered
    /// input was lost: what survives is the tail of a line, not a line.
    fn discard_line(&mut self) {
        self.line.clear();
        self.last_cr = false;
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
                "[shell] commands: help, status, ls [path], run <{}|{}|{}|{}|{}>, stop, quit",
                job::INFER,
                job::OK,
                job::CRASH,
                job::HANG,
                job::SLOW
            ),
            "status" => {
                self.drain_reports();
                let tokens = if self.job == job::INFER {
                    alloc::format!(", {}/{} tokens", self.progress.done, self.progress.total)
                } else {
                    String::new()
                };
                println!(
                    "[shell] worker {} ({}){tokens}, last exit code {}, commands served {}",
                    state_name(self.state),
                    if self.job.is_empty() { "-" } else { self.job.as_str() },
                    self.last_code,
                    self.served
                )
            }
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
        for h in [self.worker.take(), self.helper.take()].into_iter().flatten() {
            sys::kill(h).ok();
            sys::wait(h).ok();
            sys::handle_close(h).ok();
        }
        if let Some(ch) = self.job_chan.take() {
            sys::handle_close(ch).ok();
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
                // Lost input still proves the capability is there, so it announces a
                // terminal like any other successful read would.
                match &first {
                    Ok(_) | Err(Error::DataLoss) => {
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
                // The kernel dropped buffered bytes, so the half-line assembled so
                // far has a hole in it. Throwing it away and saying so is the only
                // honest answer: running what is left would run a command the person
                // never typed. The console itself is fine, so it stays open.
                Err(Error::DataLoss) => {
                    busy = true;
                    sh.discard_line();
                    println!();
                    println!("[shell] input was lost; the line was discarded");
                    sh.prompt();
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

/// Start `bin/uiworker` on `name` for the check, with a quota of its own.
fn check_job(root: Handle, name: &str) -> Result<Handle, String> {
    let (mine, theirs) = sys::channel_create().map_err(|e| format!("cannot run programs: channel: {e}"))?;
    let p = match sys::spawn(root, "bin/uiworker", WORKER_QUOTA, Some(theirs)) {
        Ok(p) => p,
        Err(e) => {
            sys::handle_close(mine).ok();
            // Only taken once spawn succeeds; a no-op if it already was.
            sys::handle_close(theirs).ok();
            return Err(format!("cannot run programs: bin/uiworker: {e}"));
        }
    };
    let sent = sys::send(mine, name.as_bytes(), None);
    sys::handle_close(mine).ok();
    if let Err(e) = sent {
        sys::kill(p).ok();
        sys::wait(p).ok();
        sys::handle_close(p).ok();
        return Err(format!("cannot run programs: handing bin/uiworker its job: {e}"));
    }
    Ok(p)
}

/// Kept so the linker never drops the rights constants the service documents it
/// needs; the front end narrows the root handle to exactly these.
pub const NEEDED_ROOT_RIGHTS: u32 = rights::SPAWN | rights::FS | rights::CONSOLE | rights::DUP;
