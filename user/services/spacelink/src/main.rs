//! `spacelink` – index, revocation and context bundles (requirements L01-L03).
//!
//! The service indexes plain-text documents from one corpus directory, ranks their
//! chunks against a query, and assembles a context bundle under a byte budget.
//!
//! Two properties are what make it worth having rather than a search box:
//!
//! * **Provenance is verifiable.** Every chunk comes back with its source path, its
//!   byte range and the SHA-256 of exactly those bytes. A caller that holds the file
//!   capability can re-read the range and check the digest itself, so "this is where
//!   the context came from" is not something the service is trusted about.
//! * **Revocation is a contract, not a cleanup.** A revoked document disappears from
//!   every later query and bundle, and re-indexing the corpus does not bring it back.
//!
//! Ranking is lexical and deliberately simple (see ADR-0013): term occurrences per
//! chunk, with ties broken by position so results are stable between runs.
#![no_std]
#![no_main]

extern crate alloc;

use alloc::string::String;
use alloc::vec::Vec;

use libspace::sha256;
use libspace::spaceabi::error::Error;
use libspace::spaceabi::link::{
    self as link_abi, ABI_VERSION, LinkReply, LinkRequest, MAX_BUNDLE, MAX_CHUNKS, MAX_DOCS, PATH_MAX,
    TEXT_MAX, req,
};
use libspace::spaceabi::syscall::{DIR_ENTRIES_MAX, DirEntry};
use libspace::{Handle, handle, println, sys};

/// Bytes per chunk. Small enough that several fit a bundle budget, large enough to
/// hold a few lines of prose.
const CHUNK_BYTES: usize = 192;
/// Largest document the indexer will read.
const DOC_MAX: usize = 8 * 1024;

struct Doc {
    path: String,
    revoked: bool,
}

struct Chunk {
    doc: u16,
    offset: u32,
    len: u32,
    digest: [u8; 32],
    text: Vec<u8>,
}

struct BundleEntry {
    chunk: usize,
    score: u32,
}

struct Link {
    channel: Handle,
    root: Option<Handle>,
    docs: Vec<Doc>,
    chunks: Vec<Chunk>,
    /// Paths revoked so far. Kept separately from `docs` so that re-indexing the
    /// corpus cannot resurrect a revoked document.
    revoked: Vec<String>,
    /// Said once: the list could not be written, so revocations end with this boot.
    store_warned: bool,
    bundle: Vec<BundleEntry>,
    bundle_bytes: u32,
}

fn as_bytes<T>(v: &T) -> &[u8] {
    // SAFETY: `T` is a `repr(C)` plain-data message.
    unsafe { core::slice::from_raw_parts(v as *const T as *const u8, core::mem::size_of::<T>()) }
}

/// Split a query into lower-case terms.
fn terms(query: &str) -> impl Iterator<Item = &str> {
    query.split(|c: char| !c.is_ascii_alphanumeric()).filter(|t| !t.is_empty())
}

/// Occurrences of every query term in `text`, case insensitive. Zero means the
/// chunk is not a result at all.
fn score(text: &[u8], query: &str) -> u32 {
    let mut total = 0u32;
    for term in terms(query) {
        let t = term.as_bytes();
        if t.is_empty() || t.len() > text.len() {
            continue;
        }
        for start in 0..=text.len() - t.len() {
            if text[start..start + t.len()].eq_ignore_ascii_case(t) {
                total = total.saturating_add(1);
            }
        }
    }
    total
}

impl Link {
    fn new(channel: Handle) -> Link {
        Link {
            channel,
            root: None,
            docs: Vec::new(),
            chunks: Vec::new(),
            revoked: Vec::new(),
            store_warned: false,
            bundle: Vec::new(),
            bundle_bytes: 0,
        }
    }

    fn reply(&self, r: &LinkReply) {
        let _ = sys::send(self.channel, as_bytes(r), None);
    }

    fn reply_err(&self, e: Error) {
        self.reply(&LinkReply { status: -(e as u32 as i32), ..Default::default() });
    }

    fn is_revoked(&self, path: &str) -> bool {
        self.revoked.iter().any(|p| p.eq_ignore_ascii_case(path))
    }

