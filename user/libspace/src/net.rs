//! Client side of the network service ([`spaceabi::net`]): a session, name
//! lookups, and TCP connections that read and write like streams.
//!
//! Every call that waits takes a timeout, and waits with `wait_any`, so a service
//! that has died or stopped answering costs the caller the timeout and nothing
//! more.

use spaceabi::error::Error;
use spaceabi::net::{self as abi, DATA_MAX, NetReply, NetRequest, req, sock};
use spaceabi::syscall::MSG_MAX;

use crate::{Handle, sys};

/// Time allowed for the service to answer a request, on top of any deadline the
/// request itself carries.
const ANSWER_MS: u64 = 2000;

/// Wait up to `timeout_ms` for a message on `chan`, then take it without blocking.
fn recv_within(chan: Handle, buf: &mut [u8], timeout_ms: u64) -> Result<(usize, Option<Handle>), Error> {
    sys::wait_any(&[chan], timeout_ms)?;
    sys::recv(chan, buf, true)
}

/// A session with the network service: what it may reach is fixed by whoever
/// created it.
pub struct Session {
    chan: Handle,
}

impl Session {
    pub fn new(chan: Handle) -> Session {
        Session { chan }
    }

    pub fn handle(&self) -> Handle {
        self.chan
    }

    /// Send `r` (moving `carry` along with it) and wait for the answer.
    fn call(&self, r: &NetRequest, carry: Option<Handle>, timeout_ms: u64) -> Result<NetReply, Error> {
        sys::send(self.chan, r.as_bytes(), carry)?;
        let mut buf = [0u8; core::mem::size_of::<NetReply>()];
        let (n, h) = recv_within(self.chan, &mut buf, timeout_ms)?;
        if let Some(h) = h {
            sys::handle_close(h).ok();
        }
        let reply = NetReply::from_bytes(&buf[..n]).ok_or(Error::Invalid)?;
        reply.result().map(|()| reply)
    }

    /// The interface configuration: address, prefix, gateway, DNS server, MAC.
    pub fn hello(&self) -> Result<NetReply, Error> {
        self.call(&NetRequest::new(req::HELLO), None, ANSWER_MS)
    }

    /// Connections opened, destinations refused, names resolved by this session.
    pub fn stats(&self) -> Result<(u64, u64, u64), Error> {
        let r = self.call(&NetRequest::new(req::STATS), None, ANSWER_MS)?;
        Ok((r.value, r.value2, r.value3))
    }

    /// The IPv4 address of `host` (a name, or an address in dotted form).
    pub fn resolve(&self, host: &str, timeout_ms: u32) -> Result<[u8; 4], Error> {
        let mut r = NetRequest::with_host(req::RESOLVE, host, 0);
        r.timeout_ms = timeout_ms;
        Ok(self.call(&r, None, timeout_ms as u64 + ANSWER_MS)?.addr)
    }

    /// Open a TCP connection to `host` (a name, or an address in dotted form) on
    /// `port`, giving up after `timeout_ms`.
    pub fn connect(&self, host: &str, port: u16, timeout_ms: u32) -> Result<TcpStream, Error> {
        let mut r = match abi::parse_ipv4(host) {
            Some(a) => NetRequest::with_addr(req::CONNECT, a, port),
            None => NetRequest::with_host(req::CONNECT, host, port),
        };
        r.timeout_ms = timeout_ms;
        let (mine, theirs) = sys::channel_create()?;
        if let Err(e) = self.call(&r, Some(theirs), ANSWER_MS) {
            sys::handle_close(mine).ok();
            // Consumed if the request was queued, still ours if it was refused
            // before that; either way it must not outlive this call.
            sys::handle_close(theirs).ok();
            return Err(e);
        }
        let mut s = TcpStream::new(mine);
        let mut m = [0u8; MSG_MAX];
        let (n, h) = recv_within(mine, &mut m, timeout_ms as u64 + ANSWER_MS)?;
        if let Some(h) = h {
            sys::handle_close(h).ok();
        }
        match (n, m[0]) {
            (7, sock::CONNECTED) => {
                s.remote = ([m[1], m[2], m[3], m[4]], u16::from_le_bytes([m[5], m[6]]));
                Ok(s)
            }
            (5, sock::ERROR) => Err(error_of(&m[1..5])),
            _ => Err(Error::Invalid),
        }
    }
}

fn error_of(b: &[u8]) -> Error {
    let code = u32::from_le_bytes([b[0], b[1], b[2], b[3]]);
    Error::from_code(code).unwrap_or(Error::Invalid)
}

/// One TCP connection. Dropping it closes the connection: cleanly if everything it
/// received was read, with a reset otherwise.
pub struct TcpStream {
    chan: Handle,
    /// Received bytes not yet handed to the reader.
    pending: [u8; DATA_MAX],
    start: usize,
    end: usize,
    eof: bool,
    error: Option<Error>,
    /// Address and port actually connected to.
    pub remote: ([u8; 4], u16),
}

impl TcpStream {
    fn new(chan: Handle) -> TcpStream {
        TcpStream {
            chan,
            pending: [0; DATA_MAX],
            start: 0,
            end: 0,
            eof: false,
            error: None,
            remote: ([0; 4], 0),
        }
    }

