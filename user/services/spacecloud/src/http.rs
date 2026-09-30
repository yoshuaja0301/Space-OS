//! Just enough HTTP/1.1 for one request per connection: a request with a body out,
//! then a response head and a body that is either `content-length` bytes, chunked,
//! or everything until the peer's close_notify. Every read has the ask's deadline.

use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

use libspace::spaceabi::cloud::fail;
use libspace::sys;
use spacetls::{TlsError, TlsStream};

use crate::Fail;

/// Longest response head, and longest line inside a chunked body.
const HEAD_MAX: usize = 8 * 1024;
const LINE_MAX: usize = 256;

/// A response's status and the headers the adapter looks at.
pub struct Head {
    pub status: u16,
    pub content_type: String,
    body: Body,
}

enum Body {
    Length(usize),
    Chunked { left: usize, done: bool },
    UntilClose { done: bool },
}

pub struct Conn {
    tls: TlsStream,
    buf: Vec<u8>,
    pos: usize,
    pub deadline: u64,
}

fn left(deadline: u64) -> Result<u64, Fail> {
    match deadline.saturating_sub(sys::ticks_ms()) {
        0 => Err(Fail::new(fail::TIMEOUT, "no answer within the time allowed")),
        n => Ok(n),
    }
}

pub fn tls_fail(e: TlsError, what: &str) -> Fail {
    let code = match e {
        TlsError::TimedOut => fail::TIMEOUT,
        TlsError::Tls(_) | TlsError::NoEntropy | TlsError::NoClock => fail::TLS,
        TlsError::Truncated | TlsError::Net(_) => fail::NET,
    };
    Fail::new(code, &format!("{what}: {e}"))
}

impl Conn {
    pub fn new(tls: TlsStream, deadline: u64) -> Conn {
        Conn { tls, buf: Vec::new(), pos: 0, deadline }
    }

    pub fn send(&mut self, head: &str, body: &[u8]) -> Result<(), Fail> {
        let mut out = Vec::with_capacity(head.len() + body.len());
        out.extend_from_slice(head.as_bytes());
        out.extend_from_slice(body);
        let t = left(self.deadline)?;
        self.tls.write_all(&out, t).map_err(|e| tls_fail(e, "sending the request"))
    }

    /// More bytes from the connection. `Ok(false)` at the peer's close_notify.
    fn fill(&mut self) -> Result<bool, Fail> {
        if self.pos > 0 {
            self.buf.drain(..self.pos);
            self.pos = 0;
        }
        let mut tmp = [0u8; 2048];
        let t = left(self.deadline)?;
        match self.tls.read(&mut tmp, t) {
            Ok(0) => Ok(false),
            Ok(n) => {
                self.buf.extend_from_slice(&tmp[..n]);
                Ok(true)
            }
            Err(e) => Err(tls_fail(e, "reading the answer")),
        }
    }

    fn line(&mut self, max: usize) -> Result<String, Fail> {
        loop {
            if let Some(i) = self.buf[self.pos..].windows(2).position(|w| w == b"\r\n") {
                let s = String::from_utf8_lossy(&self.buf[self.pos..self.pos + i]).to_string();
                self.pos += i + 2;
                return Ok(s);
            }
            if self.buf.len() - self.pos > max {
                return Err(Fail::new(fail::PROTOCOL, "a line of the answer is too long"));
            }
            if !self.fill()? {
                return Err(Fail::new(fail::PROTOCOL, "the answer ended in the middle of a line"));
            }
        }
    }

