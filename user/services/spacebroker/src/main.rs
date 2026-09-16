//! `spacebroker` – the Tool Broker (requirement G01).
//!
//! An agent may read, patch and check inside one declared workspace and nowhere
//! else. The broker is what makes that true: it holds the only file capability in
//! the picture, every tool call goes through its scope check, and every call -
//! allowed or refused - lands in an append-only audit log the operator can read
//! back afterwards.
//!
//! Patches are written through to the volume, inside the workspace and nowhere else.
//! The overlay is still the buffer a partial write lands in, but every write is
//! flushed, so a patch outlives the broker that applied it.
//!
//! (Historically: patches stayed in the overlay because the volume
//! is mounted read-only (ADR-0007). The agent cannot tell the difference: it writes
//! and reads back what it wrote, and the check runs against the patched view.
#![no_std]
#![no_main]

extern crate alloc;

use alloc::string::String;
use alloc::vec::Vec;

use libspace::spaceabi::agent::{
    ABI_VERSION, AUDIT_MAX, CHECK_VERIFY, CHUNK_MAX, ToolReply, ToolRequest, tool, verdict,
};
use libspace::spaceabi::error::Error;
use libspace::spaceabi::syscall::{DIR_ENTRIES_MAX, DirEntry};
use libspace::{Handle, handle, println, sys};

/// The workspace this broker was built to serve. Everything outside is refused.
const SCOPE: &str = "/spaceos/ws";
/// Files the overlay can hold, and the size of each.
const OVERLAY_FILES: usize = 4;
const OVERLAY_BYTES: usize = 8 * 1024;
/// The file a patch is expected to produce, and the file it is checked against.
const OUTPUT: &str = "/spaceos/ws/output.txt";
const EXPECT: &str = "/spaceos/ws/expect.txt";

struct Overlay {
    path: String,
    data: Vec<u8>,
}

struct Audit {
    seq: u32,
    tool: u32,
    verdict: u32,
    path: String,
}

struct Broker {
    operator: Handle,
    root: Option<Handle>,
    overlay: Vec<Overlay>,
    audit: Vec<Audit>,
    seq: u32,
    denied: u32,
}

fn as_bytes<T>(v: &T) -> &[u8] {
    // SAFETY: `T` is a `repr(C)` plain-data message.
    unsafe { core::slice::from_raw_parts(v as *const T as *const u8, core::mem::size_of::<T>()) }
}

fn tool_name(t: u32) -> &'static str {
    match t {
        tool::HELLO => "hello",
        tool::ATTACH => "attach",
        tool::LIST => "list",
        tool::READ => "read",
        tool::WRITE => "write",
        tool::CHECK => "check",
        tool::AUDIT => "audit",
        tool::DONE => "done",
        tool::QUIT => "quit",
        _ => "?",
    }
}

fn verdict_name(v: u32) -> &'static str {
    match v {
        verdict::ALLOWED => "allowed",
        verdict::DENIED_SCOPE => "denied-scope",
        verdict::DENIED_TOOL => "denied-tool",
        verdict::FAILED => "failed",
        _ => "?",
    }
}

/// True when `path` names something inside the workspace.
///
/// The rule is deliberately narrow: an absolute path, no `.` or `..` component, no
/// empty component, and the workspace prefix followed by a separator. Anything a
/// caller could use to climb out is simply not a valid path here.
fn in_scope(path: &str) -> bool {
    if !path.starts_with('/') || path.len() > 64 {
        return false;
    }
    let mut components = path.split('/').filter(|c| !c.is_empty());
    let scope: Vec<&str> = SCOPE.split('/').filter(|c| !c.is_empty()).collect();
    for want in &scope {
        match components.next() {
            Some(c) if c.eq_ignore_ascii_case(want) => {}
            _ => return false,
        }
    }
    let mut rest = 0usize;
    for c in components {
        if c == "." || c == ".." {
            return false;
        }
        rest += 1;
    }
    // The workspace directory itself is listable; files inside it are readable.
    rest <= 1 && !path.contains("//")
}

impl Broker {
    fn new(operator: Handle) -> Broker {
        Broker { operator, root: None, overlay: Vec::new(), audit: Vec::new(), seq: 0, denied: 0 }
    }

