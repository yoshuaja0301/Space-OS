//! `init` – the first user process.
//!
//! In the MVP it is also the acceptance-test driver for K01/K02/K03 (PRD §7): it
//! spawns the test programs from the initrd with scoped quotas, checks how they end,
//! and shuts the machine down with an exit code the harness can read.
#![no_std]
#![no_main]

extern crate alloc;

use alloc::string::String;
use alloc::vec::Vec;

use libspace::compute::Compute;
use libspace::sha256;
use libspace::shell::Session;
use libspace::spaceabi::agent::{self as agent_abi, ToolReply, ToolRequest, tool, verdict};
use libspace::spaceabi::compute::{
    self as compute_abi, BufferRef as ComputeBufferRef, Op as ComputeOp, Request as ComputeRequest,
    Response as ComputeResponse, op as cop, req as creq,
};
use libspace::spaceabi::error::{Error, decode};
use libspace::spaceabi::handle::{MAX_HANDLES, rights};
use libspace::spaceabi::hmac::{hmac_sha256, verify as hmac_verify};
use libspace::spaceabi::link::{self as link_abi, LinkReply, LinkRequest, req as lreq};
use libspace::spaceabi::pkg::{PkgReply, PkgRequest, reject_code, req as preq};
use libspace::spaceabi::shell::{Reply as ShellReply, job as shell_job, worker_state};
use libspace::spaceabi::syscall::DirEntry;
use libspace::spaceabi::syscall::nr;
use libspace::spaceabi::syscall::{ExitStatus, KernelStats};
use libspace::{Handle, exit_kind, handle, kill_reason, println, sys};

const ROOT: Handle = handle::BOOTSTRAP;
/// Quota for test children: code + 64 KiB stack + 128 KiB heap + test allocations.
const CHILD_QUOTA: u64 = 128;
const CYCLES: u32 = 50;

struct Runner {
    passed: u32,
    failed: Vec<String>,
    skipped: Vec<String>,
}

impl Runner {
    /// Run a test only when the machine has what it needs. A configuration without
    /// a disk is a supported configuration, not a failure: the test is reported as
    /// skipped with the reason, and the run still ends green.
    fn run_if(
        &mut self,
        available: bool,
        why: &str,
        id: &str,
        name: &str,
        f: impl FnOnce() -> Result<(), String>,
    ) {
        if available {
            self.run(id, name, f);
        } else {
            println!("[init] SKIP {id}: {name} ({why})");
            self.skipped.push(alloc::format!("{id} {name}: {why}"));
        }
    }

    fn run(&mut self, id: &str, name: &str, f: impl FnOnce() -> Result<(), String>) {
        println!("[init] --- {id}: {name}");
        match f() {
            Ok(()) => {
                self.passed += 1;
                println!("[init] PASS {id}: {name}");
            }
            Err(why) => {
                self.failed.push(alloc::format!("{id} {name}: {why}"));
                println!("[init] FAIL {id}: {name}: {why}");
            }
        }
    }
}

fn spawn_and_wait(name: &str, quota: u64, pass: Option<Handle>) -> Result<ExitStatus, String> {
    let p = sys::spawn(ROOT, name, quota, pass).map_err(|e| alloc::format!("spawn {name}: {e}"))?;
    let st = sys::wait(p).map_err(|e| alloc::format!("wait {name}: {e}"))?;
    sys::handle_close(p).map_err(|e| alloc::format!("close {name}: {e}"))?;
    Ok(st)
}

fn expect_exit(name: &str, st: ExitStatus, code: i32) -> Result<(), String> {
    if st.is_exited_with(code) {
        Ok(())
    } else {
        Err(alloc::format!("{name} ended with {st:?}, expected exit {code}"))
    }
}

fn expect_killed(mode: &str, st: ExitStatus, reason: u32) -> Result<(), String> {
    if st.is_killed_by(reason) {
        Ok(())
    } else {
        Err(alloc::format!(
            "fault '{mode}' ended with {st:?}, expected kill reason {} ({})",
            reason,
            kill_reason::name(reason)
        ))
    }
}

fn fault_case(mode: &str, reason: u32) -> Result<(), String> {
    fault_case_any(mode, &[reason])
}

/// Like `fault_case`, accepting any of `reasons` (some faults are reported
/// differently by emulators and real CPUs, e.g. a non-canonical jump).
fn fault_case_any(mode: &str, reasons: &[u32]) -> Result<(), String> {
    let (mine, theirs) = sys::channel_create().map_err(|e| alloc::format!("channel: {e}"))?;
    let p = sys::spawn(ROOT, "bin/fault", CHILD_QUOTA, Some(theirs))
        .map_err(|e| alloc::format!("spawn fault: {e}"))?;
    sys::send(mine, mode.as_bytes(), None).map_err(|e| alloc::format!("send mode: {e}"))?;
    let st = sys::wait(p).map_err(|e| alloc::format!("wait: {e}"))?;
    sys::handle_close(p).ok();
    sys::handle_close(mine).ok();
    if reasons.iter().any(|&r| st.is_killed_by(r)) { Ok(()) } else { expect_killed(mode, st, reasons[0]) }
}

fn cycle() -> Result<(), String> {
    let (mine, theirs) = sys::channel_create().map_err(|e| alloc::format!("channel: {e}"))?;
    let p = sys::spawn(ROOT, "bin/worker", CHILD_QUOTA, Some(theirs))
        .map_err(|e| alloc::format!("spawn worker: {e}"))?;
    let mut buf = [0u8; 16];
    let (n, _) = sys::recv(mine, &mut buf, false).map_err(|e| alloc::format!("recv: {e}"))?;
    if &buf[..n] != b"done" {
        return Err(String::from("worker sent an unexpected message"));
    }
    let st = sys::wait(p).map_err(|e| alloc::format!("wait: {e}"))?;
    sys::handle_close(p).ok();
    sys::handle_close(mine).ok();
    expect_exit("worker", st, 0)
}

/// Spawn a service that blocks in `recv`, kill it while it waits, reap it.
fn kill_blocked_cycle() -> Result<(), String> {
    let (mine, theirs) = sys::channel_create().map_err(|e| alloc::format!("channel: {e}"))?;
    let p = sys::spawn(ROOT, "bin/ipc_echo", CHILD_QUOTA, Some(theirs))
        .map_err(|e| alloc::format!("spawn: {e}"))?;
    // One round trip guarantees the child is past startup and back in recv.
    sys::send(mine, b"x", None).map_err(|e| alloc::format!("send: {e}"))?;
    let mut buf = [0u8; 8];
    let (n, _) = sys::recv(mine, &mut buf, false).map_err(|e| alloc::format!("recv: {e}"))?;
    if &buf[..n] != b"X" {
        return Err(String::from("echo reply mismatch"));
    }
    sys::kill(p).map_err(|e| alloc::format!("kill: {e}"))?;
    let st = sys::wait(p).map_err(|e| alloc::format!("wait: {e}"))?;
    sys::handle_close(p).ok();
    // The dead peer must have closed its side.
    match sys::recv(mine, &mut buf, true) {
        Err(Error::PeerClosed) => {}
        other => return Err(alloc::format!("after kill, recv gave {other:?}, expected PeerClosed")),
    }
    sys::handle_close(mine).ok();
    if st.is_killed_by(kill_reason::SIGNAL) { Ok(()) } else { Err(alloc::format!("echo ended with {st:?}")) }
}

/// Spawn `bin/blocker` in `mode`, wait until it is blocked, kill it and reap it.
/// `pass` is moved into the child together with the mode message.
fn blocked_kill_cycle(mode: &str, pass: Option<Handle>) -> Result<(), String> {
    let (mine, theirs) = sys::channel_create().map_err(|e| alloc::format!("channel: {e}"))?;
    let p = sys::spawn(ROOT, "bin/blocker", CHILD_QUOTA, Some(theirs))
        .map_err(|e| alloc::format!("spawn: {e}"))?;
    sys::send(mine, mode.as_bytes(), pass).map_err(|e| alloc::format!("send mode: {e}"))?;
    let mut buf = [0u8; 16];
    let (n, _) = sys::recv(mine, &mut buf, false).map_err(|e| alloc::format!("ready: {e}"))?;
    if &buf[..n] != b"ready" {
        return Err(String::from("blocker did not report ready"));
    }
    // It is inside sleep/wait/recv now (the reply is sent immediately before blocking).
    sys::sleep_ms(5);
    sys::kill(p).map_err(|e| alloc::format!("kill: {e}"))?;
    let st = sys::wait(p).map_err(|e| alloc::format!("wait: {e}"))?;
    sys::handle_close(p).ok();
    sys::handle_close(mine).ok();
    if st.is_killed_by(kill_reason::SIGNAL) {
        Ok(())
    } else {
        Err(alloc::format!("blocker '{mode}' ended with {st:?}"))
    }
}

/// Run `cycle` `n` times and require that no frame and no kernel-heap byte is lost.
fn no_leak_over(n: u32, label: &str, mut cycle: impl FnMut() -> Result<(), String>) -> Result<(), String> {
    for _ in 0..3 {
        cycle()?; // warm up
    }
    let before = stats()?;
    for i in 0..n {
        cycle().map_err(|e| alloc::format!("cycle {i}: {e}"))?;
    }
    let after = stats()?;
    println!(
        "[init] {label}: frames free {} -> {}, heap used {} -> {}, live processes {}",
        before.frames_free, after.frames_free, before.heap_used, after.heap_used, after.processes_live
    );
    if after.frames_free != before.frames_free || after.heap_used != before.heap_used {
        return Err(alloc::format!(
            "leak over {n} cycles: frames {} -> {}, heap {} -> {}",
            before.frames_free,
            after.frames_free,
            before.heap_used,
            after.heap_used
        ));
    }
    Ok(())
}

/// Read a whole file from the guest disk into `sink`, 4 KiB at a time.
fn stream_file(path: &str, mut sink: impl FnMut(&[u8])) -> Result<u64, String> {
    let f = sys::fs_open(ROOT, path).map_err(|e| alloc::format!("open {path}: {e}"))?;
    let st = sys::fs_stat(f).map_err(|e| alloc::format!("stat {path}: {e}"))?;
    let mut buf = [0u8; 4096];
    let mut offset = 0u64;
    loop {
        let n = sys::fs_read(f, offset, &mut buf).map_err(|e| alloc::format!("read {path}: {e}"))?;
        if n == 0 {
            break;
        }
        sink(&buf[..n]);
        offset += n as u64;
        if offset > st.size {
            sys::handle_close(f).ok();
            return Err(alloc::format!("{path} returned more than its {} byte size", st.size));
        }
    }
    sys::handle_close(f).ok();
    if offset != st.size {
        return Err(alloc::format!("{path}: read {offset} of {} bytes", st.size));
    }
    Ok(offset)
}

/// Value of `key=` in a `key=value` manifest line set.
fn manifest_value<'a>(manifest: &'a str, key: &str) -> Option<&'a str> {
    manifest.lines().find_map(|l| l.strip_prefix(key)?.strip_prefix('=').map(str::trim))
}

/// Quota for the compute service: it pays for every buffer it hands out.
const COMPUTE_QUOTA: u64 = 4096;
/// Quota for the AI runtime: its own code, stack and heap; compute buffers are
/// charged to the service that creates them.
const AI_QUOTA: u64 = 512;

fn as_bytes<T>(v: &T) -> &[u8] {
    // SAFETY: `T` is a `repr(C)` plain-data message.
    unsafe { core::slice::from_raw_parts(v as *const T as *const u8, core::mem::size_of::<T>()) }
}

/// One request/response round trip on a raw channel (used before the ABI version
/// has been negotiated, which `Compute::connect` would do for us).
fn compute_raw(ch: Handle, request: &ComputeRequest) -> Result<ComputeResponse, String> {
    sys::send(ch, as_bytes(request), None).map_err(|e| alloc::format!("send: {e}"))?;
    let mut buf = [0u8; core::mem::size_of::<ComputeResponse>()];
    let (n, _) = sys::recv(ch, &mut buf, false).map_err(|e| alloc::format!("recv: {e}"))?;
    if n != buf.len() {
        return Err(alloc::format!("reply was {n} bytes"));
    }
    // SAFETY: the service replies with exactly one Response.
    Ok(unsafe { core::ptr::read_unaligned(buf.as_ptr() as *const ComputeResponse) })
}

/// A request as raw bytes, for the few cases that need `send` directly (handle
/// transfer) instead of going through `Compute::call`.
fn as_request_bytes(r: &ComputeRequest) -> &[u8] {
    // SAFETY: `Request` is a `repr(C)` plain-data message.
    unsafe {
        core::slice::from_raw_parts(
            r as *const ComputeRequest as *const u8,
            core::mem::size_of::<ComputeRequest>(),
        )
    }
}

