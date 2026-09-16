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
use libspace::spaceabi::compute::{
    self as compute_abi, BufferRef as ComputeBufferRef, Op as ComputeOp, Request as ComputeRequest,
    Response as ComputeResponse, op as cop, req as creq,
};
use libspace::spaceabi::error::{Error, decode};
use libspace::spaceabi::handle::{MAX_HANDLES, rights};
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
}

impl Runner {
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
    let mut r = Runner { passed: 0, failed: Vec::new() };

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

    r.run("D01", "model file read from the guest disk matches the manifest checksum", || {
        let mut manifest = alloc::string::String::new();
        stream_file("/spaceos/manifest.txt", |chunk| {
            manifest.push_str(core::str::from_utf8(chunk).unwrap_or(""));
        })?;
        let want_size: u64 = manifest_value(&manifest, "size")
            .and_then(|v| v.parse().ok())
            .ok_or_else(|| String::from("manifest has no size"))?;
        let want_hash =
            manifest_value(&manifest, "sha256").ok_or_else(|| String::from("manifest has no sha256"))?;
        let path = manifest_value(&manifest, "path").ok_or_else(|| String::from("manifest has no path"))?;

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
    });

    r.run("D01", "file API rejects bad paths, missing rights and bad buffers", || {
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
    });

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

    r.run("A01", "native inference: a small model generates 128 tokens matching the pinned baseline", || {
        let (svc_client, svc_server) = sys::channel_create().map_err(|e| alloc::format!("channel: {e}"))?;
        let compute = sys::spawn(ROOT, "bin/spacecompute", COMPUTE_QUOTA, Some(svc_server))
            .map_err(|e| alloc::format!("spawn compute: {e}"))?;
        let (ai_mine, ai_theirs) = sys::channel_create().map_err(|e| alloc::format!("channel: {e}"))?;
        let ai = sys::spawn(ROOT, "bin/spaceai", AI_QUOTA, Some(ai_theirs))
            .map_err(|e| alloc::format!("spawn ai: {e}"))?;

        // The runtime gets exactly two capabilities: a compute connection and
        // read-only file system access. It cannot spawn, kill or inspect anything.
        sys::send(ai_mine, b"compute", Some(svc_client)).map_err(|e| alloc::format!("pass compute: {e}"))?;
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
    });

    let total = r.passed + r.failed.len() as u32;
    if r.failed.is_empty() {
        println!("[init] ALL TESTS PASSED ({total}/{total})");
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