    fn record(&mut self, t: u32, v: u32, path: &str) -> u32 {
        self.seq = self.seq.saturating_add(1);
        if v != verdict::ALLOWED {
            self.denied = self.denied.saturating_add(1);
            println!("[broker] {} {} -> {}", tool_name(t), path, verdict_name(v));
        }
        if self.audit.len() < AUDIT_MAX {
            let mut p = String::new();
            p.push_str(path);
            self.audit.push(Audit { seq: self.seq, tool: t, verdict: v, path: p });
        }
        self.seq
    }

    fn overlay_of(&self, path: &str) -> Option<&Overlay> {
        self.overlay.iter().find(|o| o.path.eq_ignore_ascii_case(path))
    }

    /// Read from the overlay when the agent has written the file, otherwise from the
    /// read-only volume.
    fn read_file(&self, path: &str, offset: u32, len: u32, out: &mut [u8]) -> Result<usize, Error> {
        let want = (len as usize).min(out.len());
        if let Some(o) = self.overlay_of(path) {
            let start = (offset as usize).min(o.data.len());
            let end = (start + want).min(o.data.len());
            out[..end - start].copy_from_slice(&o.data[start..end]);
            return Ok(end - start);
        }
        let root = self.root.ok_or(Error::Denied)?;
        let f = sys::fs_open(root, path)?;
        let n = sys::fs_read(f, offset as u64, &mut out[..want]);
        sys::handle_close(f).ok();
        n
    }

    /// Put the overlay's copy of `path` on the volume.
    ///
    /// The path has already been checked against the workspace by the caller -- this
    /// writes exactly what the agent was allowed to write, and the file is rewritten
    /// whole, because that is the only shape `SYS_FS_CREATE` offers.
    fn flush(&mut self, path: &str) -> Result<(), Error> {
        let Some(root) = self.root else { return Err(Error::Denied) };
        let Some(o) = self.overlay_of(path) else { return Ok(()) };
        let data = o.data.clone();
        let file = sys::fs_create(root, path)?;
        let r = sys::fs_write(file, 0, &data);
        sys::handle_close(file).ok();
        r.map(|_| ())
    }

    fn write_overlay(&mut self, path: &str, offset: u32, data: &[u8]) -> Result<usize, Error> {
        let end = (offset as usize).checked_add(data.len()).ok_or(Error::Invalid)?;
        if end > OVERLAY_BYTES {
            return Err(Error::NoMemory);
        }
        let index = match self.overlay.iter().position(|o| o.path.eq_ignore_ascii_case(path)) {
            Some(i) => i,
            None => {
                if self.overlay.len() >= OVERLAY_FILES {
                    return Err(Error::NoMemory);
                }
                let mut p = String::new();
                p.push_str(path);
                self.overlay.push(Overlay { path: p, data: Vec::new() });
                self.overlay.len() - 1
            }
        };
        let o = &mut self.overlay[index];
        if o.data.len() < end {
            o.data.try_reserve(end - o.data.len()).map_err(|_| Error::NoMemory)?;
            o.data.resize(end, 0);
        }
        o.data[offset as usize..end].copy_from_slice(data);
        // Write through. A patch that exists only in this process is a patch that
        // disappears when it exits, which is not what the agent was told it did. A
        // volume that refuses the write is reported to the agent, not swallowed.
        let written = data.len();
        self.flush(path)?;
        Ok(written)
    }

    /// Compare the patched file with the expected one, byte for byte.
    fn check(&mut self, name: &str) -> Result<u64, Error> {
        if name != CHECK_VERIFY {
            return Err(Error::NoSys);
        }
        let produced = self.overlay_of(OUTPUT).ok_or(Error::NotFound)?.data.clone();
        let root = self.root.ok_or(Error::Denied)?;
        let f = sys::fs_open(root, EXPECT)?;
        let stat = sys::fs_stat(f)?;
        let mut expected = Vec::new();
        expected.try_reserve(stat.size as usize).map_err(|_| Error::NoMemory)?;
        expected.resize(stat.size as usize, 0);
        let n = sys::fs_read(f, 0, &mut expected)?;
        sys::handle_close(f).ok();
        expected.truncate(n);
        let ok = produced == expected;
        println!(
            "[broker] check '{name}': {} ({} bytes produced, {} expected)",
            if ok { "PASS" } else { "FAIL" },
            produced.len(),
            expected.len()
        );
        Ok(u64::from(ok))
    }