    fn read_doc(&self, path: &str) -> Result<Vec<u8>, Error> {
        let root = self.root.ok_or(Error::Denied)?;
        let f = sys::fs_open(root, path)?;
        let stat = sys::fs_stat(f)?;
        if stat.size as usize > DOC_MAX {
            sys::handle_close(f).ok();
            return Err(Error::MsgSize);
        }
        let mut buf = Vec::new();
        buf.try_reserve(stat.size as usize).map_err(|_| Error::NoMemory)?;
        buf.resize(stat.size as usize, 0);
        let n = sys::fs_read(f, 0, &mut buf)?;
        sys::handle_close(f).ok();
        buf.truncate(n);
        Ok(buf)
    }

    /// Index every regular file directly inside `dir`. Re-indexing replaces the
    /// index but keeps the revocation list.
    fn index(&mut self, dir: &str) -> Result<(u32, u32), Error> {
        let root = self.root.ok_or(Error::Denied)?;
        let mut entries = [DirEntry::default(); DIR_ENTRIES_MAX];
        let n = sys::fs_list(root, dir, &mut entries)?;
        self.docs.clear();
        self.chunks.clear();
        self.bundle.clear();
        let mut skipped = 0u32;
        for e in entries.iter().take(n) {
            if e.is_dir != 0 || e.name() == "." || e.name() == ".." {
                continue;
            }
            if self.docs.len() >= MAX_DOCS {
                skipped += 1;
                continue;
            }
            let mut path = String::new();
            path.push_str(dir);
            if !path.ends_with('/') {
                path.push('/');
            }
            path.push_str(e.name());
            let revoked = self.is_revoked(&path);
            let body = if revoked { Vec::new() } else { self.read_doc(&path)? };
            let doc = self.docs.len() as u16;
            self.docs.push(Doc { path, revoked });
            if revoked {
                // A revoked document is listed as known, and indexed as nothing.
                continue;
            }
            let mut offset = 0usize;
            while offset < body.len() {
                if self.chunks.len() >= MAX_CHUNKS {
                    skipped += 1;
                    break;
                }
                // Prefer to break on a line boundary so a chunk is readable prose.
                let hard = (offset + CHUNK_BYTES).min(body.len());
                let end = body[offset..hard]
                    .iter()
                    .rposition(|b| *b == b'\n')
                    .map(|i| offset + i + 1)
                    .filter(|e| *e > offset && hard < body.len())
                    .unwrap_or(hard);
                let slice = &body[offset..end];
                let mut text = Vec::new();
                text.try_reserve(slice.len()).map_err(|_| Error::NoMemory)?;
                text.extend_from_slice(slice);
                self.chunks.push(Chunk {
                    doc,
                    offset: offset as u32,
                    len: slice.len() as u32,
                    digest: sha256::digest(slice),
                    text,
                });
                offset = end;
            }
        }
        println!(
            "[link] indexed {} document(s) into {} chunk(s) from {dir}{}",
            self.docs.iter().filter(|d| !d.revoked).count(),
            self.chunks.len(),
            if skipped > 0 { " (corpus truncated)" } else { "" }
        );
        Ok((self.docs.len() as u32, self.chunks.len() as u32))
    }

    /// Chunks that match `query`, best first. Revoked documents never appear.
    fn ranked(&self, query: &str) -> Vec<(usize, u32)> {
        let mut hits: Vec<(usize, u32)> = Vec::new();
        for (i, c) in self.chunks.iter().enumerate() {
            if self.docs[c.doc as usize].revoked {
                continue;
            }
            let s = score(&c.text, query);
            if s > 0 {
                hits.push((i, s));
            }
        }
        // Stable order: score first, then the chunk's position in the corpus, so the
        // same corpus and query always produce the same bundle.
        hits.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        hits
    }

    fn fill(&self, reply: &mut LinkReply, chunk: usize, score: u32) {
        let c = &self.chunks[chunk];
        reply.score = score;
        reply.offset = c.offset;
        reply.len = c.len;
        reply.digest = c.digest;
        reply.set_path(&self.docs[c.doc as usize].path);
        reply.set_text(&c.text[..c.text.len().min(TEXT_MAX)]);
    }

    fn query(&self, query: &str, index: u32) -> LinkReply {
        let hits = self.ranked(query);
        let mut reply = LinkReply { total: hits.len() as u32, ..Default::default() };
        match hits.get(index as usize) {
            Some(&(chunk, s)) => self.fill(&mut reply, chunk, s),
            None => reply.status = -(Error::NotFound as u32 as i32),
        }
        reply
    }

