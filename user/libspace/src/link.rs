//! Client side of the SpaceLink protocol (see `spaceabi::link`).
//!
//! A front end starts its own `bin/spacelink` and hands it one file capability;
//! everything else is a request/reply pair over the service's channel.

use spaceabi::error::Error;
use spaceabi::handle::Handle;
use spaceabi::link::{ABI_VERSION, LinkReply, LinkRequest, req};

use crate::sys;

pub struct Link {
    chan: Handle,
    process: Handle,
}

fn as_bytes(r: &LinkRequest) -> &[u8] {
    // SAFETY: `LinkRequest` is a `repr(C)` plain-data message.
    unsafe {
        core::slice::from_raw_parts(r as *const LinkRequest as *const u8, core::mem::size_of::<LinkRequest>())
    }
}

/// Counts from `STATS`: indexed documents, revoked documents, chunks.
#[derive(Clone, Copy, Default)]
pub struct Stats {
    pub documents: u32,
    pub revoked: u32,
    pub chunks: u32,
}

impl Link {
    /// Start `bin/spacelink` (spawned with `spawn_cap`) and attach it to `fs`, the one
    /// capability it runs with: read access gives search, and revocations it cannot
    /// write stay in its memory.
    pub fn start(spawn_cap: Handle, fs: Handle, quota: u64) -> Result<Link, Error> {
        let (mine, theirs) = sys::channel_create().inspect_err(|_| {
            sys::handle_close(fs).ok();
        })?;
        let process = sys::spawn(spawn_cap, "bin/spacelink", quota, Some(theirs)).inspect_err(|_| {
            // Spawn consumes the handle only once it has taken it; nothing new was
            // created since, so closing a consumed one is a no-op.
            for h in [fs, mine, theirs] {
                sys::handle_close(h).ok();
            }
        })?;
        let link = Link { chan: mine, process };
        let mut hello = LinkRequest::new(req::HELLO);
        hello.abi_version = ABI_VERSION;
        if let Err(e) = sys::send(mine, as_bytes(&hello), Some(fs)) {
            sys::handle_close(fs).ok();
            link.end();
            return Err(e);
        }
        // From here `fs` belongs to the service, whatever it answers.
        if let Err(e) = link.reply().and_then(|r| r.result()) {
            link.end();
            return Err(e);
        }
        Ok(link)
    }

    fn reply(&self) -> Result<LinkReply, Error> {
        let mut buf = [0u8; core::mem::size_of::<LinkReply>()];
        let (n, transferred) = sys::recv(self.chan, &mut buf, false)?;
        if let Some(h) = transferred {
            sys::handle_close(h).ok();
        }
        if n != buf.len() {
            return Err(Error::MsgSize);
        }
        // SAFETY: the service replies with exactly one `LinkReply`.
        Ok(unsafe { core::ptr::read_unaligned(buf.as_ptr() as *const LinkReply) })
    }

    /// Send a request and read the answer; the status is left to the caller.
    pub fn call(&self, r: &LinkRequest) -> Result<LinkReply, Error> {
        sys::send(self.chan, as_bytes(r), None)?;
        self.reply()
    }

    /// Index every text file directly inside `path`.
    pub fn index(&self, path: &str) -> Result<LinkReply, Error> {
        let r = self.call(&LinkRequest::with_path(req::INDEX, path))?;
        r.result()?;
        Ok(r)
    }

    pub fn stats(&self) -> Result<Stats, Error> {
        let r = self.call(&LinkRequest::new(req::STATS))?;
        r.result()?;
        Ok(Stats { documents: r.value, revoked: r.value2, chunks: r.value3 })
    }

    /// Result `offset` of the ranked results for `query` (its `total` says how many
    /// there are; an offset past the end is `NotFound`).
    pub fn query(&self, query: &str, offset: u32) -> Result<LinkReply, Error> {
        let mut r = LinkRequest::with_query(req::QUERY, query);
        r.offset = offset;
        self.call(&r)
    }

    /// Build a context bundle for `query` under `budget` bytes: `total` entries,
    /// `value` bytes, `digest` over the entries in order.
    pub fn bundle(&self, query: &str, budget: u32) -> Result<LinkReply, Error> {
        let mut r = LinkRequest::with_query(req::BUNDLE, query);
        r.budget = budget;
        let reply = self.call(&r)?;
        reply.result()?;
        Ok(reply)
    }

    /// Ask the service to exit, and make sure it has.
    pub fn quit(self) {
        let _ = self.call(&LinkRequest::new(req::QUIT));
        self.end();
    }

    fn end(&self) {
        let until = sys::ticks_ms() + 1000;
        while sys::wait_nonblocking(self.process).is_err() && sys::ticks_ms() < until {
            sys::sleep_ms(5);
        }
        sys::kill(self.process).ok();
        sys::wait(self.process).ok();
        sys::handle_close(self.process).ok();
        sys::handle_close(self.chan).ok();
    }
}