    /// Queue all of `data` for sending. Waits while the connection has no room,
    /// for at most `timeout_ms` in total.
    pub fn write_all(&mut self, data: &[u8], timeout_ms: u64) -> Result<(), Error> {
        let deadline = sys::ticks_ms() + timeout_ms;
        let mut m = [0u8; MSG_MAX];
        m[0] = sock::DATA;
        for chunk in data.chunks(DATA_MAX) {
            m[1..1 + chunk.len()].copy_from_slice(chunk);
            loop {
                match sys::send(self.chan, &m[..1 + chunk.len()], None) {
                    Ok(()) => break,
                    Err(Error::WouldBlock) if sys::ticks_ms() < deadline => sys::sleep_ms(1),
                    Err(Error::WouldBlock) => return Err(Error::TimedOut),
                    Err(Error::PeerClosed) => return Err(self.why_closed()),
                    Err(e) => return Err(e),
                }
            }
        }
        Ok(())
    }

    /// No more data from this side: the peer reads end-of-stream once everything
    /// queued before has arrived. Reading stays possible. Like a write, it waits
    /// (at most `timeout_ms`) while the connection is still busy with earlier data.
    pub fn shutdown(&mut self, timeout_ms: u64) -> Result<(), Error> {
        let deadline = sys::ticks_ms() + timeout_ms;
        loop {
            match sys::send(self.chan, &[sock::SHUTDOWN], None) {
                Err(Error::WouldBlock) if sys::ticks_ms() < deadline => sys::sleep_ms(1),
                Err(Error::WouldBlock) => return Err(Error::TimedOut),
                Err(Error::PeerClosed) => return Err(self.why_closed()),
                r => return r,
            }
        }
    }

    /// The connection is gone (the service closed the channel). Whatever it said
    /// last, if it said anything, is the reason.
    fn why_closed(&mut self) -> Error {
        if self.error.is_none() {
            let mut m = [0u8; MSG_MAX];
            while let Ok((n, h)) = sys::recv(self.chan, &mut m, true) {
                if let Some(h) = h {
                    sys::handle_close(h).ok();
                }
                if n == 5 && m[0] == sock::ERROR {
                    self.error = Some(error_of(&m[1..5]));
                }
            }
        }
        self.error.unwrap_or(Error::PeerClosed)
    }

    /// Take the next message, waiting up to `timeout_ms` (0: do not wait).
    /// `Ok(false)` when nothing came in time.
    fn fill(&mut self, timeout_ms: u64) -> Result<bool, Error> {
        if timeout_ms > 0 {
            match sys::wait_any(&[self.chan], timeout_ms) {
                Ok(_) => {}
                Err(Error::TimedOut) => return Ok(false),
                Err(e) => return Err(e),
            }
        }
        let mut m = [0u8; MSG_MAX];
        match sys::recv(self.chan, &mut m, true) {
            Ok((n, h)) => {
                if let Some(h) = h {
                    sys::handle_close(h).ok();
                }
                match (n, m[0]) {
                    (2.., sock::RECEIVED) => {
                        self.pending[..n - 1].copy_from_slice(&m[1..n]);
                        self.start = 0;
                        self.end = n - 1;
                    }
                    (1, sock::EOF) => self.eof = true,
                    (5, sock::ERROR) => {
                        let e = error_of(&m[1..5]);
                        self.error = Some(e);
                        return Err(e);
                    }
                    _ => return Err(Error::Invalid),
                }
                Ok(true)
            }
            Err(Error::WouldBlock) => Ok(false),
            // The service closed the channel without a word: it is gone.
            Err(Error::PeerClosed) if self.eof => Ok(true),
            Err(e) => Err(e),
        }
    }

    /// Read what has arrived, waiting up to `timeout_ms` for something to. `Ok(0)`
    /// is end of stream; `TimedOut` means nothing came in time.
    pub fn read(&mut self, out: &mut [u8], timeout_ms: u64) -> Result<usize, Error> {
        let deadline = sys::ticks_ms() + timeout_ms;
        loop {
            if self.start < self.end {
                let n = out.len().min(self.end - self.start);
                out[..n].copy_from_slice(&self.pending[self.start..self.start + n]);
                self.start += n;
                return Ok(n);
            }
            if self.eof || out.is_empty() {
                return Ok(0);
            }
            if let Some(e) = self.error {
                return Err(e);
            }
            let left = deadline.saturating_sub(sys::ticks_ms());
            if !self.fill(left.max(1))? && sys::ticks_ms() >= deadline {
                return Err(Error::TimedOut);
            }
        }
    }

    /// Like [`read`](Self::read), but only what is already there: `Ok(None)` if
    /// nothing is.
    pub fn try_read(&mut self, out: &mut [u8]) -> Result<Option<usize>, Error> {
        if self.start == self.end && !self.eof && self.error.is_none() && !self.fill(0)? {
            return Ok(None);
        }
        self.read(out, 0).map(Some)
    }

    /// Read until end of stream, appending to `out`, for at most `timeout_ms`.
    pub fn read_to_end(&mut self, out: &mut alloc::vec::Vec<u8>, timeout_ms: u64) -> Result<(), Error> {
        let deadline = sys::ticks_ms() + timeout_ms;
        let mut buf = [0u8; DATA_MAX];
        loop {
            let left = deadline.saturating_sub(sys::ticks_ms());
            if left == 0 {
                return Err(Error::TimedOut);
            }
            match self.read(&mut buf, left)? {
                0 => return Ok(()),
                n => out.extend_from_slice(&buf[..n]),
            }
        }
    }
}

impl Drop for TcpStream {
    fn drop(&mut self) {
        sys::handle_close(self.chan).ok();
    }
}