    pub fn head(&mut self) -> Result<Head, Fail> {
        let status_line = self.line(HEAD_MAX)?;
        let mut parts = status_line.splitn(3, ' ');
        let version = parts.next().unwrap_or("");
        let status: u16 = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0);
        if !version.starts_with("HTTP/1.") || status < 100 {
            return Err(Fail::new(fail::PROTOCOL, &format!("not an HTTP status line: {status_line:.60}")));
        }
        let (mut length, mut chunked, mut content_type) = (None, false, String::new());
        let mut total = status_line.len();
        loop {
            let line = self.line(HEAD_MAX)?;
            if line.is_empty() {
                break;
            }
            total += line.len();
            if total > HEAD_MAX {
                return Err(Fail::new(fail::PROTOCOL, "the answer's head is too long"));
            }
            let Some((name, value)) = line.split_once(':') else { continue };
            let (name, value) = (name.trim(), value.trim());
            if name.eq_ignore_ascii_case("content-length") {
                length = value.parse::<usize>().ok();
            } else if name.eq_ignore_ascii_case("transfer-encoding") {
                chunked = value.eq_ignore_ascii_case("chunked");
            } else if name.eq_ignore_ascii_case("content-type") {
                content_type = value.to_ascii_lowercase();
            }
        }
        let body = match (chunked, length) {
            (true, _) => Body::Chunked { left: 0, done: false },
            (false, Some(n)) => Body::Length(n),
            (false, None) => Body::UntilClose { done: false },
        };
        Ok(Head { status, content_type, body })
    }

    /// Append the next piece of the body to `out`; `Ok(false)` once it has ended.
    pub fn body(&mut self, head: &mut Head, out: &mut Vec<u8>) -> Result<bool, Fail> {
        match &mut head.body {
            Body::Length(n) => {
                if *n == 0 {
                    return Ok(false);
                }
                let k = self.take(*n, out)?;
                if k == 0 {
                    return Err(Fail::new(fail::PROTOCOL, "the answer ended before its length"));
                }
                *n -= k;
                Ok(true)
            }
            Body::Chunked { left, done } => {
                if *done {
                    return Ok(false);
                }
                if *left == 0 {
                    let size_line = self.line(LINE_MAX)?;
                    let size = size_line.split(';').next().unwrap_or("").trim();
                    let size = usize::from_str_radix(size, 16)
                        .map_err(|_| Fail::new(fail::PROTOCOL, "a chunk size that is not hexadecimal"))?;
                    if size == 0 {
                        // Trailers, then the empty line that ends the body.
                        while !self.line(LINE_MAX)?.is_empty() {}
                        *done = true;
                        return Ok(false);
                    }
                    *left = size;
                }
                let k = self.take(*left, out)?;
                if k == 0 {
                    return Err(Fail::new(fail::PROTOCOL, "the answer ended inside a chunk"));
                }
                *left -= k;
                if *left == 0 && !self.line(LINE_MAX)?.is_empty() {
                    return Err(Fail::new(fail::PROTOCOL, "a chunk longer than its size"));
                }
                Ok(true)
            }
            Body::UntilClose { done } => {
                if *done {
                    return Ok(false);
                }
                let k = self.take(usize::MAX, out)?;
                if k == 0 {
                    *done = true;
                }
                Ok(k > 0)
            }
        }
    }

    /// Up to `n` buffered or freshly read bytes onto `out`; 0 at the peer's end.
    fn take(&mut self, n: usize, out: &mut Vec<u8>) -> Result<usize, Fail> {
        if self.pos == self.buf.len() && !self.fill()? {
            return Ok(0);
        }
        let k = n.min(self.buf.len() - self.pos);
        out.extend_from_slice(&self.buf[self.pos..self.pos + k]);
        self.pos += k;
        Ok(k)
    }

    /// End a connection whose answer arrived whole: close_notify, then the peer's.
    pub fn close(self) {
        let _ = self.tls.close(500);
    }

    /// Read what is left of a short body (an error's JSON), at most `max` bytes.
    pub fn small_body(&mut self, head: &mut Head, max: usize) -> Result<Vec<u8>, Fail> {
        let mut out = Vec::new();
        while out.len() < max && self.body(head, &mut out)? {}
        out.truncate(max);
        Ok(out)
    }
}