    fn list(&mut self, path: &str) -> Result<(u64, String), Error> {
        let root = self.root.ok_or(Error::Denied)?;
        let mut entries = [DirEntry::default(); DIR_ENTRIES_MAX];
        let n = sys::fs_list(root, path, &mut entries)?;
        let mut text = String::new();
        for e in entries.iter().take(n) {
            if e.name() == "." || e.name() == ".." {
                continue;
            }
            if !text.is_empty() {
                text.push(' ');
            }
            text.push_str(e.name());
        }
        // Overlay files the agent created are part of the view it sees.
        for o in &self.overlay {
            let name = o.path.rsplit('/').next().unwrap_or("");
            if !text.split(' ').any(|t| t.eq_ignore_ascii_case(name)) {
                if !text.is_empty() {
                    text.push(' ');
                }
                text.push_str(name);
            }
        }
        Ok((n as u64, text))
    }

    /// Serve one agent request. Returns false when the agent is finished.
    fn serve(&mut self, agent: Handle, r: &ToolRequest) -> bool {
        let mut reply = ToolReply::default();
        let path = r.path();
        match r.tool {
            tool::HELLO => {
                if r.abi_version == ABI_VERSION {
                    reply.seq = self.record(r.tool, verdict::ALLOWED, "-");
                    reply.value = ABI_VERSION as u64;
                } else {
                    reply.seq = self.record(r.tool, verdict::DENIED_TOOL, "-");
                    reply.verdict = verdict::DENIED_TOOL;
                    reply.status = -(Error::Invalid as u32 as i32);
                }
            }
            tool::LIST | tool::READ | tool::WRITE => {
                if !in_scope(path) {
                    reply.seq = self.record(r.tool, verdict::DENIED_SCOPE, path);
                    reply.verdict = verdict::DENIED_SCOPE;
                    reply.status = -(Error::Denied as u32 as i32);
                } else {
                    let outcome = match r.tool {
                        tool::LIST => self.list(path).map(|(n, text)| {
                            reply.set_data(text.as_bytes());
                            n
                        }),
                        tool::READ => {
                            let mut buf = [0u8; CHUNK_MAX];
                            self.read_file(path, r.offset, r.len, &mut buf).map(|n| {
                                reply.set_data(&buf[..n]);
                                n as u64
                            })
                        }
                        _ => self.write_overlay(path, r.offset, r.data()).map(|n| n as u64),
                    };
                    match outcome {
                        Ok(v) => {
                            reply.value = v;
                            reply.seq = self.record(r.tool, verdict::ALLOWED, path);
                        }
                        Err(e) => {
                            reply.status = -(e as u32 as i32);
                            reply.verdict = verdict::FAILED;
                            reply.seq = self.record(r.tool, verdict::FAILED, path);
                        }
                    }
                }
            }
            tool::CHECK => match self.check(path) {
                Ok(v) => {
                    reply.value = v;
                    reply.seq = self.record(r.tool, verdict::ALLOWED, path);
                }
                Err(e) => {
                    reply.status = -(e as u32 as i32);
                    reply.verdict = verdict::FAILED;
                    reply.seq = self.record(r.tool, verdict::FAILED, path);
                }
            },
            tool::DONE => {
                reply.seq = self.record(r.tool, verdict::ALLOWED, "-");
                reply.value = self.denied as u64;
                let _ = sys::send(agent, as_bytes(&reply), None);
                return false;
            }
            // An agent has no business attaching channels, reading the audit or
            // shutting the broker down: those are the operator's tools.
            _ => {
                reply.seq = self.record(r.tool, verdict::DENIED_TOOL, path);
                reply.verdict = verdict::DENIED_TOOL;
                reply.status = -(Error::Denied as u32 as i32);
            }
        }
        let _ = sys::send(agent, as_bytes(&reply), None);
        true
    }

