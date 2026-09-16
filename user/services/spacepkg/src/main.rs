//! `spacepkg` – the package service (requirement P01).
//!
//! It installs authenticated packages and can go back to the version that was
//! active before. Three things make that more than a file copy:
//!
//! * A package is opened by [`spaceabi::pkg::open`], which checks shape, then
//!   authentication, then content, and says which of the three failed. An operator
//!   can tell a corrupted download from a forged one.
//! * A refused package changes nothing. The active version, the history and the
//!   payload are only touched after the package has been fully verified.
//! * Rollback is a history, not a flag: the previous payload is still held, so going
//!   back does not mean re-reading a file that may itself have been replaced.
//!
//! What it is not: the store lives in memory, because the volume is mounted
//! read-only (ADR-0007), and authentication is a MAC rather than a public-key
//! signature (ADR-0014).
#![no_std]
#![no_main]

extern crate alloc;

use alloc::string::String;
use alloc::vec::Vec;

use libspace::spaceabi::error::Error;
use libspace::spaceabi::pkg::{
    self, CHUNK_MAX, PAYLOAD_MAX, PkgReply, PkgRequest, RELEASE_KEY, Reject, reject_code, req,
};
use libspace::{Handle, handle, println, sys};

const ABI_VERSION: u32 = 0;
/// Versions kept so a rollback has somewhere to go.
const HISTORY_MAX: usize = 4;
/// Largest package image the service will read.
const IMAGE_MAX: usize = pkg::PAYLOAD_MAX as usize + 1024;

/// A package that has been proven: name, version, payload digest and payload.
type Verified = (String, u32, [u8; 32], Vec<u8>);

struct Installed {
    name: String,
    version: u32,
    digest: [u8; 32],
    payload: Vec<u8>,
}

struct Service {
    channel: Handle,
    root: Option<Handle>,
    /// Oldest first; the last entry is active.
    history: Vec<Installed>,
}

fn as_bytes<T>(v: &T) -> &[u8] {
    // SAFETY: `T` is a `repr(C)` plain-data message.
    unsafe { core::slice::from_raw_parts(v as *const T as *const u8, core::mem::size_of::<T>()) }
}

impl Service {
    fn new(channel: Handle) -> Service {
        Service { channel, root: None, history: Vec::new() }
    }

    fn reply(&self, r: &PkgReply) {
        let _ = sys::send(self.channel, as_bytes(r), None);
    }

    fn reply_err(&self, e: Error, reject: i32) {
        let mut r = self.status_reply();
        r.status = -(e as u32 as i32);
        r.reject = reject;
        self.reply(&r);
    }

    /// Every reply carries the current state, so a caller never has to ask twice to
    /// find out whether a refusal changed anything.
    fn status_reply(&self) -> PkgReply {
        let mut r = PkgReply { history: self.history.len() as u32, ..Default::default() };
        if let Some(active) = self.history.last() {
            r.version = active.version;
            r.len = active.payload.len() as u32;
            r.digest = active.digest;
            r.set_name(&active.name);
        }
        if self.history.len() >= 2 {
            r.previous = self.history[self.history.len() - 2].version;
        }
        r
    }

    fn read_image(&self, path: &str) -> Result<Vec<u8>, Error> {
        let root = self.root.ok_or(Error::Denied)?;
        let f = sys::fs_open(root, path)?;
        let stat = sys::fs_stat(f)?;
        if stat.size as usize > IMAGE_MAX {
            sys::handle_close(f).ok();
            return Err(Error::MsgSize);
        }
        let mut buf = Vec::new();
        buf.try_reserve(stat.size as usize).map_err(|_| Error::NoMemory)?;
        buf.resize(stat.size as usize, 0);
        let mut done = 0usize;
        while done < buf.len() {
            let n = sys::fs_read(f, done as u64, &mut buf[done..])?;
            if n == 0 {
                break;
            }
            done += n;
        }
        sys::handle_close(f).ok();
        buf.truncate(done);
        Ok(buf)
    }

    /// Verify a package. Returns what the header claims once it has been proven.
    fn verify(&self, path: &str) -> Result<Verified, (Error, i32)> {
        let image = self.read_image(path).map_err(|e| (e, reject_code::NONE))?;
        let (header, payload) = pkg::open(&image, RELEASE_KEY).map_err(|r: Reject| {
            println!("[pkg] {path}: refused, {} ({:?})", r.message(), r);
            (Error::Denied, r.code())
        })?;
        let mut name = String::new();
        name.push_str(header.name());
        let mut owned = Vec::new();
        owned.try_reserve(payload.len()).map_err(|_| (Error::NoMemory, reject_code::NONE))?;
        owned.extend_from_slice(payload);
        Ok((name, header.version, header.payload_digest, owned))
    }

