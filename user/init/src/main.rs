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

use libspace::spaceabi::error::Error;
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
    let (mine, theirs) = sys::channel_create().map_err(|e| alloc::format!("channel: {e}"))?;
    let p = sys::spawn(ROOT, "bin/fault", CHILD_QUOTA, Some(theirs))
        .map_err(|e| alloc::format!("spawn fault: {e}"))?;
    sys::send(mine, mode.as_bytes(), None).map_err(|e| alloc::format!("send mode: {e}"))?;
    let st = sys::wait(p).map_err(|e| alloc::format!("wait: {e}"))?;
    sys::handle_close(p).ok();
    sys::handle_close(mine).ok();
    expect_killed(mode, st, reason)
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