    /// Serve the agent until it finishes or its channel closes. An agent that
    /// crashes ends this loop through `PeerClosed`, which is not an error: the
    /// broker reports what happened and the operator decides.
    fn serve_agent(&mut self, agent: Handle) {
        let mut buf = [0u8; core::mem::size_of::<ToolRequest>()];
        loop {
            match sys::recv(agent, &mut buf, false) {
                Ok((n, transferred)) if n == buf.len() => {
                    if let Some(h) = transferred {
                        sys::handle_close(h).ok();
                    }
                    // SAFETY: the agent sends exactly one `ToolRequest`.
                    let r: ToolRequest = unsafe { core::ptr::read_unaligned(buf.as_ptr() as *const _) };
                    if !self.serve(agent, &r) {
                        println!("[broker] agent reported done");
                        return;
                    }
                }
                Ok((n, transferred)) => {
                    if let Some(h) = transferred {
                        sys::handle_close(h).ok();
                    }
                    println!("[broker] malformed tool request of {n} bytes");
                    let reply = ToolReply {
                        status: -(Error::MsgSize as u32 as i32),
                        verdict: verdict::DENIED_TOOL,
                        ..Default::default()
                    };
                    let _ = sys::send(agent, as_bytes(&reply), None);
                }
                Err(Error::PeerClosed) => {
                    println!("[broker] agent is gone; the workspace is unchanged from here on");
                    return;
                }
                Err(e) => {
                    println!("[broker] receive from agent failed: {e}");
                    return;
                }
            }
        }
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn space_main() -> i32 {
    println!("[broker] Space OS Tool Broker, ABI v{ABI_VERSION}, workspace {SCOPE}");
    let mut b = Broker::new(handle::BOOTSTRAP);
    let mut buf = [0u8; core::mem::size_of::<ToolRequest>()];
    let mut agent: Option<Handle> = None;
    loop {
        let (n, transferred) = match sys::recv(b.operator, &mut buf, false) {
            Ok(v) => v,
            Err(Error::PeerClosed) => break,
            Err(e) => {
                println!("[broker] receive failed: {e}");
                break;
            }
        };
        if n != buf.len() {
            if let Some(h) = transferred {
                sys::handle_close(h).ok();
            }
            let reply = ToolReply { status: -(Error::MsgSize as u32 as i32), ..Default::default() };
            let _ = sys::send(b.operator, as_bytes(&reply), None);
            continue;
        }
        // SAFETY: the operator sends exactly one `ToolRequest`.
        let r: ToolRequest = unsafe { core::ptr::read_unaligned(buf.as_ptr() as *const _) };
        let mut reply = ToolReply::default();
        match r.tool {
            tool::HELLO => match transferred {
                Some(h) if r.abi_version == ABI_VERSION => {
                    if let Some(old) = b.root.replace(h) {
                        sys::handle_close(old).ok();
                    }
                    println!("[broker] operator attached, ABI v{ABI_VERSION}");
                }
                other => {
                    if let Some(h) = other {
                        sys::handle_close(h).ok();
                    }
                    reply.status = -(Error::Denied as u32 as i32);
                }
            },
            tool::ATTACH => match transferred {
                Some(h) => {
                    if let Some(old) = agent.replace(h) {
                        sys::handle_close(old).ok();
                    }
                    println!("[broker] serving an agent");
                    // Serve the agent to completion, then report.
                    b.serve_agent(h);
                    reply.value = b.denied as u64;
                    reply.seq = b.seq;
                    if let Some(h) = agent.take() {
                        sys::handle_close(h).ok();
                    }
                }
                None => reply.status = -(Error::Invalid as u32 as i32),
            },
            tool::AUDIT => {
                if let Some(h) = transferred {
                    sys::handle_close(h).ok();
                }
                match b.audit.get(r.offset as usize) {
                    Some(a) => {
                        reply.seq = a.seq;
                        reply.verdict = a.verdict;
                        reply.value = b.audit.len() as u64;
                        let mut line = String::new();
                        line.push_str(tool_name(a.tool));
                        line.push(' ');
                        line.push_str(&a.path);
                        reply.set_data(line.as_bytes());
                    }
                    None => {
                        reply.status = -(Error::NotFound as u32 as i32);
                        reply.value = b.audit.len() as u64;
                    }
                }
            }
            tool::QUIT => {
                if let Some(h) = transferred {
                    sys::handle_close(h).ok();
                }
                let _ = sys::send(b.operator, as_bytes(&reply), None);
                break;
            }
            _ => {
                if let Some(h) = transferred {
                    sys::handle_close(h).ok();
                }
                reply.status = -(Error::NoSys as u32 as i32);
            }
        }
        let _ = sys::send(b.operator, as_bytes(&reply), None);
    }
    if let Some(h) = agent.take() {
        sys::handle_close(h).ok();
    }
    if let Some(h) = b.root.take() {
        sys::handle_close(h).ok();
    }
    println!("[broker] closing: {} tool calls, {} refused", b.seq, b.denied);
    0
}
