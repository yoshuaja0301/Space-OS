//! Client side of the session protocol (see `spaceabi::shell`).
//!
//! A front end holds one channel to `spaceshell` and one narrowed root capability
//! that it hands over during `open`. Everything else is a request/reply pair.

use spaceabi::error::Error;
use spaceabi::handle::{Handle, rights};
use spaceabi::shell::{ABI_VERSION, Command, Progress, Reply, cmd};

use crate::sys;

pub struct Session {
    channel: Handle,
}

/// Start a session service only to ask it whether the OS is usable
/// ([`Session::check`]), and end it again: for a front end that has no session of
/// its own to ask. The session gets `root` narrowed to starting programs and
/// reading files (no console: it must not take keystrokes meant for someone else).
/// `root` needs `SPAWN`, `FS`, `TRANSFER` and `DUP`.
pub fn check_usable(root: Handle) -> Result<bool, Error> {
    const QUOTA: u64 = 192;
    let (mine, theirs) = sys::channel_create()?;
    let shell = match sys::spawn(root, "bin/spaceshell", QUOTA, Some(theirs)) {
        Ok(h) => h,
        Err(e) => {
            sys::handle_close(mine).ok();
            sys::handle_close(theirs).ok();
            return Err(e);
        }
    };
    let result = sys::handle_dup(root, rights::SPAWN | rights::FS | rights::TRANSFER | rights::DUP)
        .and_then(|narrow| Session::open(mine, narrow))
        .and_then(|s| {
            let usable = s.check();
            s.quit().ok();
            usable
        });
    // A shell that does not go after QUIT is stopped: a check must not linger.
    let until = sys::ticks_ms() + 1000;
    while sys::wait_nonblocking(shell).is_err() && sys::ticks_ms() < until {
        sys::sleep_ms(5);
    }
    sys::kill(shell).ok();
    sys::wait(shell).ok();
    sys::handle_close(shell).ok();
    sys::handle_close(mine).ok();
    result
}

fn as_bytes<T>(v: &T) -> &[u8] {
    // SAFETY: `T` is a `repr(C)` plain-data message.
    unsafe { core::slice::from_raw_parts(v as *const T as *const u8, core::mem::size_of::<T>()) }
}

impl Session {
    /// Negotiate the version and hand the shell the capability it runs with. The
    /// shell can do nothing at all until this succeeds, so a front end decides
    /// exactly how much authority a session has.
    pub fn open(channel: Handle, root: Handle) -> Result<Session, Error> {
        let s = Session { channel };
        let c = Command { kind: cmd::HELLO, abi_version: ABI_VERSION, ..Default::default() };
        sys::send(channel, as_bytes(&c), Some(root))?;
        s.reply()?.result()?;
        Ok(s)
    }

    fn reply(&self) -> Result<Reply, Error> {
        let mut buf = [0u8; core::mem::size_of::<Reply>()];
        let (n, transferred) = sys::recv(self.channel, &mut buf, false)?;
        if let Some(h) = transferred {
            sys::handle_close(h).ok();
        }
        if n != buf.len() {
            return Err(Error::MsgSize);
        }
        // SAFETY: the shell replies with exactly one `Reply`.
        Ok(unsafe { core::ptr::read_unaligned(buf.as_ptr() as *const Reply) })
    }

    /// Send a command and read the answer. The status is left to the caller so
    /// tests can inspect refusals.
    pub fn call(&self, kind: u32, text: &str) -> Result<Reply, Error> {
        sys::send(self.channel, as_bytes(&Command::new(kind, text)), None)?;
        self.reply()
    }

    pub fn status(&self) -> Result<Reply, Error> {
        self.call(cmd::STATUS, "")
    }

    pub fn list(&self, path: &str) -> Result<Reply, Error> {
        self.call(cmd::LIST, path)
    }

    pub fn run(&self, job: &str) -> Result<Reply, Error> {
        self.call(cmd::RUN, job)
    }

    pub fn stop(&self) -> Result<Reply, Error> {
        self.call(cmd::STOP, "")
    }

    pub fn quit(&self) -> Result<Reply, Error> {
        self.call(cmd::QUIT, "")
    }

    /// Ask the shell to show the OS is usable (`cmd::CHECK`). `Ok(true)` when it is.
    pub fn check(&self) -> Result<bool, Error> {
        let r = self.call(cmd::CHECK, "")?;
        r.result()?;
        Ok(r.value == 1)
    }

    /// What the running (or last) job has done: tokens, timing, memory, and how the
    /// last Stop went.
    pub fn progress(&self) -> Result<Progress, Error> {
        // Room for either answer: a refusal is a `Reply`, which is the larger of the two.
        const ROOM: usize = if core::mem::size_of::<Reply>() > core::mem::size_of::<Progress>() {
            core::mem::size_of::<Reply>()
        } else {
            core::mem::size_of::<Progress>()
        };
        sys::send(self.channel, as_bytes(&Command::new(cmd::PROGRESS, "")), None)?;
        let mut buf = [0u8; ROOM];
        let (n, transferred) = sys::recv(self.channel, &mut buf, false)?;
        if let Some(h) = transferred {
            sys::handle_close(h).ok();
        }
        // A refusal (an old shell, a session not yet open) comes back as a `Reply`.
        if n == core::mem::size_of::<Reply>() {
            // SAFETY: the shell answered with exactly one `Reply`.
            let r: Reply = unsafe { core::ptr::read_unaligned(buf.as_ptr() as *const Reply) };
            r.result()?;
            return Err(Error::MsgSize);
        }
        if n != core::mem::size_of::<Progress>() {
            return Err(Error::MsgSize);
        }
        // SAFETY: the shell answers `PROGRESS` with exactly one `Progress`.
        Ok(unsafe { core::ptr::read_unaligned(buf.as_ptr() as *const Progress) })
    }
}