/// Quota for the package service: the image it reads plus the versions it holds.
const PKG_QUOTA: u64 = 256;

fn as_pkg_bytes(r: &PkgRequest) -> &[u8] {
    // SAFETY: `PkgRequest` is a `repr(C)` plain-data message.
    unsafe {
        core::slice::from_raw_parts(r as *const PkgRequest as *const u8, core::mem::size_of::<PkgRequest>())
    }
}

fn pkg_reply(ch: Handle) -> Result<PkgReply, String> {
    let mut buf = [0u8; core::mem::size_of::<PkgReply>()];
    let (n, transferred) = sys::recv(ch, &mut buf, false).map_err(|e| alloc::format!("recv: {e}"))?;
    if let Some(h) = transferred {
        sys::handle_close(h).ok();
    }
    if n != buf.len() {
        return Err(alloc::format!("spacepkg replied with {n} bytes"));
    }
    // SAFETY: the service replies with exactly one `PkgReply`.
    Ok(unsafe { core::ptr::read_unaligned(buf.as_ptr() as *const PkgReply) })
}

fn pkg_call(ch: Handle, r: &PkgRequest) -> Result<PkgReply, String> {
    sys::send(ch, as_pkg_bytes(r), None).map_err(|e| alloc::format!("send: {e}"))?;
    pkg_reply(ch)
}

fn pkg_start() -> Result<(Handle, Handle), String> {
    let (mine, theirs) = sys::channel_create().map_err(|e| alloc::format!("channel: {e}"))?;
    let svc = sys::spawn(ROOT, "bin/spacepkg", PKG_QUOTA, Some(theirs))
        .map_err(|e| alloc::format!("spawn spacepkg: {e}"))?;
    let root =
        sys::handle_dup(ROOT, rights::FS | rights::TRANSFER).map_err(|e| alloc::format!("dup root: {e}"))?;
    let hello = PkgRequest { kind: preq::HELLO, abi_version: 0, ..Default::default() };
    sys::send(mine, as_pkg_bytes(&hello), Some(root)).map_err(|e| alloc::format!("hello: {e}"))?;
    pkg_reply(mine)?.result().map_err(|e| alloc::format!("hello: {e}"))?;
    Ok((mine, svc))
}

fn pkg_stop(ch: Handle, svc: Handle) -> Result<(), String> {
    pkg_call(ch, &PkgRequest::new(preq::QUIT))?;
    let st = sys::wait(svc).map_err(|e| alloc::format!("wait: {e}"))?;
    sys::handle_close(svc).ok();
    sys::handle_close(ch).ok();
    expect_exit("spacepkg", st, 0)
}

/// Read the whole payload of the active package, one reply at a time.
fn pkg_payload(ch: Handle, len: u32) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    let mut offset = 0u32;
    while offset < len {
        let mut r = PkgRequest::new(preq::READ);
        r.offset = offset;
        let reply = pkg_call(ch, &r)?;
        reply.result().map_err(|e| alloc::format!("read payload: {e}"))?;
        if reply.len == 0 {
            break;
        }
        out.extend_from_slice(reply.data());
        offset += reply.len;
    }
    if out.len() != len as usize {
        return Err(alloc::format!("payload is {} bytes, the service reported {len}", out.len()));
    }
    Ok(out)
}

/// SpaceLink corpus on the guest volume, and the document that exists to be revoked.
const CORPUS: &str = "/spaceos/docs";
const SECRET: &str = "/spaceos/docs/SECRET.TXT";
/// Quota for the SpaceLink service: the corpus plus its index.
const LINK_QUOTA: u64 = 256;

fn as_link_bytes(r: &LinkRequest) -> &[u8] {
    // SAFETY: `LinkRequest` is a `repr(C)` plain-data message.
    unsafe {
        core::slice::from_raw_parts(r as *const LinkRequest as *const u8, core::mem::size_of::<LinkRequest>())
    }
}

fn link_call(ch: Handle, r: &LinkRequest) -> Result<LinkReply, String> {
    sys::send(ch, as_link_bytes(r), None).map_err(|e| alloc::format!("send: {e}"))?;
    let mut buf = [0u8; core::mem::size_of::<LinkReply>()];
    let (n, transferred) = sys::recv(ch, &mut buf, false).map_err(|e| alloc::format!("recv: {e}"))?;
    if let Some(h) = transferred {
        sys::handle_close(h).ok();
    }
    if n != buf.len() {
        return Err(alloc::format!("spacelink replied with {n} bytes"));
    }
    // SAFETY: the service replies with exactly one `LinkReply`.
    Ok(unsafe { core::ptr::read_unaligned(buf.as_ptr() as *const LinkReply) })
}

/// Start the service and hand it a file capability narrowed to `FS`.
fn link_start() -> Result<(Handle, Handle), String> {
    let (mine, theirs) = sys::channel_create().map_err(|e| alloc::format!("channel: {e}"))?;
    let svc = sys::spawn(ROOT, "bin/spacelink", LINK_QUOTA, Some(theirs))
        .map_err(|e| alloc::format!("spawn spacelink: {e}"))?;
    let root =
        sys::handle_dup(ROOT, rights::FS | rights::TRANSFER).map_err(|e| alloc::format!("dup root: {e}"))?;
    let mut hello = LinkRequest::new(lreq::HELLO);
    hello.abi_version = link_abi::ABI_VERSION;
    sys::send(mine, as_link_bytes(&hello), Some(root)).map_err(|e| alloc::format!("hello: {e}"))?;
    let mut buf = [0u8; core::mem::size_of::<LinkReply>()];
    let (n, _) = sys::recv(mine, &mut buf, false).map_err(|e| alloc::format!("hello reply: {e}"))?;
    if n != buf.len() {
        return Err(String::from("spacelink did not answer HELLO"));
    }
    Ok((mine, svc))
}

fn link_stop(link: Handle, svc: Handle) -> Result<(), String> {
    link_call(link, &LinkRequest::new(lreq::QUIT))?;
    let st = sys::wait(svc).map_err(|e| alloc::format!("wait: {e}"))?;
    sys::handle_close(svc).ok();
    sys::handle_close(link).ok();
    expect_exit("spacelink", st, 0)
}

/// Re-read the byte range SpaceLink named and check the digest it reported. This is
/// the whole point of provenance: the caller does not have to believe the service.
fn verify_provenance(reply: &LinkReply) -> Result<(), String> {
    if reply.len == 0 || reply.len as usize > 1024 {
        return Err(alloc::format!("chunk of {} bytes is not usable", reply.len));
    }
    let f = sys::fs_open(ROOT, reply.path()).map_err(|e| alloc::format!("open {}: {e}", reply.path()))?;
    let mut buf = alloc::vec![0u8; reply.len as usize];
    let n = sys::fs_read(f, reply.offset as u64, &mut buf).map_err(|e| alloc::format!("read: {e}"));
    sys::handle_close(f).ok();
    let n = n?;
    if n != reply.len as usize {
        return Err(alloc::format!("chunk claims {} bytes, the file yields {n}", reply.len));
    }
    if sha256::digest(&buf) != reply.digest {
        return Err(alloc::format!(
            "digest mismatch for {} [{}..{}]",
            reply.path(),
            reply.offset,
            reply.offset + reply.len
        ));
    }
    if !buf.starts_with(reply.text()) {
        return Err(String::from("the text returned is not the start of the chunk on disk"));
    }
    Ok(())
}

/// Every result for `query` must come from somewhere other than `path`.
fn assert_absent(link: Handle, query: &str, path: &str) -> Result<(), String> {
    let first = link_call(link, &LinkRequest::with_query(lreq::QUERY, query))?;
    let total = if first.status == 0 { first.total } else { 0 };
    for i in 0..total {
        let mut q = LinkRequest::with_query(lreq::QUERY, query);
        q.offset = i;
        let hit = link_call(link, &q)?;
        if hit.status == 0 && hit.path().eq_ignore_ascii_case(path) {
            return Err(alloc::format!("{path} still appears in results for {query:?}"));
        }
    }
    Ok(())
}

/// The Tool Broker speaks fixed-size messages; these helpers keep the tests
/// readable without hiding which channel each call travels on.
fn as_tool_bytes(r: &ToolRequest) -> &[u8] {
    // SAFETY: `ToolRequest` is a `repr(C)` plain-data message.
    unsafe {
        core::slice::from_raw_parts(r as *const ToolRequest as *const u8, core::mem::size_of::<ToolRequest>())
    }
}

fn broker_reply(ch: Handle) -> Result<ToolReply, String> {
    let mut buf = [0u8; core::mem::size_of::<ToolReply>()];
    let (n, transferred) = sys::recv(ch, &mut buf, false).map_err(|e| alloc::format!("recv: {e}"))?;
    if let Some(h) = transferred {
        sys::handle_close(h).ok();
    }
    if n != buf.len() {
        return Err(alloc::format!("broker replied with {n} bytes"));
    }
    // SAFETY: the broker replies with exactly one `ToolReply`.
    Ok(unsafe { core::ptr::read_unaligned(buf.as_ptr() as *const ToolReply) })
}

fn tool_call(ch: Handle, r: &ToolRequest) -> Result<ToolReply, String> {
    sys::send(ch, as_tool_bytes(r), None).map_err(|e| alloc::format!("send: {e}"))?;
    broker_reply(ch)
}

fn broker_hello(op: Handle, root: Handle) -> Result<(), String> {
    let mut hello = ToolRequest::new(tool::HELLO, "-");
    hello.abi_version = agent_abi::ABI_VERSION;
    sys::send(op, as_tool_bytes(&hello), Some(root)).map_err(|e| alloc::format!("hello: {e}"))?;
    broker_reply(op)?.result().map_err(|e| alloc::format!("hello: {e}"))?;
    Ok(())
}

fn broker_quit(op: Handle) -> Result<(), String> {
    sys::send(op, as_tool_bytes(&ToolRequest::new(tool::QUIT, "-")), None)
        .map_err(|e| alloc::format!("quit: {e}"))?;
    broker_reply(op)?;
    Ok(())
}

/// Walk the audit log; returns (entries, allowed, refused).
fn read_audit(op: Handle) -> Result<(u32, u32, u32), String> {
    let (mut entries, mut allowed, mut denied) = (0u32, 0u32, 0u32);
    loop {
        let mut req = ToolRequest::new(tool::AUDIT, "-");
        req.offset = entries;
        let reply = tool_call(op, &req)?;
        if reply.status != 0 {
            return Ok((entries, allowed, denied));
        }
        if reply.verdict == verdict::ALLOWED {
            allowed += 1;
        } else {
            denied += 1;
        }
        entries += 1;
        if entries > 512 {
            return Err(String::from("the audit log never ended"));
        }
    }
}

