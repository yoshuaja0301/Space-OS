//! Client side of the session protocol (see `spaceabi::shell`).
//!
//! A front end holds one channel to `spaceshell` and one narrowed root capability
//! that it hands over during `open`. Everything else is a request/reply pair.

use spaceabi::error::Error;
use spaceabi::handle::Handle;
use spaceabi::shell::{ABI_VERSION, Command, Progress, Reply, cmd};

use crate::sys;

pub struct Session {
    channel: Handle,
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