    /// Assemble a bundle: the best chunks that fit the budget, in ranked order.
    fn build_bundle(&mut self, query: &str, budget: u32) -> LinkReply {
        let hits = self.ranked(query);
        self.bundle.clear();
        self.bundle_bytes = 0;
        for (chunk, s) in hits {
            if self.bundle.len() >= MAX_BUNDLE {
                break;
            }
            let len = self.chunks[chunk].len;
            if self.bundle_bytes + len > budget {
                continue;
            }
            self.bundle_bytes += len;
            self.bundle.push(BundleEntry { chunk, score: s });
        }
        // The bundle digest chains the chunk digests in order, so two bundles are
        // identical exactly when they carry the same chunks in the same order.
        let mut h = sha256::Sha256::new();
        for e in &self.bundle {
            h.update(&self.chunks[e.chunk].digest);
        }
        println!(
            "[link] bundle for {query:?}: {} chunk(s), {} byte(s) of {budget} budget",
            self.bundle.len(),
            self.bundle_bytes
        );
        LinkReply {
            total: self.bundle.len() as u32,
            value: self.bundle_bytes,
            digest: h.finish(),
            ..Default::default()
        }
    }

    fn bundle_entry(&self, index: u32) -> LinkReply {
        let mut reply = LinkReply { total: self.bundle.len() as u32, ..Default::default() };
        match self.bundle.get(index as usize) {
            Some(e) => self.fill(&mut reply, e.chunk, e.score),
            None => reply.status = -(Error::NotFound as u32 as i32),
        }
        reply
    }

    /// Write the revocation list, one path per line.
    ///
    /// A revocation that only lives in this process is a promise that ends when the
    /// process does, which is not what "revoked" is supposed to mean.
    fn save_revoked(&mut self) {
        let Some(root) = self.root else { return };
        let mut text = String::new();
        for p in &self.revoked {
            text.push_str(p);
            text.push('\n');
        }
        let r = sys::fs_create(root, link_abi::REVOKED_PATH).and_then(|h| {
            let w = sys::fs_write(h, 0, text.as_bytes());
            sys::handle_close(h).ok();
            w
        });
        if let (Err(e), false) = (r, self.store_warned) {
            self.store_warned = true;
            println!("[link] revocations not kept on disk ({e}); they last only for this boot");
        }
    }