fn audit_contains(op: Handle, entries: u32, want_verdict: u32, want: &str) -> Result<bool, String> {
    for i in 0..entries {
        let mut req = ToolRequest::new(tool::AUDIT, "-");
        req.offset = i;
        let reply = tool_call(op, &req)?;
        if reply.status == 0 && reply.verdict == want_verdict && reply.text() == want {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Poll the session until its worker leaves the running state. The point of the
/// poll is the assertion: the session has to answer every single one of these
/// while the worker is crashing, spinning or sleeping.
fn wait_for_worker(s: &Session, want: u32, timeout_ms: u64) -> Result<ShellReply, String> {
    let deadline = sys::ticks_ms().saturating_add(timeout_ms);
    loop {
        let st = s.status().map_err(|e| alloc::format!("status while waiting: {e}"))?;
        st.result().map_err(|e| alloc::format!("status while waiting: {e}"))?;
        if st.state == want {
            return Ok(st);
        }
        if st.state != worker_state::RUNNING {
            return Err(alloc::format!("worker reached state {}, expected {want}", st.state));
        }
        if sys::ticks_ms() >= deadline {
            return Err(alloc::format!("worker still running after {timeout_ms} ms"));
        }
        sys::sleep_ms(2);
    }
}

fn expect_status(what: &str, got: i32, want: Error) -> Result<(), String> {
    let want_status = -(want as u32 as i32);
    if got == want_status {
        Ok(())
    } else {
        Err(alloc::format!("{what}: status {got}, expected {want_status} ({want:?})"))
    }
}

fn close_enough(a: f32, b: f32) -> bool {
    let diff = if a > b { a - b } else { b - a };
    diff <= 1e-4 * (1.0 + if a > 0.0 { a } else { -a })
}

fn stats() -> Result<KernelStats, String> {
    sys::kstats(ROOT).map_err(|e| alloc::format!("kstats: {e}"))
}

#[unsafe(no_mangle)]
pub extern "C" fn space_main() -> i32 {
    let me = sys::self_info().unwrap_or_default();
    println!(
        "[init] Space OS init running: pid {}, ABI v{}, quota {} pages ({} used)",
        me.pid, me.abi_version, me.quota_pages, me.used_pages
    );
    let mut r = Runner { passed: 0, failed: Vec::new(), skipped: Vec::new() };
    // Storage is optional hardware. Everything that reads the guest disk is skipped
    // (not failed) on a machine that has none.
    let disk = match sys::kstats(ROOT) {
        Ok(s) => s.volume_sectors != 0,
        Err(e) => {
            println!("[init] kstats failed: {e}");
            false
        }
    };
    println!(
        "[init] storage: {}",
        if disk { "FAT32 volume mounted" } else { "none; disk-backed tests will be skipped" }
    );

    r.run("K01", "boot reached init in user space", || {
        let s = stats()?;
        println!(
            "[init] kernel: {} MiB frames free of {}, heap {} KiB used, uptime {} ms",
            s.frames_free * 4 / 1024,
            s.frames_total * 4 / 1024,
            s.heap_used / 1024,
            s.uptime_ms
        );
        if s.processes_live == 1 {
            Ok(())
        } else {
            Err(alloc::format!("{} live processes, expected 1", s.processes_live))
        }
    });

    r.run("K02", "hello world process exits 0", || {
        expect_exit("hello", spawn_and_wait("bin/hello", CHILD_QUOTA, None)?, 0)
    });

    r.run("K02", "spawn of a missing program is rejected", || {
        match sys::spawn(ROOT, "bin/does_not_exist", CHILD_QUOTA, None) {
            Err(Error::NotFound) => Ok(()),
            other => Err(alloc::format!("got {other:?}")),
        }
    });

    r.run("K02", "spawn beyond quota is rejected", || match sys::spawn(ROOT, "bin/hello", 4, None) {
        Err(Error::Quota) => Ok(()),
        other => Err(alloc::format!("got {other:?}")),
    });

    r.run("K02", "write to kernel memory kills the process (page fault)", || {
        fault_case("kernel_write", kill_reason::PAGE_FAULT)
    });
    r.run("K02", "null read kills the process (page fault)", || {
        fault_case("null_read", kill_reason::PAGE_FAULT)
    });
    r.run("K02", "executing the NX stack kills the process (page fault)", || {
        fault_case("nx_exec", kill_reason::PAGE_FAULT)
    });
    r.run("K02", "divide by zero kills the process", || fault_case("div_zero", kill_reason::DIVIDE_ERROR));
    r.run("K02", "undefined instruction kills the process", || {
        fault_case("ud2", kill_reason::INVALID_OPCODE)
    });
    r.run("K02", "privileged instruction in ring 3 kills the process (#GP)", || {
        fault_case("cli", kill_reason::GENERAL_PROTECTION)
    });
    r.run("C01", "writing through a read-only memory-object mapping kills the process", || {
        fault_case("ro_vmo_write", kill_reason::PAGE_FAULT)
    });
    r.run("K02", "int3 in ring 3 kills the process (breakpoint)", || {
        fault_case("int3", kill_reason::BREAKPOINT)
    });
    r.run("K02", "TF set before syscall: kernel survives the ring-0 #DB, process is terminated", || {
        fault_case("tf_syscall", kill_reason::DEBUG)
    });
    r.run("K02", "jump to a kernel address kills the process (page fault)", || {
        fault_case("kernel_rip_jump", kill_reason::PAGE_FAULT)
    });
    r.run("K02", "jump to a non-canonical address kills the process (#GP on hardware, #PF on TCG)", || {
        fault_case_any("noncanon_rip_jump", &[kill_reason::GENERAL_PROTECTION, kill_reason::PAGE_FAULT])
    });
    r.run("K02", "syscall with a non-canonical user rsp returns normally", || {
        let (mine, theirs) = sys::channel_create().map_err(|e| alloc::format!("channel: {e}"))?;
        let p = sys::spawn(ROOT, "bin/fault", CHILD_QUOTA, Some(theirs))
            .map_err(|e| alloc::format!("spawn: {e}"))?;
        sys::send(mine, b"noncanon_rsp_syscall", None).map_err(|e| alloc::format!("send: {e}"))?;
        let st = sys::wait(p).map_err(|e| alloc::format!("wait: {e}"))?;
        sys::handle_close(p).ok();
        sys::handle_close(mine).ok();
        expect_exit("fault", st, 0)
    });
    r.run("K02", "syscall with rsp pointing into the kernel returns normally", || {
        let (mine, theirs) = sys::channel_create().map_err(|e| alloc::format!("channel: {e}"))?;
        let p = sys::spawn(ROOT, "bin/fault", CHILD_QUOTA, Some(theirs))
            .map_err(|e| alloc::format!("spawn: {e}"))?;
        sys::send(mine, b"kernel_rsp_syscall", None).map_err(|e| alloc::format!("send: {e}"))?;
        let st = sys::wait(p).map_err(|e| alloc::format!("wait: {e}"))?;
        sys::handle_close(p).ok();
        sys::handle_close(mine).ok();
        expect_exit("fault", st, 0)
    });
    r.run(
        "K02",
        "malformed executables are rejected (bad entry, bad magic, truncated, huge segment)",
        || {
            for name in
                ["fixtures/bad_entry", "fixtures/bad_magic", "fixtures/truncated", "fixtures/huge_segment"]
            {
                match sys::spawn(ROOT, name, 4096, None) {
                    Err(Error::NoExec) | Err(Error::Quota) => {}
                    other => return Err(alloc::format!("{name}: got {other:?}")),
                }
            }
            Ok(())
        },
    );
    r.run("K02", "kernel pointer passed to a syscall is rejected, not dereferenced", || {
        let (mine, theirs) = sys::channel_create().map_err(|e| alloc::format!("channel: {e}"))?;
        let p = sys::spawn(ROOT, "bin/fault", CHILD_QUOTA, Some(theirs))
            .map_err(|e| alloc::format!("spawn: {e}"))?;
        sys::send(mine, b"kernel_syscall_ptr", None).map_err(|e| alloc::format!("send: {e}"))?;
        let st = sys::wait(p).map_err(|e| alloc::format!("wait: {e}"))?;
        sys::handle_close(p).ok();
        sys::handle_close(mine).ok();
        expect_exit("fault", st, 0)
    });

    r.run("K02", "syscall ABI negative tests (handles, rights, pointers)", || {
        let (mine, theirs) = sys::channel_create().map_err(|e| alloc::format!("channel: {e}"))?;
        let st = spawn_and_wait("bin/abi_negative", CHILD_QUOTA, Some(theirs))?;
        sys::handle_close(mine).ok();
        expect_exit("abi_negative", st, 0)
    });

    r.run("K02", "IPC echo round trips and handle transfer", || {
        let (mine, theirs) = sys::channel_create().map_err(|e| alloc::format!("channel: {e}"))?;
        let p = sys::spawn(ROOT, "bin/ipc_echo", CHILD_QUOTA, Some(theirs))
            .map_err(|e| alloc::format!("spawn: {e}"))?;
        let mut buf = [0u8; 256];
        for (i, msg) in ["ping", "space os", "context is a service"].iter().enumerate() {
            sys::send(mine, msg.as_bytes(), None).map_err(|e| alloc::format!("send {i}: {e}"))?;
            let (n, h) = sys::recv(mine, &mut buf, false).map_err(|e| alloc::format!("recv {i}: {e}"))?;
            let reply = core::str::from_utf8(&buf[..n]).unwrap_or("");
            if h.is_some() || reply != msg.to_uppercase() {
                return Err(alloc::format!("reply {i} was {reply:?}"));
            }
        }
        // Handle transfer: the child must answer on the endpoint we hand over.
        let (side_a, side_b) = sys::channel_create().map_err(|e| alloc::format!("channel2: {e}"))?;
        sys::send(mine, b"reply here", Some(side_b)).map_err(|e| alloc::format!("send with handle: {e}"))?;
        if sys::handle_info(side_b).is_ok() {
            return Err(String::from("transferred handle still present in sender"));
        }
        let (n, _) = sys::recv(side_a, &mut buf, false).map_err(|e| alloc::format!("recv on side_a: {e}"))?;
        if &buf[..n] != b"via transferred handle" {
            return Err(alloc::format!("side_a got {:?}", core::str::from_utf8(&buf[..n])));
        }
        sys::handle_close(side_a).ok();
        sys::handle_close(mine).ok(); // peer sees PeerClosed and exits 0
        let st = sys::wait(p).map_err(|e| alloc::format!("wait: {e}"))?;
        sys::handle_close(p).ok();
        expect_exit("ipc_echo", st, 0)
    });

    r.run("K02", "timer: sleep(50 ms) advances the clock", || {
        let t0 = sys::ticks_ms();
        sys::sleep_ms(50);
        let dt = sys::ticks_ms() - t0;
        if (50..1000).contains(&dt) { Ok(()) } else { Err(alloc::format!("slept {dt} ms")) }
    });

    r.run("K02", "runaway process is preempted and can be killed", || {
        let p = sys::spawn(ROOT, "bin/spin", CHILD_QUOTA, None).map_err(|e| alloc::format!("spawn: {e}"))?;
        let t0 = sys::ticks_ms();
        sys::sleep_ms(60);
        let dt = sys::ticks_ms() - t0;
        if dt > 2000 {
            return Err(alloc::format!("init starved by the spinner for {dt} ms"));
        }
        sys::kill(p).map_err(|e| alloc::format!("kill: {e}"))?;
        let st = sys::wait(p).map_err(|e| alloc::format!("wait: {e}"))?;
        sys::handle_close(p).ok();
        if st.kind == exit_kind::KILLED && st.reason == kill_reason::SIGNAL {
            Ok(())
        } else {
            Err(alloc::format!("spin ended with {st:?}"))
        }
    });

    r.run("K03", "memory quota is enforced and reusable", || {
        expect_exit("quota", spawn_and_wait("bin/quota", 100, None)?, 0)
    });

    r.run("K03", "50 spawn/exit cycles leak no frames and no kernel heap", || {
        for _ in 0..5 {
            cycle()?; // warm up allocator caches
        }
        let before = stats()?;
        for i in 0..CYCLES {
            cycle().map_err(|e| alloc::format!("cycle {i}: {e}"))?;
        }
        let after = stats()?;
        println!(
            "[init] frames free before={} after={} ; heap used before={} after={} ; switches={}",
            before.frames_free, after.frames_free, before.heap_used, after.heap_used, after.context_switches
        );
        if after.frames_free != before.frames_free {
            return Err(alloc::format!(
                "frame leak of {} pages over {CYCLES} cycles",
                before.frames_free as i64 - after.frames_free as i64
            ));
        }
        if after.heap_used != before.heap_used {
            return Err(alloc::format!(
                "kernel heap drift of {} bytes over {CYCLES} cycles",
                after.heap_used as i64 - before.heap_used as i64
            ));
        }
        if after.processes_live != 1 {
            return Err(alloc::format!("{} live processes after cycles", after.processes_live));
        }
        Ok(())
    });

    r.run("K03", "20 kill-while-blocked cycles leak no frames and no kernel heap", || {
        for _ in 0..3 {
            kill_blocked_cycle()?;
        }
        let before = stats()?;
        for i in 0..20 {
            kill_blocked_cycle().map_err(|e| alloc::format!("cycle {i}: {e}"))?;
        }
        let after = stats()?;
        println!(
            "[init] frames free before={} after={} ; heap used before={} after={}",
            before.frames_free, after.frames_free, before.heap_used, after.heap_used
        );
        if after.frames_free != before.frames_free
            || after.heap_used != before.heap_used
            || after.processes_live != 1
        {
            return Err(alloc::format!(
                "leak: frames {}->{} heap {}->{} live {}",
                before.frames_free,
                after.frames_free,
                before.heap_used,
                after.heap_used,
                after.processes_live
            ));
        }
        Ok(())
    });

    r.run("K03", "20 kill-while-sleeping cycles release the thread at once, not at wake-up", || {
        no_leak_over(20, "kill while sleeping", || blocked_kill_cycle("sleep", None))
    });

    r.run("K03", "20 kill-while-blocked-in-recv cycles leak nothing (peer stays open)", || {
        no_leak_over(20, "kill while in recv", || blocked_kill_cycle("recv", None))
    });

    r.run("K03", "20 kill-while-waiting cycles leak nothing although the target keeps running", || {
        let target = sys::spawn(ROOT, "bin/spin", CHILD_QUOTA, None)
            .map_err(|e| alloc::format!("spawn target: {e}"))?;
        let res = no_leak_over(20, "kill while in wait", || {
            let dup = sys::handle_dup(target, rights::WAIT | rights::TRANSFER)
                .map_err(|e| alloc::format!("dup target: {e}"))?;
            blocked_kill_cycle("wait", Some(dup))
        });
        sys::kill(target).ok();
        sys::wait(target).ok();
        sys::handle_close(target).ok();
        res
    });

    r.run("K02", "a full handle table refuses spawn instead of orphaning the child", || {
        let mut spare = Vec::new();
        loop {
            match sys::handle_dup(ROOT, rights::STATS) {
                Ok(h) => spare.push(h),
                Err(Error::TooManyHandles) => break,
                Err(e) => return Err(alloc::format!("dup: {e}")),
            }
            if spare.len() > 1024 {
                return Err(String::from("handle table never filled up"));
            }
        }
        let spawn_result = sys::spawn(ROOT, "bin/hello", CHILD_QUOTA, None);
        for h in spare.drain(..) {
            sys::handle_close(h).map_err(|e| alloc::format!("close spare: {e}"))?;
        }
        match spawn_result {
            Err(Error::TooManyHandles) => {}
            other => return Err(alloc::format!("spawn with a full table gave {other:?}")),
        }
        // Nothing may have been created behind our back.
        sys::sleep_ms(20);
        let s = stats()?;
        if s.processes_live != 1 {
            return Err(alloc::format!("{} live processes after the refused spawn", s.processes_live));
        }
        expect_exit("hello", spawn_and_wait("bin/hello", CHILD_QUOTA, None)?, 0)
    });

    r.run("D01", "SHA-256 in the guest matches the published test vectors", || {
        let empty = sha256::to_hex(&sha256::digest(b""));
        let abc = sha256::to_hex(&sha256::digest(b"abc"));
        let million_a = {
            let mut h = sha256::Sha256::new();
            for _ in 0..1000 {
                h.update(&[b'a'; 1000]);
            }
            sha256::to_hex(&h.finish())
        };
        let want_empty = b"e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
        let want_abc = b"ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";
        let want_million = b"cdc76e5c9914fb9281a1c7e284d73e67f1809a48a497200e046d39ccc7112cd0";
        if &empty != want_empty || &abc != want_abc || &million_a != want_million {
            return Err(String::from("SHA-256 implementation disagrees with the FIPS vectors"));
        }
        Ok(())
    });

    r.run_if(
        disk,
        "no disk on this machine",
        "D01",
        "model file read from the guest disk matches the manifest checksum",
        || {
            let mut manifest = alloc::string::String::new();
            stream_file("/spaceos/manifest.txt", |chunk| {
                manifest.push_str(core::str::from_utf8(chunk).unwrap_or(""));
            })?;
            let want_size: u64 = manifest_value(&manifest, "size")
                .and_then(|v| v.parse().ok())
                .ok_or_else(|| String::from("manifest has no size"))?;
            let want_hash =
                manifest_value(&manifest, "sha256").ok_or_else(|| String::from("manifest has no sha256"))?;
            let path =
                manifest_value(&manifest, "path").ok_or_else(|| String::from("manifest has no path"))?;

            let mut hasher = sha256::Sha256::new();
            let read = stream_file(path, |chunk| hasher.update(chunk))?;
            let got = sha256::to_hex(&hasher.finish());
            let got = core::str::from_utf8(&got).unwrap_or("");
            if read != want_size {
                return Err(alloc::format!("{path}: {read} bytes read, manifest says {want_size}"));
            }
            if got != want_hash {
                return Err(alloc::format!("{path}: sha256 {got}, manifest says {want_hash}"));
            }
            println!("[init] model {path}: {read} bytes, sha256 {got} verified from the guest disk");
            Ok(())
        },
    );

    r.run_if(
        disk,
        "no disk on this machine",
        "D01",
        "file API rejects bad paths, missing rights and bad buffers",
        || {
            match sys::fs_open(ROOT, "/spaceos/not_here.bin") {
                Err(Error::NotFound) => {}
                other => return Err(alloc::format!("missing file gave {other:?}")),
            }
            match sys::fs_open(ROOT, "/spaceos") {
                Ok(h) => {
                    // A directory has no size and must not be readable as a file.
                    let st = sys::fs_stat(h).map_err(|e| alloc::format!("stat dir: {e}"))?;
                    sys::handle_close(h).ok();
                    if st.size != 0 {
                        return Err(String::from("directory reported a non-zero size"));
                    }
                }
                Err(Error::NotFound) | Err(Error::Invalid) => {}
                Err(e) => return Err(alloc::format!("opening a directory gave {e}")),
            }
            // A regular file is not a directory: the walk must stop there rather than
            // decode the file's own bytes as directory entries.
            match sys::fs_open(ROOT, "/spaceos/manifest.txt/anything") {
                Err(Error::NotFound) => {}
                other => return Err(alloc::format!("walking through a file gave {other:?}")),
            }
            let no_fs = sys::handle_dup(ROOT, rights::STATS).map_err(|e| alloc::format!("dup: {e}"))?;
            let denied = sys::fs_open(no_fs, "/spaceos/manifest.txt");
            sys::handle_close(no_fs).ok();
            match denied {
                Err(Error::Denied) => {}
                other => return Err(alloc::format!("open without the FS right gave {other:?}")),
            }
            let f = sys::fs_open(ROOT, "/spaceos/manifest.txt").map_err(|e| alloc::format!("open: {e}"))?;
            let bad = decode(unsafe { sys::raw(nr::FS_READ, f as u64, 0, 0xFFFF_8000_0000_0000, 64, 0, 0) });
            let past_end = {
                let mut buf = [0u8; 16];
                sys::fs_read(f, 1 << 40, &mut buf)
            };
            let as_channel = sys::fs_stat(handle::BOOTSTRAP);
            sys::handle_close(f).ok();
            if bad != Err(Error::Fault) {
                return Err(alloc::format!("read into kernel memory gave {bad:?}"));
            }
            if past_end != Ok(0) {
                return Err(alloc::format!("read past end of file gave {past_end:?}"));
            }
            if as_channel != Err(Error::Denied) {
                return Err(alloc::format!("stat on a channel handle gave {as_channel:?}"));
            }
            Ok(())
        },
    );

    r.run("C01", "Compute ABI: version negotiation, device query and queue lifetime", || {
        let (mine, theirs) = sys::channel_create().map_err(|e| alloc::format!("channel: {e}"))?;
        let svc = sys::spawn(ROOT, "bin/spacecompute", COMPUTE_QUOTA, Some(theirs))
            .map_err(|e| alloc::format!("spawn compute: {e}"))?;

        // Anything before HELLO must be refused.
        let early = compute_raw(mine, &ComputeRequest { kind: creq::DEVICE_QUERY, ..Default::default() })?;
        expect_status("request before HELLO", early.status, Error::Denied)?;

        // A client speaking a different ABI version must be refused, not guessed at.
        let bad = compute_raw(
            mine,
            &ComputeRequest {
                kind: creq::HELLO,
                abi_version: compute_abi::ABI_VERSION + 99,
                ..Default::default()
            },
        )?;
        expect_status("HELLO with the wrong ABI version", bad.status, Error::Invalid)?;

        let c = Compute::connect(mine).map_err(|e| alloc::format!("connect: {e}"))?;
        let (backend_id, abi, limit) = c.device_query().map_err(|e| alloc::format!("device_query: {e}"))?;
        if backend_id != compute_abi::backend::CPU || abi != compute_abi::ABI_VERSION {
            return Err(alloc::format!("device query reported backend {backend_id}, abi {abi}"));
        }
        println!("[init] compute device: CPU backend, ABI v{abi}, buffers up to {} MiB", limit >> 20);

        // Queues are finite and their ids must be validated.
        let mut queues = Vec::new();
        loop {
            match c.queue_create() {
                Ok(q) => queues.push(q),
                Err(Error::NoMemory) => break,
                Err(e) => return Err(alloc::format!("queue_create: {e}")),
            }
            if queues.len() > 64 {
                return Err(String::from("queue table never filled up"));
            }
        }
        if queues.len() != compute_abi::MAX_QUEUES {
            return Err(alloc::format!(
                "{} queues created, expected {}",
                queues.len(),
                compute_abi::MAX_QUEUES
            ));
        }
        let unknown = c
            .call(&ComputeRequest {
                kind: creq::SUBMIT,
                queue: 99,
                op: ComputeOp { kind: cop::FILL, ..Default::default() },
                ..Default::default()
            })
            .map_err(|e| alloc::format!("submit: {e}"))?;
        expect_status("submit on an unknown queue", unknown.status, Error::BadHandle)?;

        c.shutdown().map_err(|e| alloc::format!("shutdown: {e}"))?;
        let st = sys::wait(svc).map_err(|e| alloc::format!("wait: {e}"))?;
        sys::handle_close(svc).ok();
        sys::handle_close(mine).ok();
        expect_exit("spacecompute", st, 0)
    });

    r.run("C01", "Compute ABI: shared buffers, operations, bounds, unsupported ops", || {
        let (mine, theirs) = sys::channel_create().map_err(|e| alloc::format!("channel: {e}"))?;
        let svc = sys::spawn(ROOT, "bin/spacecompute", COMPUTE_QUOTA, Some(theirs))
            .map_err(|e| alloc::format!("spawn compute: {e}"))?;
        let c = Compute::connect(mine).map_err(|e| alloc::format!("connect: {e}"))?;
        let q = c.queue_create().map_err(|e| alloc::format!("queue: {e}"))?;

        let a = c.buffer_create(4096).map_err(|e| alloc::format!("buffer a: {e}"))?;
        let b = c.buffer_create(4096).map_err(|e| alloc::format!("buffer b: {e}"))?;
        let dst = c.buffer_create(4096).map_err(|e| alloc::format!("buffer dst: {e}"))?;
        // SAFETY: nothing else touches these buffers while we fill them.
        let (av, bv, dv) = unsafe { (a.as_f32_mut(), b.as_f32_mut(), dst.as_f32_mut()) };

        // The service sees what we write: shared memory, not copies.
        for i in 0..16 {
            av[i] = i as f32;
            bv[i] = (2 * i) as f32;
        }
        c.run(
            q,
            ComputeOp {
                kind: cop::ADD,
                dst: dst.reference(),
                a: a.reference(),
                b: b.reference(),
                dims: [16, 0, 0, 0],
                ..Default::default()
            },
        )
        .map_err(|e| alloc::format!("add: {e}"))?;
        for (i, &got) in dv.iter().take(16).enumerate() {
            if !close_enough(got, (3 * i) as f32) {
                return Err(alloc::format!("add: element {i} is {got}"));
            }
        }

        // MATMUL against a reference computed here.
        let (m, k, n) = (4usize, 8usize, 3usize);
        for (i, v) in av.iter_mut().enumerate().take(m * k) {
            *v = (i % 7) as f32 - 3.0;
        }
        for (i, v) in bv.iter_mut().enumerate().take(n * k) {
            *v = ((i % 5) as f32) * 0.25 - 0.5;
        }
        c.run(
            q,
            ComputeOp {
                kind: cop::MATMUL,
                dst: dst.reference(),
                a: a.reference(),
                b: b.reference(),
                dims: [m as u32, k as u32, n as u32, 0],
                ..Default::default()
            },
        )
        .map_err(|e| alloc::format!("matmul: {e}"))?;
        for row in 0..m {
            for col in 0..n {
                let mut want = 0.0f32;
                for i in 0..k {
                    want += av[row * k + i] * bv[col * k + i];
                }
                if !close_enough(dv[row * n + col], want) {
                    return Err(alloc::format!(
                        "matmul[{row},{col}] = {}, expected {want}",
                        dv[row * n + col]
                    ));
                }
            }
        }

        // ARGMAX returns its result in the reply value.
        for (i, v) in av.iter_mut().enumerate().take(32) {
            *v = -(i as f32);
        }
        av[19] = 100.0;
        let idx = c
            .run(
                q,
                ComputeOp { kind: cop::ARGMAX, a: a.reference(), dims: [32, 0, 0, 0], ..Default::default() },
            )
            .map_err(|e| alloc::format!("argmax: {e}"))?;
        if idx != 19 {
            return Err(alloc::format!("argmax returned {idx}"));
        }

        // Bounds and kinds are checked at submit time.
        let bad_id = c
            .call(&ComputeRequest {
                kind: creq::SUBMIT,
                queue: q,
                op: ComputeOp {
                    kind: cop::FILL,
                    dst: ComputeBufferRef::whole(250, 16),
                    dims: [4, 0, 0, 0],
                    ..Default::default()
                },
                ..Default::default()
            })
            .map_err(|e| alloc::format!("submit: {e}"))?;
        expect_status("op on an unknown buffer", bad_id.status, Error::BadHandle)?;

        let past_end = c
            .call(&ComputeRequest {
                kind: creq::SUBMIT,
                queue: q,
                op: ComputeOp {
                    kind: cop::FILL,
                    dst: ComputeBufferRef { id: a.id, _pad: 0, offset: 4096, len: 64 },
                    dims: [16, 0, 0, 0],
                    ..Default::default()
                },
                ..Default::default()
            })
            .map_err(|e| alloc::format!("submit: {e}"))?;
        expect_status("op past the end of a buffer", past_end.status, Error::Invalid)?;

        let too_small = c
            .call(&ComputeRequest {
                kind: creq::SUBMIT,
                queue: q,
                op: ComputeOp {
                    kind: cop::FILL,
                    dst: ComputeBufferRef { id: a.id, _pad: 0, offset: 0, len: 16 },
                    dims: [4096, 0, 0, 0],
                    ..Default::default()
                },
                ..Default::default()
            })
            .map_err(|e| alloc::format!("submit: {e}"))?;
        expect_status("op larger than its buffer slice", too_small.status, Error::MsgSize)?;

        let unsupported = c
            .call(&ComputeRequest {
                kind: creq::SUBMIT,
                queue: q,
                op: ComputeOp { kind: 0xDEAD_BEEF, ..Default::default() },
                ..Default::default()
            })
            .map_err(|e| alloc::format!("submit: {e}"))?;
        expect_status("unsupported operation", unsupported.status, Error::NoSys)?;

        let unknown_ticket = c
            .call(&ComputeRequest { kind: creq::WAIT, ticket: 4242, timeout_ms: 10, ..Default::default() })
            .map_err(|e| alloc::format!("wait: {e}"))?;
        expect_status("wait on an unknown ticket", unknown_ticket.status, Error::BadHandle)?;

        // A zero element count would make every kernel read element 0 of an empty
        // slice. It has to be refused at submit, not panic the service.
        for kind in [cop::SOFTMAX, cop::ARGMAX, cop::FILL, cop::RMSNORM, cop::EMBED] {
            let empty = c
                .call(&ComputeRequest {
                    kind: creq::SUBMIT,
                    queue: q,
                    op: ComputeOp {
                        kind,
                        dst: dst.reference(),
                        a: a.reference(),
                        b: b.reference(),
                        dims: [0, 0, 0, 0],
                        ..Default::default()
                    },
                    ..Default::default()
                })
                .map_err(|e| alloc::format!("submit: {e}"))?;
            expect_status("operation with a zero dimension", empty.status, Error::Invalid)?;
        }

        // Releasing a buffer a pending ticket still points at must be refused.
        let pending = c
            .submit(q, ComputeOp { kind: cop::SPIN, dims: [1, 0, 0, 0], ..Default::default() })
            .map_err(|e| alloc::format!("submit spin: {e}"))?;
        let busy = c
            .call(&ComputeRequest {
                kind: creq::SUBMIT,
                queue: q,
                op: ComputeOp {
                    kind: cop::FILL,
                    dst: a.reference(),
                    dims: [4, 0, 0, 0],
                    ..Default::default()
                },
                ..Default::default()
            })
            .map_err(|e| alloc::format!("submit fill: {e}"))?;
        let busy_ticket = busy.ticket;
        let refused = c
            .call(&ComputeRequest { kind: creq::BUFFER_RELEASE, buffer: a.id, ..Default::default() })
            .map_err(|e| alloc::format!("release: {e}"))?;
        expect_status("release of a buffer in use", refused.status, Error::WouldBlock)?;
        c.cancel(busy_ticket).map_err(|e| alloc::format!("cancel: {e}"))?;
        c.wait_raw(busy_ticket, 100).map_err(|e| alloc::format!("reap: {e}"))?;
        c.wait(pending, 1000).map_err(|e| alloc::format!("spin: {e}"))?;

        // Handles a client transfers with a request are closed by the service; a
        // stream of them must not exhaust its handle table.
        for i in 0..(MAX_HANDLES * 2) {
            let extra = sys::handle_dup(ROOT, rights::STATS | rights::TRANSFER)
                .map_err(|e| alloc::format!("dup: {e}"))?;
            if sys::send(
                mine,
                as_request_bytes(&ComputeRequest { kind: creq::DEVICE_QUERY, ..Default::default() }),
                Some(extra),
            )
            .is_err()
            {
                return Err(alloc::format!("send with a transferred handle failed at {i}"));
            }
            let mut reply = [0u8; core::mem::size_of::<ComputeResponse>()];
            let (n, _) = sys::recv(mine, &mut reply, false).map_err(|e| alloc::format!("reply: {e}"))?;
            if n != reply.len() {
                return Err(alloc::format!("short reply at {i}"));
            }
        }

        for buf in [a, b, dst] {
            c.buffer_release(&buf).map_err(|e| alloc::format!("release: {e}"))?;
        }
        c.shutdown().map_err(|e| alloc::format!("shutdown: {e}"))?;
        let st = sys::wait(svc).map_err(|e| alloc::format!("wait: {e}"))?;
        sys::handle_close(svc).ok();
        sys::handle_close(mine).ok();
        expect_exit("spacecompute", st, 0)
    });

    r.run("C01", "Compute ABI: a long operation times out, resumes and can be cancelled", || {
        let (mine, theirs) = sys::channel_create().map_err(|e| alloc::format!("channel: {e}"))?;
        let svc = sys::spawn(ROOT, "bin/spacecompute", COMPUTE_QUOTA, Some(theirs))
            .map_err(|e| alloc::format!("spawn compute: {e}"))?;
        let c = Compute::connect(mine).map_err(|e| alloc::format!("connect: {e}"))?;
        let q = c.queue_create().map_err(|e| alloc::format!("queue: {e}"))?;

        // A ticket that cannot finish inside the deadline reports RUNNING, keeps its
        // progress, and finishes on a later wait.
        let long = ComputeOp { kind: cop::SPIN, dims: [40_000_000, 0, 0, 0], ..Default::default() };
        let t = c.submit(q, long).map_err(|e| alloc::format!("submit: {e}"))?;
        let first = c.wait_raw(t, 1).map_err(|e| alloc::format!("wait: {e}"))?;
        expect_status("wait that runs out of time", first.status, Error::WouldBlock)?;
        if first.flags != compute_abi::state::RUNNING {
            return Err(alloc::format!("timed-out wait reported flags {}", first.flags));
        }
        let value = c.wait(t, 120_000).map_err(|e| alloc::format!("second wait: {e}"))?;
        if value != 40_000_000 {
            return Err(alloc::format!("resumed ticket produced {value}"));
        }

        // A ticket cancelled between waits reports CANCELLED and is forgotten.
        let t2 = c.submit(q, long).map_err(|e| alloc::format!("submit 2: {e}"))?;
        let partial = c.wait_raw(t2, 1).map_err(|e| alloc::format!("wait 2: {e}"))?;
        expect_status("wait before cancelling", partial.status, Error::WouldBlock)?;
        c.cancel(t2).map_err(|e| alloc::format!("cancel: {e}"))?;
        let after = c.wait_raw(t2, 1000).map_err(|e| alloc::format!("wait after cancel: {e}"))?;
        if after.status != 0 || after.flags != compute_abi::state::CANCELLED {
            return Err(alloc::format!(
                "cancelled ticket reported status {} flags {}",
                after.status,
                after.flags
            ));
        }
        let gone = c
            .call(&ComputeRequest { kind: creq::CANCEL, ticket: t2, ..Default::default() })
            .map_err(|e| alloc::format!("cancel twice: {e}"))?;
        expect_status("cancelling a forgotten ticket", gone.status, Error::BadHandle)?;

        c.shutdown().map_err(|e| alloc::format!("shutdown: {e}"))?;
        let st = sys::wait(svc).map_err(|e| alloc::format!("wait: {e}"))?;
        sys::handle_close(svc).ok();
        sys::handle_close(mine).ok();
        expect_exit("spacecompute", st, 0)
    });

    r.run("C01", "a mapped memory object outlives the close of its last handle", || {
        const LEN: usize = 64 * 1024;
        const CHURN: usize = 1024 * 1024;
        let before = stats()?;
        let h = sys::vmo_create(LEN).map_err(|e| alloc::format!("vmo_create: {e}"))?;
        let ptr = sys::vmo_map(h, false).map_err(|e| alloc::format!("vmo_map: {e}"))?;
        // Drop every handle to the object: only the mapping refers to it now.
        sys::handle_close(h).map_err(|e| alloc::format!("close: {e}"))?;
        if sys::vmo_size(h).is_ok() {
            return Err(String::from("the closed handle is still usable"));
        }
        // SAFETY: LEN bytes mapped read-write in this address space.
        let buf = unsafe { core::slice::from_raw_parts_mut(ptr, LEN) };
        for (i, b) in buf.iter_mut().enumerate() {
            *b = (i % 251) as u8;
        }
        // Churn the frame allocator. If closing the handle had freed the frames they
        // would be handed out here, zeroed, and the pattern below would be gone.
        let churn = sys::mem_map(CHURN).map_err(|e| alloc::format!("mem_map: {e}"))?;
        // SAFETY: CHURN bytes mapped read-write in this address space.
        unsafe { core::ptr::write_bytes(churn, 0xAA, CHURN) };
        sys::mem_unmap(churn, CHURN).map_err(|e| alloc::format!("mem_unmap: {e}"))?;
        if let Some((i, b)) = buf.iter().enumerate().find(|&(i, b)| *b != (i % 251) as u8) {
            return Err(alloc::format!("mapping corrupted at byte {i}: {b:#04x}"));
        }
        sys::mem_unmap(ptr, LEN).map_err(|e| alloc::format!("unmap object: {e}"))?;
        let after = stats()?;
        if after.frames_free != before.frames_free {
            return Err(alloc::format!("leak: frames {} -> {}", before.frames_free, after.frames_free));
        }
        Ok(())
    });

    r.run("C01", "compute service teardown returns every frame it allocated", || {
        let before = stats()?;
        for i in 0..3 {
            let (mine, theirs) = sys::channel_create().map_err(|e| alloc::format!("channel: {e}"))?;
            let svc = sys::spawn(ROOT, "bin/spacecompute", COMPUTE_QUOTA, Some(theirs))
                .map_err(|e| alloc::format!("spawn: {e}"))?;
            let c = Compute::connect(mine).map_err(|e| alloc::format!("connect: {e}"))?;
            let q = c.queue_create().map_err(|e| alloc::format!("queue: {e}"))?;
            let buf = c.buffer_create(256 * 1024).map_err(|e| alloc::format!("buffer: {e}"))?;
            c.run(
                q,
                ComputeOp {
                    kind: cop::FILL,
                    dst: buf.reference(),
                    dims: [65536, 0, 0, 0],
                    scalar: 1.5,
                    ..Default::default()
                },
            )
            .map_err(|e| alloc::format!("fill: {e}"))?;
            // SAFETY: only this process touches the buffer right now.
            let v = unsafe { buf.as_f32_mut() };
            if !close_enough(v[65535], 1.5) {
                return Err(String::from("shared buffer did not receive the fill"));
            }
            // Deliberately leave the buffer mapped in cycle 1 so teardown has to
            // reclaim it rather than relying on an orderly release.
            if i != 0 {
                c.buffer_release(&buf).map_err(|e| alloc::format!("release: {e}"))?;
            }
            c.shutdown().map_err(|e| alloc::format!("shutdown: {e}"))?;
            let st = sys::wait(svc).map_err(|e| alloc::format!("wait: {e}"))?;
            sys::handle_close(svc).ok();
            sys::mem_unmap(buf.ptr, buf.len).ok();
            sys::handle_close(buf.handle).ok();
            sys::handle_close(mine).ok();
            expect_exit("spacecompute", st, 0)?;
        }
        let after = stats()?;
        println!(
            "[init] compute cycles: frames free {} -> {}, heap used {} -> {}",
            before.frames_free, after.frames_free, before.heap_used, after.heap_used
        );
        if after.frames_free != before.frames_free || after.heap_used != before.heap_used {
            return Err(alloc::format!(
                "leak: frames {} -> {}, heap {} -> {}",
                before.frames_free,
                after.frames_free,
                before.heap_used,
                after.heap_used
            ));
        }
        Ok(())
    });

    r.run_if(
        disk,
        "no disk on this machine",
        "A01",
        "native inference: a small model generates 128 tokens matching the pinned baseline",
        || {
            let (svc_client, svc_server) =
                sys::channel_create().map_err(|e| alloc::format!("channel: {e}"))?;
            let compute = sys::spawn(ROOT, "bin/spacecompute", COMPUTE_QUOTA, Some(svc_server))
                .map_err(|e| alloc::format!("spawn compute: {e}"))?;
            let (ai_mine, ai_theirs) = sys::channel_create().map_err(|e| alloc::format!("channel: {e}"))?;
            let ai = sys::spawn(ROOT, "bin/spaceai", AI_QUOTA, Some(ai_theirs))
                .map_err(|e| alloc::format!("spawn ai: {e}"))?;

            // The runtime gets exactly two capabilities: a compute connection and
            // read-only file system access. It cannot spawn, kill or inspect anything.
            sys::send(ai_mine, b"compute", Some(svc_client))
                .map_err(|e| alloc::format!("pass compute: {e}"))?;
            let fs = sys::handle_dup(ROOT, rights::FS | rights::TRANSFER)
                .map_err(|e| alloc::format!("dup fs: {e}"))?;
            sys::send(ai_mine, b"fs", Some(fs)).map_err(|e| alloc::format!("pass fs: {e}"))?;

            let st = sys::wait(ai).map_err(|e| alloc::format!("wait ai: {e}"))?;
            sys::handle_close(ai).ok();
            sys::kill(compute).ok();
            sys::wait(compute).ok();
            sys::handle_close(compute).ok();
            sys::handle_close(ai_mine).ok();
            expect_exit("spaceai", st, 0)
        },
    );

    r.run(
        "U01",
        "session service survives a crashed worker and keeps serving Stop and the file manager",
        || {
            const SHELL_QUOTA: u64 = 192;
            let (mine, theirs) = sys::channel_create().map_err(|e| alloc::format!("channel: {e}"))?;
            let shell = sys::spawn(ROOT, "bin/spaceshell", SHELL_QUOTA, Some(theirs))
                .map_err(|e| alloc::format!("spawn shell: {e}"))?;
            // The session gets exactly two powers: start jobs and list files. No
            // shutdown, no kernel stats, no debug - a crashed session cannot take the
            // machine down even if it wanted to.
            let shell_root = sys::handle_dup(ROOT, rights::SPAWN | rights::FS | rights::TRANSFER)
                .map_err(|e| alloc::format!("dup root: {e}"))?;
            let s = Session::open(mine, shell_root).map_err(|e| alloc::format!("open session: {e}"))?;

            let check_alive = |what: &str| -> Result<(), String> {
                let st = s.status().map_err(|e| alloc::format!("status after {what}: {e}"))?;
                st.result().map_err(|e| alloc::format!("status after {what}: {e}"))?;
                if disk {
                    let l = s.list("/spaceos").map_err(|e| alloc::format!("list after {what}: {e}"))?;
                    let n = l.result().map_err(|e| alloc::format!("list after {what}: {e}"))?;
                    if n == 0 {
                        return Err(alloc::format!("file manager listed nothing after {what}"));
                    }
                }
                Ok(())
            };

            check_alive("startup")?;

            // 1. The worker faults. The kernel kills it; the session must report the
            //    reason and carry on.
            s.run(shell_job::CRASH).and_then(|r| r.result()).map_err(|e| alloc::format!("run crash: {e}"))?;
            let st = wait_for_worker(&s, worker_state::CRASHED, 5_000)?;
            if st.reason != kill_reason::PAGE_FAULT {
                return Err(alloc::format!(
                    "crashed worker reported reason {} ({}), expected PAGE_FAULT",
                    st.reason,
                    kill_reason::name(st.reason)
                ));
            }
            check_alive("a crashed worker")?;

            // 2. A worker wedged in a loop that never enters the kernel again. The
            //    session has to answer while it spins, and Stop has to end it.
            s.run(shell_job::HANG).and_then(|r| r.result()).map_err(|e| alloc::format!("run hang: {e}"))?;
            sys::sleep_ms(50);
            let during = s.status().map_err(|e| alloc::format!("status during hang: {e}"))?;
            during.result().map_err(|e| alloc::format!("status during hang: {e}"))?;
            if during.state != worker_state::RUNNING {
                return Err(alloc::format!(
                    "wedged worker reported state {}, expected running",
                    during.state
                ));
            }
            check_alive("a wedged worker")?;
            let stopped = s.stop().map_err(|e| alloc::format!("stop hang: {e}"))?;
            stopped.result().map_err(|e| alloc::format!("stop hang: {e}"))?;
            if stopped.state != worker_state::STOPPED {
                return Err(alloc::format!("stop left state {}, expected stopped", stopped.state));
            }
            check_alive("stopping a wedged worker")?;

            // 3. A worker blocked in a syscall is just as stoppable.
            s.run(shell_job::SLOW).and_then(|r| r.result()).map_err(|e| alloc::format!("run slow: {e}"))?;
            sys::sleep_ms(20);
            let stopped = s.stop().map_err(|e| alloc::format!("stop slow: {e}"))?;
            stopped.result().map_err(|e| alloc::format!("stop slow: {e}"))?;
            if stopped.state != worker_state::STOPPED {
                return Err(alloc::format!("stop of a sleeping worker left state {}", stopped.state));
            }

            // 4. Stop with nothing running is refused, not a crash.
            let idle_stop = s.stop().map_err(|e| alloc::format!("stop idle: {e}"))?;
            expect_status("stop with no worker", idle_stop.status, Error::NotFound)?;

            // 5. A job that simply finishes still finishes.
            s.run(shell_job::OK).and_then(|r| r.result()).map_err(|e| alloc::format!("run ok: {e}"))?;
            let done = wait_for_worker(&s, worker_state::DONE, 10_000)?;
            if done.reason != 0 {
                return Err(alloc::format!("a clean job reported kill reason {}", done.reason));
            }
            check_alive("a completed job")?;

            let final_status = s.status().map_err(|e| alloc::format!("final status: {e}"))?;
            println!("[init] session served {} commands and outlived 4 workers", final_status.served);
            s.quit().and_then(|r| r.result()).map_err(|e| alloc::format!("quit: {e}"))?;
            let st = sys::wait(shell).map_err(|e| alloc::format!("wait shell: {e}"))?;
            sys::handle_close(shell).ok();
            sys::handle_close(mine).ok();
            expect_exit("spaceshell", st, 0)
        },
    );

    r.run("U01", "a session cannot exceed the capabilities it was given", || {
        const SHELL_QUOTA: u64 = 192;
        let (mine, theirs) = sys::channel_create().map_err(|e| alloc::format!("channel: {e}"))?;
        let shell = sys::spawn(ROOT, "bin/spaceshell", SHELL_QUOTA, Some(theirs))
            .map_err(|e| alloc::format!("spawn shell: {e}"))?;
        // Without the FS right the file manager is refused; jobs still run. The
        // shell has no way to widen what it was handed.
        let narrow = sys::handle_dup(ROOT, rights::SPAWN | rights::TRANSFER)
            .map_err(|e| alloc::format!("dup root: {e}"))?;
        let s = Session::open(mine, narrow).map_err(|e| alloc::format!("open session: {e}"))?;
        let denied = s.list("/spaceos").map_err(|e| alloc::format!("list: {e}"))?;
        expect_status("list without the FS right", denied.status, Error::Denied)?;
        s.run(shell_job::OK).and_then(|r| r.result()).map_err(|e| alloc::format!("run ok: {e}"))?;
        wait_for_worker(&s, worker_state::DONE, 10_000)?;
        let unknown = s.call(0xFFFF_FFFF, "").map_err(|e| alloc::format!("unknown command: {e}"))?;
        expect_status("unknown session command", unknown.status, Error::NoSys)?;
        s.quit().and_then(|r| r.result()).map_err(|e| alloc::format!("quit: {e}"))?;
        let st = sys::wait(shell).map_err(|e| alloc::format!("wait shell: {e}"))?;
        sys::handle_close(shell).ok();
        sys::handle_close(mine).ok();
        expect_exit("spaceshell", st, 0)
    });

    r.run_if(disk, "no disk on this machine", "U01", "the file manager lists the guest volume", || {
        let mut entries = [DirEntry::default(); 16];
        let n = sys::fs_list(ROOT, "/", &mut entries).map_err(|e| alloc::format!("list /: {e}"))?;
        if !entries.iter().take(n).any(|e| e.is_dir != 0 && e.name() == "SPACEOS") {
            return Err(String::from("the volume root does not contain SPACEOS/"));
        }
        let n = sys::fs_list(ROOT, "/spaceos", &mut entries).map_err(|e| alloc::format!("list: {e}"))?;
        let model = entries
            .iter()
            .take(n)
            .find(|e| e.name() == "MODEL.SLM")
            .ok_or_else(|| String::from("model.slm missing from the listing"))?;
        let f = sys::fs_open(ROOT, "/spaceos/model.slm").map_err(|e| alloc::format!("open: {e}"))?;
        let stat = sys::fs_stat(f).map_err(|e| alloc::format!("stat: {e}"))?;
        sys::handle_close(f).ok();
        if model.size != stat.size {
            return Err(alloc::format!("listing says {} bytes, stat says {}", model.size, stat.size));
        }
        // Listing is a directory operation: a file is not a directory, and the right
        // is still required.
        match sys::fs_list(ROOT, "/spaceos/model.slm", &mut entries) {
            Err(Error::Invalid) => {}
            other => return Err(alloc::format!("listing a file gave {other:?}")),
        }
        let no_fs = sys::handle_dup(ROOT, rights::STATS).map_err(|e| alloc::format!("dup: {e}"))?;
        let denied = sys::fs_list(no_fs, "/spaceos", &mut entries);
        sys::handle_close(no_fs).ok();
        match denied {
            Err(Error::Denied) => {}
            other => return Err(alloc::format!("listing without the FS right gave {other:?}")),
        }
        Ok(())
    });

    r.run_if(
        disk,
        "no disk on this machine",
        "G01",
        "agent reads, patches and checks inside its workspace",
        || {
            const BROKER_QUOTA: u64 = 256;
            const AGENT_QUOTA: u64 = 256;
            let (op, op_broker) = sys::channel_create().map_err(|e| alloc::format!("channel: {e}"))?;
            let broker = sys::spawn(ROOT, "bin/spacebroker", BROKER_QUOTA, Some(op_broker))
                .map_err(|e| alloc::format!("spawn broker: {e}"))?;
            // The broker is the only process in this picture with a file capability, and
            // it is narrowed to FS: it cannot spawn, shut down, or read kernel state.
            let broker_root = sys::handle_dup(ROOT, rights::FS | rights::TRANSFER)
                .map_err(|e| alloc::format!("dup root: {e}"))?;
            broker_hello(op, broker_root)?;

            // The agent is handed one channel and nothing else.
            let (broker_side, agent_side) =
                sys::channel_create().map_err(|e| alloc::format!("channel: {e}"))?;
            let agent = sys::spawn(ROOT, "bin/spaceagent", AGENT_QUOTA, Some(agent_side))
                .map_err(|e| alloc::format!("spawn agent: {e}"))?;
            let mut attach = ToolRequest::new(tool::ATTACH, "-");
            attach.abi_version = agent_abi::ABI_VERSION;
            sys::send(op, as_tool_bytes(&attach), Some(broker_side))
                .map_err(|e| alloc::format!("attach: {e}"))?;
            // The reply arrives only once the agent is finished with the broker.
            let summary = broker_reply(op)?;
            summary.result().map_err(|e| alloc::format!("attach: {e}"))?;

            let st = sys::wait(agent).map_err(|e| alloc::format!("wait agent: {e}"))?;
            sys::handle_close(agent).ok();
            expect_exit("spaceagent", st, 0)?;

            // The audit log is the operator's record of what the agent actually did.
            let (entries, allowed, denied) = read_audit(op)?;
            println!("[init] audit: {entries} entries, {allowed} allowed, {denied} refused");
            if summary.value != denied as u64 {
                return Err(alloc::format!(
                    "broker reported {} refusals, the audit log holds {denied}",
                    summary.value
                ));
            }
            if denied < 3 {
                return Err(alloc::format!("expected at least 3 refusals in the audit, found {denied}"));
            }
            if !audit_contains(op, entries, verdict::ALLOWED, "read /spaceos/ws/input.txt")? {
                return Err(String::from("the audit does not record the agent reading its input"));
            }
            if !audit_contains(op, entries, verdict::DENIED_SCOPE, "read /spaceos/manifest.txt")? {
                return Err(String::from("the audit does not record the out-of-scope read"));
            }
            if !audit_contains(op, entries, verdict::DENIED_SCOPE, "write /spaceos/ws/../stolen.txt")? {
                return Err(String::from("the audit does not record the attempted escape"));
            }
            if !audit_contains(op, entries, verdict::DENIED_TOOL, "audit -")? {
                return Err(String::from("the audit does not record the agent reaching for the audit log"));
            }
            if !audit_contains(op, entries, verdict::ALLOWED, "check verify")? {
                return Err(String::from("the audit does not record a passing check"));
            }

            broker_quit(op)?;
            let st = sys::wait(broker).map_err(|e| alloc::format!("wait broker: {e}"))?;
            sys::handle_close(broker).ok();
            sys::handle_close(op).ok();
            expect_exit("spacebroker", st, 0)
        },
    );

    r.run("G01", "every way out of the workspace is refused and recorded", || {
        const BROKER_QUOTA: u64 = 256;
        let (op, op_broker) = sys::channel_create().map_err(|e| alloc::format!("channel: {e}"))?;
        let broker = sys::spawn(ROOT, "bin/spacebroker", BROKER_QUOTA, Some(op_broker))
            .map_err(|e| alloc::format!("spawn broker: {e}"))?;
        let broker_root = sys::handle_dup(ROOT, rights::FS | rights::TRANSFER)
            .map_err(|e| alloc::format!("dup root: {e}"))?;
        broker_hello(op, broker_root)?;

        // Drive the agent side of the protocol directly, so every refusal can be
        // checked one at a time instead of inferred from an agent's exit code.
        let (broker_side, mine) = sys::channel_create().map_err(|e| alloc::format!("channel: {e}"))?;
        let mut attach = ToolRequest::new(tool::ATTACH, "-");
        attach.abi_version = agent_abi::ABI_VERSION;
        sys::send(op, as_tool_bytes(&attach), Some(broker_side))
            .map_err(|e| alloc::format!("attach: {e}"))?;

        let mut hello = ToolRequest::new(tool::HELLO, "-");
        hello.abi_version = agent_abi::ABI_VERSION;
        tool_call(mine, &hello)?.result().map_err(|e| alloc::format!("hello: {e}"))?;

        let escapes: &[(&str, u32)] = &[
            ("/spaceos/manifest.txt", tool::READ),
            ("/spaceos/ws/../stolen.txt", tool::WRITE),
            ("/spaceos/ws/sub/deep.txt", tool::READ),
            ("/spaceos/ws//double.txt", tool::READ),
            ("/spaceos/wsx/other.txt", tool::READ),
            ("spaceos/ws/relative.txt", tool::READ),
            ("/", tool::LIST),
            ("/spaceos", tool::LIST),
        ];
        for (path, t) in escapes {
            let mut req = ToolRequest::new(*t, path);
            req.len = 8;
            req.set_data(b"x");
            let reply = tool_call(mine, &req)?;
            if reply.verdict != verdict::DENIED_SCOPE {
                return Err(alloc::format!(
                    "path {path:?} gave verdict {} instead of denied-scope",
                    reply.verdict
                ));
            }
        }

        // Tools that belong to the operator are refused on the agent channel.
        for t in [tool::AUDIT, tool::ATTACH, tool::QUIT, 0xDEAD_BEEF] {
            let reply = tool_call(mine, &ToolRequest::new(t, "-"))?;
            if reply.verdict != verdict::DENIED_TOOL {
                return Err(alloc::format!("tool {t} gave verdict {} instead of denied-tool", reply.verdict));
            }
        }

        // In scope but impossible: a missing file and an unknown check are failures,
        // not escapes, and they are recorded as such.
        let mut missing = ToolRequest::new(tool::READ, "/spaceos/ws/nope.txt");
        missing.len = 8;
        let reply = tool_call(mine, &missing)?;
        if reply.verdict != verdict::FAILED {
            return Err(alloc::format!("a missing file gave verdict {}", reply.verdict));
        }
        let reply = tool_call(mine, &ToolRequest::new(tool::CHECK, "not-a-check"))?;
        if reply.verdict != verdict::FAILED {
            return Err(alloc::format!("an unknown check gave verdict {}", reply.verdict));
        }

        // A malformed message does not end the session.
        sys::send(mine, b"short", None).map_err(|e| alloc::format!("send short: {e}"))?;
        let reply = broker_reply(mine)?;
        expect_status("malformed tool request", reply.status, Error::MsgSize)?;
        let mut hello = ToolRequest::new(tool::HELLO, "-");
        hello.abi_version = agent_abi::ABI_VERSION;
        tool_call(mine, &hello)?.result().map_err(|e| alloc::format!("hello after garbage: {e}"))?;

        tool_call(mine, &ToolRequest::new(tool::DONE, "-"))?;
        let summary = broker_reply(op)?;
        summary.result().map_err(|e| alloc::format!("attach: {e}"))?;
        let (entries, allowed, denied) = read_audit(op)?;
        println!(
            "[init] audit after the escape attempts: {entries} entries, {allowed} allowed, {denied} refused"
        );
        if denied < escapes.len() as u32 + 4 {
            return Err(alloc::format!("only {denied} refusals recorded for {} attempts", escapes.len() + 4));
        }
        broker_quit(op)?;
        let st = sys::wait(broker).map_err(|e| alloc::format!("wait broker: {e}"))?;
        sys::handle_close(broker).ok();
        sys::handle_close(mine).ok();
        sys::handle_close(op).ok();
        expect_exit("spacebroker", st, 0)
    });

    r.run("G01", "the broker outlives an agent that disappears", || {
        const BROKER_QUOTA: u64 = 256;
        let (op, op_broker) = sys::channel_create().map_err(|e| alloc::format!("channel: {e}"))?;
        let broker = sys::spawn(ROOT, "bin/spacebroker", BROKER_QUOTA, Some(op_broker))
            .map_err(|e| alloc::format!("spawn broker: {e}"))?;
        let broker_root = sys::handle_dup(ROOT, rights::FS | rights::TRANSFER)
            .map_err(|e| alloc::format!("dup root: {e}"))?;
        broker_hello(op, broker_root)?;
        let (broker_side, mine) = sys::channel_create().map_err(|e| alloc::format!("channel: {e}"))?;
        let mut attach = ToolRequest::new(tool::ATTACH, "-");
        attach.abi_version = agent_abi::ABI_VERSION;
        sys::send(op, as_tool_bytes(&attach), Some(broker_side))
            .map_err(|e| alloc::format!("attach: {e}"))?;
        let mut hello = ToolRequest::new(tool::HELLO, "-");
        hello.abi_version = agent_abi::ABI_VERSION;
        tool_call(mine, &hello)?.result().map_err(|e| alloc::format!("hello: {e}"))?;
        // The agent vanishes without saying DONE.
        sys::handle_close(mine).ok();
        let summary = broker_reply(op)?;
        summary.result().map_err(|e| alloc::format!("attach: {e}"))?;
        // The broker is still there and still answers.
        let (entries, _, _) = read_audit(op)?;
        if entries == 0 {
            return Err(String::from("the audit log is empty after the agent vanished"));
        }
        broker_quit(op)?;
        let st = sys::wait(broker).map_err(|e| alloc::format!("wait broker: {e}"))?;
        sys::handle_close(broker).ok();
        sys::handle_close(op).ok();
        expect_exit("spacebroker", st, 0)
    });

    r.run_if(
        disk,
        "no disk on this machine",
        "L01",
        "corpus is indexed and queries return ranked chunks with verifiable provenance",
        || {
            let (link, svc) = link_start()?;
            let indexed = link_call(link, &LinkRequest::with_path(lreq::INDEX, CORPUS))?;
            let chunks = indexed.result().map_err(|e| alloc::format!("index: {e}"))?;
            if indexed.value != 4 {
                return Err(alloc::format!("indexed {} documents, expected 4", indexed.value));
            }
            if chunks < 4 {
                return Err(alloc::format!("indexed {chunks} chunks, expected at least one per document"));
            }

            let top = link_call(link, &LinkRequest::with_query(lreq::QUERY, "channel"))?;
            top.result().map_err(|e| alloc::format!("query: {e}"))?;
            if !top.path().ends_with("IPC.TXT") {
                return Err(alloc::format!("top hit for 'channel' is {}, expected IPC.TXT", top.path()));
            }
            if top.score < 2 {
                return Err(alloc::format!("top hit scored {}, expected at least 2 occurrences", top.score));
            }
            // Provenance is checked, not taken on trust: read the byte range the service
            // named and hash it here.
            verify_provenance(&top)?;
            if !top.text().starts_with(b"Space OS inter-process") {
                return Err(String::from("the returned text is not the start of the document"));
            }

            // Every result of a broader query must also be verifiable.
            let broad = link_call(link, &LinkRequest::with_query(lreq::QUERY, "space os"))?;
            let n = broad.result().map_err(|e| alloc::format!("query: {e}"))?;
            if n < 4 {
                return Err(alloc::format!("'space os' matched {n} chunks, expected at least 4"));
            }
            let mut last = u32::MAX;
            for i in 0..n {
                let mut q = LinkRequest::with_query(lreq::QUERY, "space os");
                q.offset = i;
                let hit = link_call(link, &q)?;
                hit.result().map_err(|e| alloc::format!("query {i}: {e}"))?;
                if hit.score > last {
                    return Err(String::from("results are not ordered by score"));
                }
                last = hit.score;
                verify_provenance(&hit)?;
            }
            let mut past = LinkRequest::with_query(lreq::QUERY, "space os");
            past.offset = n;
            let reply = link_call(link, &past)?;
            expect_status("query past the last result", reply.status, Error::NotFound)?;

            let stats = link_call(link, &LinkRequest::new(lreq::STATS))?;
            stats.result().map_err(|e| alloc::format!("stats: {e}"))?;
            if stats.value != 4 || stats.value2 != 0 {
                return Err(alloc::format!("stats says {} documents, {} revoked", stats.value, stats.value2));
            }
            link_stop(link, svc)
        },
    );

    r.run_if(
        disk,
        "no disk on this machine",
        "L02",
        "a revoked document leaves every result and does not return on re-index",
        || {
            let (link, svc) = link_start()?;
            link_call(link, &LinkRequest::with_path(lreq::INDEX, CORPUS))?
                .result()
                .map_err(|e| alloc::format!("index: {e}"))?;

            // Before revocation the document is findable by a word only it contains.
            let before = link_call(link, &LinkRequest::with_query(lreq::QUERY, "embargo"))?;
            before.result().map_err(|e| alloc::format!("query embargo: {e}"))?;
            if !before.path().ends_with("SECRET.TXT") {
                return Err(alloc::format!("'embargo' matched {} before revocation", before.path()));
            }

            let revoked = link_call(link, &LinkRequest::with_path(lreq::REVOKE, SECRET))?;
            revoked.result().map_err(|e| alloc::format!("revoke: {e}"))?;

            let after = link_call(link, &LinkRequest::with_query(lreq::QUERY, "embargo"))?;
            expect_status("query for a revoked document", after.status, Error::NotFound)?;
            if after.total != 0 {
                return Err(alloc::format!("'embargo' still matches {} chunks", after.total));
            }
            // A word the revoked document shares with the rest of the corpus must still
            // work, and must not surface the revoked document.
            assert_absent(link, "channel", SECRET)?;
            assert_absent(link, "quota", SECRET)?;

            // Not in any bundle either.
            let mut b = LinkRequest::with_query(lreq::BUNDLE, "channel quota");
            b.budget = 4096;
            let bundle = link_call(link, &b)?;
            let entries = bundle.result().map_err(|e| alloc::format!("bundle: {e}"))?;
            for i in 0..entries {
                let mut e = LinkRequest::new(lreq::BUNDLE_ENTRY);
                e.offset = i;
                let entry = link_call(link, &e)?;
                entry.result().map_err(|e| alloc::format!("bundle entry {i}: {e}"))?;
                if entry.path().eq_ignore_ascii_case(SECRET) {
                    return Err(String::from("a revoked document reached a context bundle"));
                }
            }

            // Re-indexing the corpus must not resurrect it.
            let reindexed = link_call(link, &LinkRequest::with_path(lreq::INDEX, CORPUS))?;
            reindexed.result().map_err(|e| alloc::format!("re-index: {e}"))?;
            let after = link_call(link, &LinkRequest::with_query(lreq::QUERY, "embargo"))?;
            expect_status("query after re-index", after.status, Error::NotFound)?;
            let stats = link_call(link, &LinkRequest::new(lreq::STATS))?;
            stats.result().map_err(|e| alloc::format!("stats: {e}"))?;
            if stats.value != 3 || stats.value2 != 1 {
                return Err(alloc::format!(
                    "after re-index stats says {} live documents and {} revoked, expected 3 and 1",
                    stats.value,
                    stats.value2
                ));
            }
            assert_absent(link, "channel", SECRET)?;
            link_stop(link, svc)
        },
    );

    r.run_if(
        disk,
        "no disk on this machine",
        "L03",
        "a context bundle fits its budget, is reproducible and carries provenance",
        || {
            let (link, svc) = link_start()?;
            link_call(link, &LinkRequest::with_path(lreq::INDEX, CORPUS))?
                .result()
                .map_err(|e| alloc::format!("index: {e}"))?;

            const BUDGET: u32 = 400;
            let mut b = LinkRequest::with_query(lreq::BUNDLE, "channel quota");
            b.budget = BUDGET;
            let bundle = link_call(link, &b)?;
            let entries = bundle.result().map_err(|e| alloc::format!("bundle: {e}"))?;
            if entries == 0 {
                return Err(String::from("the bundle is empty"));
            }
            if bundle.value > BUDGET {
                return Err(alloc::format!("the bundle is {} bytes, over the {BUDGET} budget", bundle.value));
            }

            // Walk the bundle: every entry verifiable, the byte count exact, and the
            // manifest digest reproducible from the entries alone.
            let mut total_bytes = 0u32;
            let mut chain = sha256::Sha256::new();
            for i in 0..entries {
                let mut e = LinkRequest::new(lreq::BUNDLE_ENTRY);
                e.offset = i;
                let entry = link_call(link, &e)?;
                entry.result().map_err(|e| alloc::format!("bundle entry {i}: {e}"))?;
                verify_provenance(&entry)?;
                total_bytes += entry.len;
                chain.update(&entry.digest);
            }
            if total_bytes != bundle.value {
                return Err(alloc::format!(
                    "entries add up to {total_bytes} bytes, the bundle claims {}",
                    bundle.value
                ));
            }
            if chain.finish() != bundle.digest {
                return Err(String::from("the bundle digest does not match the digests of its entries"));
            }
            println!("[init] bundle: {entries} chunk(s), {} of {BUDGET} bytes", bundle.value);

            // The same corpus and the same query give the same bundle.
            let again = link_call(link, &b)?;
            again.result().map_err(|e| alloc::format!("bundle again: {e}"))?;
            if again.digest != bundle.digest || again.value != bundle.value {
                return Err(String::from("the same query produced a different bundle"));
            }

            // A budget nothing fits into yields an empty bundle, not an error.
            let mut tiny = LinkRequest::with_query(lreq::BUNDLE, "channel quota");
            tiny.budget = 1;
            let empty = link_call(link, &tiny)?;
            let n = empty.result().map_err(|e| alloc::format!("tiny bundle: {e}"))?;
            if n != 0 || empty.value != 0 {
                return Err(alloc::format!("a 1-byte budget produced {n} entries"));
            }

            // Revocation changes the bundle, and the change is visible in its digest.
            link_call(link, &LinkRequest::with_path(lreq::REVOKE, SECRET))?
                .result()
                .map_err(|e| alloc::format!("revoke: {e}"))?;
            let after = link_call(link, &b)?;
            after.result().map_err(|e| alloc::format!("bundle after revoke: {e}"))?;
            if after.digest == bundle.digest {
                return Err(String::from("revoking a document left the bundle digest unchanged"));
            }
            link_stop(link, svc)
        },
    );

    r.run("P01", "HMAC-SHA256 matches the RFC 4231 test vectors", || {
        // The package MAC is only worth anything if the primitive under it is right,
        // and "the host and the guest agree" would not catch a shared mistake.
        let cases: &[(&[u8], &[u8], &str)] = &[
            (&[0x0b; 20], b"Hi There", "b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7"),
            (
                b"Jefe",
                b"what do ya want for nothing?",
                "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843",
            ),
            (&[0xaa; 20], &[0xdd; 50], "773ea91e36800e46854db8ebd09181a72959098b3ef8c122d9635514ced565fe"),
            (
                &[0xaa; 131],
                b"Test Using Larger Than Block-Size Key - Hash Key First",
                "60e431591ee0b67f0d8a26aacbf5b77f8e0bc6213728c5140546040f0ee37f54",
            ),
        ];
        for (i, (key, data, want)) in cases.iter().enumerate() {
            let got = hmac_sha256(key, data);
            let hex = sha256::to_hex(&got);
            let hex = core::str::from_utf8(&hex).unwrap_or("");
            if hex != *want {
                return Err(alloc::format!("RFC 4231 case {}: got {hex}, expected {want}", i + 1));
            }
        }
        // The comparison used on MACs must still be a comparison.
        let a = hmac_sha256(b"k", b"m");
        let mut b = a;
        b[31] ^= 1;
        if !hmac_verify(&a, &a) || hmac_verify(&a, &b) {
            return Err(String::from("the constant-time MAC comparison is wrong"));
        }
        Ok(())
    });

    r.run_if(
        disk,
        "no disk on this machine",
        "P01",
        "packages install only when they authenticate, and a refusal changes nothing",
        || {
            let (svc_ch, svc) = pkg_start()?;

            // Nothing installed yet.
            let st = pkg_call(svc_ch, &PkgRequest::new(preq::STATUS))?;
            st.result().map_err(|e| alloc::format!("status: {e}"))?;
            if st.version != 0 || st.history != 0 {
                return Err(String::from("the service starts with something installed"));
            }

            // Version 1, then version 2.
            let one = pkg_call(svc_ch, &PkgRequest::with_path(preq::INSTALL, "/spaceos/pkg/demo1.spk"))?;
            one.result().map_err(|e| alloc::format!("install v1: {e}"))?;
            if one.version != 1 || one.name() != "demo" || one.previous != 0 {
                return Err(alloc::format!(
                    "after v1: name {:?} version {} previous {}",
                    one.name(),
                    one.version,
                    one.previous
                ));
            }
            let payload_v1 = pkg_payload(svc_ch, one.len)?;
            if sha256::digest(&payload_v1) != one.digest {
                return Err(String::from(
                    "the installed payload does not match the digest the package carried",
                ));
            }

            let two = pkg_call(svc_ch, &PkgRequest::with_path(preq::INSTALL, "/spaceos/pkg/demo2.spk"))?;
            two.result().map_err(|e| alloc::format!("install v2: {e}"))?;
            if two.version != 2 || two.previous != 1 || two.history != 2 {
                return Err(alloc::format!(
                    "after v2: version {} previous {} history {}",
                    two.version,
                    two.previous,
                    two.history
                ));
            }

            // Three packages that must be refused, each for its own reason, and none of
            // them may disturb what is installed.
            let bad: &[(&str, i32, &str)] = &[
                ("/spaceos/pkg/badpay.spk", reject_code::PAYLOAD, "a flipped payload byte"),
                ("/spaceos/pkg/forged.spk", reject_code::MAC, "the wrong signing key"),
                ("/spaceos/pkg/trunc.spk", reject_code::TRUNCATED, "a missing payload"),
                ("/spaceos/manifest.txt", reject_code::FORMAT, "not a package at all"),
            ];
            for (path, want, why) in bad {
                let reply = pkg_call(svc_ch, &PkgRequest::with_path(preq::INSTALL, path))?;
                if reply.status == 0 {
                    return Err(alloc::format!("{path} ({why}) was installed"));
                }
                if reply.reject != *want {
                    return Err(alloc::format!(
                        "{path} ({why}) was refused with reject code {}, expected {want}",
                        reply.reject
                    ));
                }
                if reply.version != 2 || reply.history != 2 {
                    return Err(alloc::format!(
                        "{path} ({why}) changed the installed state to version {} ({} held)",
                        reply.version,
                        reply.history
                    ));
                }
            }

            // A missing file is an error, not a package refusal.
            let missing = pkg_call(svc_ch, &PkgRequest::with_path(preq::INSTALL, "/spaceos/pkg/nope.spk"))?;
            expect_status("installing a missing package", missing.status, Error::NotFound)?;
            if missing.reject != reject_code::NONE {
                return Err(String::from("a missing file was reported as a package refusal"));
            }

            // Verification without installation leaves the store alone.
            let checked = pkg_call(svc_ch, &PkgRequest::with_path(preq::VERIFY, "/spaceos/pkg/demo1.spk"))?;
            checked.result().map_err(|e| alloc::format!("verify: {e}"))?;
            if checked.version != 1 {
                return Err(alloc::format!("verify reported version {}", checked.version));
            }
            let st = pkg_call(svc_ch, &PkgRequest::new(preq::STATUS))?;
            if st.version != 2 {
                return Err(String::from("verify changed what is installed"));
            }
            pkg_stop(svc_ch, svc)
        },
    );

    r.run_if(
        disk,
        "no disk on this machine",
        "P01",
        "rollback returns the previous version, and only as far as the history goes",
        || {
            let (svc_ch, svc) = pkg_start()?;
            pkg_call(svc_ch, &PkgRequest::with_path(preq::INSTALL, "/spaceos/pkg/demo1.spk"))?
                .result()
                .map_err(|e| alloc::format!("install v1: {e}"))?;
            let v1 = pkg_call(svc_ch, &PkgRequest::new(preq::STATUS))?;
            let payload_v1 = pkg_payload(svc_ch, v1.len)?;

            pkg_call(svc_ch, &PkgRequest::with_path(preq::INSTALL, "/spaceos/pkg/demo2.spk"))?
                .result()
                .map_err(|e| alloc::format!("install v2: {e}"))?;
            let v2 = pkg_call(svc_ch, &PkgRequest::new(preq::STATUS))?;
            let payload_v2 = pkg_payload(svc_ch, v2.len)?;
            if payload_v1 == payload_v2 {
                return Err(String::from("the two versions carry the same payload"));
            }

            // Installing an older version is not how you go back.
            let down = pkg_call(svc_ch, &PkgRequest::with_path(preq::INSTALL, "/spaceos/pkg/demo1.spk"))?;
            expect_status("installing an older version", down.status, Error::Invalid)?;
            if down.version != 2 {
                return Err(String::from("a refused downgrade changed the active version"));
            }

            let back = pkg_call(svc_ch, &PkgRequest::new(preq::ROLLBACK))?;
            back.result().map_err(|e| alloc::format!("rollback: {e}"))?;
            if back.version != 1 || back.previous != 0 || back.history != 1 {
                return Err(alloc::format!(
                    "after rollback: version {} previous {} history {}",
                    back.version,
                    back.previous,
                    back.history
                ));
            }
            // The bytes that come back are the ones version 1 was installed with, not a
            // re-read of a file that could since have changed.
            let rolled = pkg_payload(svc_ch, back.len)?;
            if rolled != payload_v1 {
                return Err(String::from("rollback did not restore the earlier payload"));
            }
            if sha256::digest(&rolled) != back.digest {
                return Err(String::from("the rolled-back payload does not match its digest"));
            }

            // Nothing earlier is held, so there is nowhere left to go.
            let none = pkg_call(svc_ch, &PkgRequest::new(preq::ROLLBACK))?;
            expect_status("rollback with no earlier version", none.status, Error::NotFound)?;
            if none.version != 1 {
                return Err(String::from("a refused rollback changed the active version"));
            }

            // And after rolling back, the newer version installs again.
            let forward = pkg_call(svc_ch, &PkgRequest::with_path(preq::INSTALL, "/spaceos/pkg/demo2.spk"))?;
            forward.result().map_err(|e| alloc::format!("re-install v2: {e}"))?;
            if forward.version != 2 || forward.previous != 1 {
                return Err(alloc::format!(
                    "re-install left version {} previous {}",
                    forward.version,
                    forward.previous
                ));
            }
            pkg_stop(svc_ch, svc)
        },
    );

    let total = r.passed + r.failed.len() as u32;
    for skipped in &r.skipped {
        println!("[init] skipped: {skipped}");
    }
    if r.failed.is_empty() {
        println!("[init] ALL TESTS PASSED ({total}/{total}, {} skipped)", r.skipped.len());
        let _ = sys::shutdown(ROOT, 0);
    } else {
        println!("[init] TESTS FAILED: {} of {}", r.failed.len(), total);
        for f in &r.failed {
            println!("[init]   - {f}");
        }
        let _ = sys::shutdown(ROOT, 1);
    }
    0
}