    fn install(&mut self, path: &str) {
        let (name, version, digest, payload) = match self.verify(path) {
            Ok(v) => v,
            Err((e, reject)) => return self.reply_err(e, reject),
        };
        if let Some(active) = self.history.last() {
            if !active.name.eq_ignore_ascii_case(&name) {
                println!("[pkg] {path}: refused, package is '{name}' but '{}' is installed", active.name);
                return self.reply_err(Error::Invalid, reject_code::NONE);
            }
            if version <= active.version {
                // A package that verifies is still not one to install over a newer
                // one: going backwards is what `ROLLBACK` is for, and it keeps the
                // history honest.
                println!(
                    "[pkg] {path}: refused, version {version} is not newer than the installed {}",
                    active.version
                );
                return self.reply_err(Error::Invalid, reject_code::NONE);
            }
        }
        if self.history.len() == HISTORY_MAX {
            self.history.remove(0);
        }
        if self.history.try_reserve(1).is_err() {
            return self.reply_err(Error::NoMemory, reject_code::NONE);
        }
        self.history.push(Installed { name, version, digest, payload });
        let active = self.history.last().expect("just pushed");
        println!(
            "[pkg] installed '{}' version {} ({} bytes), {} version(s) held",
            active.name,
            active.version,
            active.payload.len(),
            self.history.len()
        );
        let r = self.status_reply();
        self.reply(&r);
    }

    fn verify_only(&self, path: &str) {
        match self.verify(path) {
            Ok((name, version, _, payload)) => {
                println!("[pkg] {path}: verified '{name}' version {version} ({} bytes)", payload.len());
                let mut r = self.status_reply();
                // Report what the package says, not what is installed.
                r.version = version;
                r.len = payload.len() as u32;
                r.set_name(&name);
                self.reply(&r);
            }
            Err((e, reject)) => self.reply_err(e, reject),
        }
    }

    fn rollback(&mut self) {
        if self.history.len() < 2 {
            println!("[pkg] rollback refused: no earlier version is held");
            return self.reply_err(Error::NotFound, reject_code::NONE);
        }
        let gone = self.history.pop().expect("checked");
        let active = self.history.last().expect("checked");
        println!("[pkg] rolled back '{}' from version {} to {}", active.name, gone.version, active.version);
        let r = self.status_reply();
        self.reply(&r);
    }

    fn read(&self, offset: u32) {
        let Some(active) = self.history.last() else {
            return self.reply_err(Error::NotFound, reject_code::NONE);
        };
        let start = (offset as usize).min(active.payload.len());
        let end = (start + CHUNK_MAX).min(active.payload.len());
        let mut r = self.status_reply();
        r.len = (end - start) as u32;
        r.data[..end - start].copy_from_slice(&active.payload[start..end]);
        self.reply(&r);
    }

    /// Handle one request. Returns false when the service should exit.
    fn handle(&mut self, r: &PkgRequest, transferred: Option<Handle>) -> bool {
        if r.kind != req::HELLO && self.root.is_none() {
            if let Some(h) = transferred {
                sys::handle_close(h).ok();
            }
            self.reply_err(Error::Denied, reject_code::NONE);
            return true;
        }
        match r.kind {
            req::HELLO => match transferred {
                Some(h) if r.abi_version == ABI_VERSION => {
                    if let Some(old) = self.root.replace(h) {
                        sys::handle_close(old).ok();
                    }
                    println!(
                        "[pkg] operator attached, ABI v{ABI_VERSION}, payload limit {} KiB",
                        PAYLOAD_MAX / 1024
                    );
                    let reply = self.status_reply();
                    self.reply(&reply);
                }
                other => {
                    if let Some(h) = other {
                        sys::handle_close(h).ok();
                    }
                    self.reply_err(Error::Denied, reject_code::NONE);
                }
            },
            req::INSTALL => self.install(r.path()),
            req::VERIFY => self.verify_only(r.path()),
            req::ROLLBACK => self.rollback(),
            req::STATUS => {
                let reply = self.status_reply();
                self.reply(&reply);
            }
            req::READ => self.read(r.offset),
            req::QUIT => {
                let reply = self.status_reply();
                self.reply(&reply);
                return false;
            }
            _ => self.reply_err(Error::NoSys, reject_code::NONE),
        }
        true
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn space_main() -> i32 {
    println!("[pkg] Space OS package service, ABI v{ABI_VERSION}");
    let mut svc = Service::new(handle::BOOTSTRAP);
    let mut buf = [0u8; core::mem::size_of::<PkgRequest>()];
    loop {
        match sys::recv(handle::BOOTSTRAP, &mut buf, false) {
            Ok((n, transferred)) if n == buf.len() => {
                // SAFETY: the operator sends exactly one `PkgRequest`.
                let r: PkgRequest = unsafe { core::ptr::read_unaligned(buf.as_ptr() as *const _) };
                if !svc.handle(&r, transferred) {
                    break;
                }
            }
            Ok((n, transferred)) => {
                if let Some(h) = transferred {
                    sys::handle_close(h).ok();
                }
                println!("[pkg] malformed request of {n} bytes");
                svc.reply_err(Error::MsgSize, reject_code::NONE);
            }
            Err(Error::PeerClosed) => {
                println!("[pkg] operator disconnected");
                break;
            }
            Err(e) => {
                println!("[pkg] receive failed: {e}");
                break;
            }
        }
    }
    if let Some(h) = svc.root.take() {
        sys::handle_close(h).ok();
    }
    println!("[pkg] closing with {} version(s) held", svc.history.len());
    0
}
