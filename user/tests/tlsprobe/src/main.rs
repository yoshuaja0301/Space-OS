//! Runs one TLS case against a lab service and reports how it ended.
//!
//! The parent sends the case and port (with a root handle narrowed to reading
//! files, for the lab authority's certificate), then a network session. The answer
//! is one line: `ok <what happened>` or `err <kind> <why>`, where `kind` is
//! [`spacetls::TlsError::kind`]. TLS runs in this process, not in the parent, so a
//! TLS failure of any sort ends here.
#![no_std]
#![no_main]

extern crate alloc;

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use libspace::net::Session;
use libspace::{Handle, handle, println, sys};
use spacetls::{TlsConfig, TlsError, TlsStream};

const HOST: &str = "tls.lab.test";
const CA_PATH: &str = "/spaceos/tls/labca.der";
/// TLS buffers, certificate parsing and the handshake's keys.
const HEAP_PAGES: usize = 128;

fn byte(i: usize) -> u8 {
    (i as u32).wrapping_mul(2_654_435_761).rotate_left(11) as u8 ^ (i >> 7) as u8
}

fn read_file(root: Handle, path: &str) -> Result<Vec<u8>, String> {
    let f = sys::fs_open(root, path).map_err(|e| format!("open {path}: {e}"))?;
    let size = sys::fs_stat(f).map_err(|e| format!("stat {path}: {e}"))?.size as usize;
    let mut out = alloc::vec![0u8; size];
    let mut at = 0;
    while at < size {
        match sys::fs_read(f, at as u64, &mut out[at..]) {
            Ok(0) => break,
            Ok(n) => at += n,
            Err(e) => {
                sys::handle_close(f).ok();
                return Err(format!("read {path}: {e}"));
            }
        }
    }
    sys::handle_close(f).ok();
    out.truncate(at);
    Ok(out)
}

fn err(e: TlsError, context: &str) -> String {
    format!("err {} {context}{e}", e.kind())
}

/// Handshake, then 16 KiB through the echo service and back, then close_notify.
fn echo(s: &Session, port: u16, cfg: &TlsConfig) -> String {
    const LEN: usize = 16 * 1024;
    let mut tls = match TlsStream::connect(s, HOST, port, cfg, 10_000) {
        Ok(t) => t,
        Err(e) => return err(e, ""),
    };
    let (mut sent, mut got) = (0usize, 0usize);
    let mut out = [0u8; 1024];
    let mut buf = [0u8; 1024];
    while got < LEN {
        if sent < LEN {
            let n = (LEN - sent).min(out.len());
            for (k, b) in out[..n].iter_mut().enumerate() {
                *b = byte(sent + k);
            }
            if let Err(e) = tls.write_all(&out[..n], 5000) {
                return err(e, &format!("writing after {sent} bytes: "));
            }
            sent += n;
        }
        // Take what has come back; wait only once everything is sent.
        let wait = if sent < LEN { 1 } else { 5000 };
        match tls.read(&mut buf, wait) {
            Ok(0) => return format!("err eof the echo ended after {got} of {LEN} bytes"),
            Ok(n) => {
                if let Some(k) = buf[..n].iter().enumerate().position(|(k, b)| *b != byte(got + k)) {
                    return format!("err altered the echo differs from byte {} on", got + k);
                }
                got += n;
            }
            Err(TlsError::TimedOut) if sent < LEN => {}
            Err(e) => return err(e, &format!("reading after {got} bytes: ")),
        }
    }
    let suite = tls.suite.map_or(String::from("no suite"), |c| format!("{c:?}"));
    let (tcp_ms, handshake_ms) = (tls.tcp_ms, tls.handshake_ms);
    if let Err(e) = tls.close(3000) {
        return err(e, "closing: ");
    }
    format!(
        "ok {suite}: TCP in {tcp_ms} ms, handshake in {handshake_ms} ms, {LEN} bytes echoed intact, \
         closed with close_notify"
    )
}

/// A handshake that must be refused: the answer names why it was.
fn refuse(s: &Session, port: u16, cfg: &TlsConfig) -> String {
    match TlsStream::connect(s, HOST, port, cfg, 10_000) {
        Ok(t) => format!("ok connected ({:?}): the certificate was accepted", t.suite),
        Err(e) => err(e, ""),
    }
}

/// Handshake, one message, and the altered answer must be caught.
fn tamper(s: &Session, port: u16, cfg: &TlsConfig) -> String {
    let mut tls = match TlsStream::connect(s, HOST, port, cfg, 10_000) {
        Ok(t) => t,
        Err(e) => return err(e, "handshake: "),
    };
    if let Err(e) = tls.write_all(b"is this answer genuine?", 3000) {
        return err(e, "writing: ");
    }
    let mut buf = [0u8; 256];
    match tls.read(&mut buf, 5000) {
        Ok(n) => format!("ok read {n} bytes of an answer that was altered on the way"),
        Err(e) => err(e, ""),
    }
}

/// Handshake, one message, a partial answer, then the end without close_notify.
fn truncate(s: &Session, port: u16, cfg: &TlsConfig) -> String {
    let mut tls = match TlsStream::connect(s, HOST, port, cfg, 10_000) {
        Ok(t) => t,
        Err(e) => return err(e, "handshake: "),
    };
    if let Err(e) = tls.write_all(b"tell me everything", 3000) {
        return err(e, "writing: ");
    }
    let mut buf = [0u8; 256];
    let mut got = 0;
    loop {
        match tls.read(&mut buf, 5000) {
            Ok(0) => return format!("ok the answer ended cleanly after {got} bytes"),
            Ok(n) => got += n,
            Err(e) => return err(e, &format!("after {got} bytes of answer: ")),
        }
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn space_main() -> i32 {
    libspace::heap::set_pages(HEAP_PAGES);
    let mut buf = [0u8; 64];
    let Ok((n, Some(root))) = sys::recv(handle::BOOTSTRAP, &mut buf, false) else {
        println!("[tlsprobe] no case from the parent");
        return 1;
    };
    let text = String::from(core::str::from_utf8(&buf[..n]).unwrap_or(""));
    let Ok((_, Some(session))) = sys::recv(handle::BOOTSTRAP, &mut buf, false) else {
        println!("[tlsprobe] no network session from the parent");
        return 1;
    };
    let mut words = text.split(' ');
    let case = words.next().unwrap_or("");
    let port: u16 = words.next().and_then(|p| p.parse().ok()).unwrap_or(0);
    let answer = match read_file(root, CA_PATH) {
        Err(e) => format!("err setup {e}"),
        Ok(ca) => match TlsConfig::with_roots(&[&ca]) {
            Err(e) => err(e, "trust anchor: "),
            Ok(cfg) => {
                let s = Session::new(session);
                match case {
                    "echo" => echo(&s, port, &cfg),
                    "refuse" => refuse(&s, port, &cfg),
                    "tamper" => tamper(&s, port, &cfg),
                    "truncate" => truncate(&s, port, &cfg),
                    _ => format!("err setup unknown case {case:?}"),
                }
            }
        },
    };
    println!("[tlsprobe] {case} on port {port}: {answer}");
    let msg = answer.as_bytes();
    let _ = sys::send(handle::BOOTSTRAP, &msg[..msg.len().min(libspace::spaceabi::syscall::MSG_MAX)], None);
    0
}