    /// Read back what an earlier process revoked.
    fn load_revoked(&mut self) {
        let Some(root) = self.root else { return };
        let Ok(file) = sys::fs_open(root, link_abi::REVOKED_PATH) else { return };
        let mut text = String::new();
        let mut buf = [0u8; 256];
        let mut off = 0u64;
        loop {
            match sys::fs_read(file, off, &mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    match core::str::from_utf8(&buf[..n]) {
                        Ok(part) => text.push_str(part),
                        Err(_) => {
                            text.clear();
                            break;
                        }
                    }
                    off += n as u64;
                    if text.len() > MAX_DOCS * (PATH_MAX + 1) {
                        text.clear();
                        break;
                    }
                }
                Err(_) => {
                    text.clear();
                    break;
                }
            }
        }
        sys::handle_close(file).ok();
        for line in text.lines() {
            let line = line.trim();
            // A line this service could never have written is not one to trust.
            if line.is_empty() || line.len() > PATH_MAX || !line.is_ascii() {
                continue;
            }
            if self.is_revoked(line) || self.revoked.len() >= MAX_DOCS {
                continue;
            }
            if self.revoked.try_reserve(1).is_err() {
                break;
            }
            let mut p = String::new();
            p.push_str(line);
            self.revoked.push(p);
        }
        if !self.revoked.is_empty() {
            println!("[link] {} revocation(s) loaded from disk", self.revoked.len());
        }
    }

    /// Drop every revocation, on disk as well as in memory.
    fn forget(&mut self) -> u32 {
        self.revoked.clear();
        self.save_revoked();
        println!("[link] revocation list cleared");
        0
    }

    fn revoke(&mut self, path: &str) -> Result<u32, Error> {
        if path.is_empty() {
            return Err(Error::Invalid);
        }
        if !self.is_revoked(path) {
            let mut p = String::new();
            p.push_str(path);
            self.revoked.try_reserve(1).map_err(|_| Error::NoMemory)?;
            self.revoked.push(p);
        }
        let mut hit = false;
        for d in self.docs.iter_mut() {
            if d.path.eq_ignore_ascii_case(path) {
                d.revoked = true;
                hit = true;
            }
        }
        // Drop the revoked document's chunks outright: a revoked document should not
        // be sitting in memory waiting for a filter to remember to exclude it.
        let docs = &self.docs;
        self.chunks.retain(|c| !docs[c.doc as usize].revoked);
        self.bundle.clear();
        println!(
            "[link] revoked {path}{}; {} chunk(s) remain",
            if hit { "" } else { " (not in the current index)" },
            self.chunks.len()
        );
        self.save_revoked();
        Ok(self.revoked.len() as u32)
    }

    /// Handle one request. Returns false when the service should exit.
    fn handle(&mut self, r: &LinkRequest, transferred: Option<Handle>) -> bool {
        if r.kind != req::HELLO && self.root.is_none() {
            if let Some(h) = transferred {
                sys::handle_close(h).ok();
            }
            self.reply_err(Error::Denied);
            return true;
        }
        match r.kind {
            req::HELLO => match transferred {
                Some(h) if r.abi_version == ABI_VERSION => {
                    if let Some(old) = self.root.replace(h) {
                        sys::handle_close(old).ok();
                    }
                    println!("[link] operator attached, ABI v{ABI_VERSION}");
                    // Whatever an earlier process revoked is still revoked.
                    self.load_revoked();
                    self.reply(&LinkReply { value: ABI_VERSION, ..Default::default() });
                }
                other => {
                    if let Some(h) = other {
                        sys::handle_close(h).ok();
                    }
                    self.reply_err(Error::Denied);
                }
            },
            req::INDEX => match self.index(r.path()) {
                Ok((docs, chunks)) => self.reply(&LinkReply {
                    total: chunks,
                    value: docs,
                    value3: chunks,
                    ..Default::default()
                }),
                Err(e) => self.reply_err(e),
            },
            req::QUERY => {
                let reply = self.query(r.query(), r.offset);
                self.reply(&reply);
            }
            req::REVOKE => match self.revoke(r.path()) {
                Ok(n) => self.reply(&LinkReply { total: n, value2: n, ..Default::default() }),
                Err(e) => self.reply_err(e),
            },
            req::BUNDLE => {
                let reply = self.build_bundle(r.query(), r.budget);
                self.reply(&reply);
            }
            req::BUNDLE_ENTRY => {
                let reply = self.bundle_entry(r.offset);
                self.reply(&reply);
            }
            req::STATS => self.reply(&LinkReply {
                total: self.chunks.len() as u32,
                value: self.docs.iter().filter(|d| !d.revoked).count() as u32,
                value2: self.revoked.len() as u32,
                value3: self.chunks.len() as u32,
                ..Default::default()
            }),
            req::FORGET => {
                let value = self.forget();
                self.reply(&LinkReply { value, ..Default::default() });
            }
            req::QUIT => {
                self.reply(&LinkReply::default());
                return false;
            }
            _ => self.reply_err(Error::NoSys),
        }
        true
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn space_main() -> i32 {
    println!("[link] SpaceLink service, ABI v{ABI_VERSION}");
    let mut link = Link::new(handle::BOOTSTRAP);
    let mut buf = [0u8; core::mem::size_of::<LinkRequest>()];
    loop {
        match sys::recv(handle::BOOTSTRAP, &mut buf, false) {
            Ok((n, transferred)) if n == buf.len() => {
                // SAFETY: the operator sends exactly one `LinkRequest`.
                let r: LinkRequest = unsafe { core::ptr::read_unaligned(buf.as_ptr() as *const _) };
                if !link.handle(&r, transferred) {
                    break;
                }
            }
            Ok((n, transferred)) => {
                if let Some(h) = transferred {
                    sys::handle_close(h).ok();
                }
                println!("[link] malformed request of {n} bytes");
                link.reply_err(Error::MsgSize);
            }
            Err(Error::PeerClosed) => {
                println!("[link] operator disconnected");
                break;
            }
            Err(e) => {
                println!("[link] receive failed: {e}");
                break;
            }
        }
    }
    if let Some(h) = link.root.take() {
        sys::handle_close(h).ok();
    }
    println!("[link] closing: {} chunk(s), {} revoked document(s)", link.chunks.len(), link.revoked.len());
    0
}
