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

use alloc::string::{String, ToString};
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
    /// Said once: the store could not be written, so installs are for this boot only.
    store_warned: bool,
}

fn as_bytes<T>(v: &T) -> &[u8] {
    // SAFETY: `T` is a `repr(C)` plain-data message.
    unsafe { core::slice::from_raw_parts(v as *const T as *const u8, core::mem::size_of::<T>()) }
}

impl Service {
    fn new(channel: Handle) -> Service {
        Service { channel, root: None, history: Vec::new(), store_warned: false }
    }

    /// Encode the whole history into the store format.
    fn encode_store(&self) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&pkg::STORE_MAGIC);
        out.extend_from_slice(&pkg::STORE_FORMAT.to_le_bytes());
        out.extend_from_slice(&(self.history.len() as u32).to_le_bytes());
        for e in &self.history {
            out.extend_from_slice(&(e.name.len() as u32).to_le_bytes());
            out.extend_from_slice(e.name.as_bytes());
            out.extend_from_slice(&e.version.to_le_bytes());
            out.extend_from_slice(&e.digest);
            out.extend_from_slice(&(e.payload.len() as u32).to_le_bytes());
            out.extend_from_slice(&e.payload);
        }
        out
    }

    /// Read the history back. Every length is checked against what is left in the
    /// buffer: a store that has been truncated or tampered with is treated as no
    /// store at all, never as a reason to panic.
    fn decode_store(raw: &[u8]) -> Option<Vec<Installed>> {
        let mut at = 0usize;
        let mut take = |n: usize| -> Option<&[u8]> {
            let end = at.checked_add(n)?;
            if end > raw.len() {
                return None;
            }
            let s = &raw[at..end];
            at = end;
            Some(s)
        };
        if take(8)? != pkg::STORE_MAGIC {
            return None;
        }
        if u32::from_le_bytes(take(4)?.try_into().ok()?) != pkg::STORE_FORMAT {
            return None;
        }
        let count = u32::from_le_bytes(take(4)?.try_into().ok()?) as usize;
        if count > HISTORY_MAX {
            return None;
        }
        let mut out = Vec::new();
        out.try_reserve(count).ok()?;
        for _ in 0..count {
            let name_len = u32::from_le_bytes(take(4)?.try_into().ok()?) as usize;
            if name_len > pkg::NAME_MAX {
                return None;
            }
            let name = core::str::from_utf8(take(name_len)?).ok()?.to_string();
            let version = u32::from_le_bytes(take(4)?.try_into().ok()?);
            let digest: [u8; 32] = take(32)?.try_into().ok()?;
            let payload_len = u32::from_le_bytes(take(4)?.try_into().ok()?) as usize;
            if payload_len as u64 > pkg::PAYLOAD_MAX {
                return None;
            }
            let payload = take(payload_len)?.to_vec();
            out.push(Installed { name, version, digest, payload });
        }
        Some(out)
    }

    /// Write the store, if this operator handed over the right to write it.
    ///
    /// A volume that cannot be written is not an error here -- the diskless machine
    /// is a supported configuration -- but it is said once, so nobody reads "installed"
    /// as "installed for good".
    fn save(&mut self) {
        let Some(root) = self.root else { return };
        let raw = self.encode_store();
        let r = sys::fs_create(root, pkg::STORE_PATH).and_then(|h| {
            let w = sys::fs_write(h, 0, &raw);
            sys::handle_close(h).ok();
            w
        });
        match r {
            Ok(_) => {}
            Err(e) => {
                if !self.store_warned {
                    self.store_warned = true;
                    println!("[pkg] store not kept on disk ({e}); installs last only for this boot");
                }
            }
        }
    }

    /// Load what an earlier process installed.
    fn load(&mut self) {
        let Some(root) = self.root else { return };
        let file = match sys::fs_open(root, pkg::STORE_PATH) {
            Ok(h) => h,
            // Nothing installed yet is the ordinary case, not a failure.
            Err(_) => return,
        };
        let mut raw = Vec::new();
        let mut buf = [0u8; 512];
        let mut off = 0u64;
        loop {
            match sys::fs_read(file, off, &mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    if raw.len() + n > IMAGE_MAX * HISTORY_MAX {
                        raw.clear();
                        break;
                    }
                    raw.extend_from_slice(&buf[..n]);
                    off += n as u64;
                }
                Err(_) => {
                    raw.clear();
                    break;
                }
            }
        }
        sys::handle_close(file).ok();
        match Self::decode_store(&raw) {
            Some(history) if !history.is_empty() => {
                let active = history.last().expect("not empty");
                println!(
                    "[pkg] store loaded from disk: '{}' version {}, {} version(s) held",
                    active.name,
                    active.version,
                    history.len()
                );
                self.history = history;
            }
            Some(_) => {}
            None => {
                if !raw.is_empty() {
                    println!("[pkg] store on disk is not readable; starting empty");
                }
            }
        }
    }

    /// Forget everything, on disk as well as in memory.
    fn reset(&mut self) {
        self.history.clear();
        self.save();
        println!("[pkg] store cleared");
        let r = self.status_reply();
        self.reply(&r);
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
        self.save();
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
        self.save();
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
                    // Whatever an earlier process installed is still installed.
                    self.load();
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
            req::RESET => self.reset(),
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
