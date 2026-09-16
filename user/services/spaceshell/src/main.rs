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
use libspace::spaceabi::shell::{ABI_VERSION, Command, Reply, cmd, worker_state};
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
        let Some(root) = self.root else {
            return self.reply_err(Error::Denied);
        };
        let (mine, theirs) = match sys::channel_create() {
            Ok(v) => v,
            Err(e) => return self.reply_err(e),
        };
        let p = match sys::spawn(root, "bin/uiworker", WORKER_QUOTA, Some(theirs)) {
            Ok(p) => p,
            Err(e) => {
                sys::handle_close(mine).ok();
                return self.reply_err(e);
            }
        };
        if let Err(e) = sys::send(mine, name.as_bytes(), None) {
            sys::kill(p).ok();
            sys::handle_close(p).ok();
            sys::handle_close(mine).ok();
            return self.reply_err(e);
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
        self.reply(Reply::default());
    }

    fn stop(&mut self) {
        let Some(h) = self.worker else {
            return self.reply_err(Error::NotFound);
        };
        // `kill` works whatever the worker is doing: computing, blocked in a syscall,
        // or spinning without ever entering the kernel again.
        if let Err(e) = sys::kill(h) {
            return self.reply_err(e);
        }
        // Reap it here so STOP is synchronous from the caller's point of view. The
        // target is already dead, so this cannot block for long.
        match sys::wait(h) {
            Ok(st) => self.finish(st),
            Err(e) => {
                println!("[shell] stop: wait failed ({e})");
                self.state = worker_state::STOPPED;
            }
        }
        sys::handle_close(h).ok();
        self.worker = None;
        self.reply(Reply::default());
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
    loop {
        sh.poll_worker();
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
            Err(Error::WouldBlock) => sys::sleep_ms(POLL_MS),
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
pub const NEEDED_ROOT_RIGHTS: u32 = rights::SPAWN | rights::FS;
