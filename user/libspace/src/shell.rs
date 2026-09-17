//! Client side of the session protocol (see `spaceabi::shell`).
//!
//! A front end holds one channel to `spaceshell` and one narrowed root capability
//! that it hands over during `open`. Everything else is a request/reply pair.

use spaceabi::error::Error;
use spaceabi::handle::Handle;
use spaceabi::shell::{ABI_VERSION, Command, Reply, cmd};

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
}
