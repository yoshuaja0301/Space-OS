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

use libspace::sha256;
use libspace::spaceabi::error::{Error, decode};
use libspace::spaceabi::handle::rights;
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
