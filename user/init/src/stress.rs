//! The stability run (PRD §9 "Stabilitas MVP", ADR-0019).
//!
//! `stress=N` on the kernel command line runs the acceptance suite N times in one
//! boot; `stress=Nm` keeps starting passes for N minutes. After every pass comes a
//! round of chaos: workloads of every kind started together and killed at moments
//! nobody chose, the network service among them in the middle of a transfer.
//!
//! Then every process but this one is gone, and the machine must be exactly where
//! the first pass left it: the kernel's free frames and heap, the live processes
//! and threads, this process's own heap and its handles. Anything else is a leak,
//! reported with its size. Each pass ends in one line with those numbers, which
//! the harness turns into the run's memory record.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use libspace::net::TcpStream;
use libspace::spaceabi::error::Error;
use libspace::spaceabi::handle::{MAX_HANDLES, rights};
use libspace::{Handle, heap, kill_reason, println, sys};

use crate::{Hardware, ROOT, nettest, stats, suite};

/// How long the run goes on.
#[derive(Clone, Copy)]
pub enum Limit {
    Passes(u32),
    Minutes(u64),
}

/// Stop after this many failed passes: past that, more passes only repeat the
/// story and bury the first failure under the rest.
const FAILED_PASSES_MAX: u32 = 3;
/// Pages for a churn process: code, stack, heap, and the most `mem` maps at once.
const CHURN_QUOTA: u64 = 128;
/// How long a pass's end waits for stragglers -- a child of a killed process still
/// on its way out -- before a difference counts as a leak.
const SETTLE_MS: u64 = 2000;
/// The chaos round lets its workloads run for a random time up to this long...
const RUN_MS_MAX: u64 = 1500;
/// ...and waits a random time up to this long between two kills.
const GAP_MS_MAX: u64 = 40;

/// `stress=` from the kernel command line, if it has one.
pub fn mode() -> Result<Option<Limit>, String> {
    let mut buf = [0u8; 512];
    let n = sys::cmdline(ROOT, &mut buf).map_err(|e| format!("cannot read the kernel command line: {e}"))?;
    if n > buf.len() {
        return Err(format!("a {n}-byte kernel command line; at most {} are read", buf.len()));
    }
    let line =
        core::str::from_utf8(&buf[..n]).map_err(|_| String::from("the kernel command line is not UTF-8"))?;
    let Some(v) = line.split_whitespace().find_map(|t| t.strip_prefix("stress=")) else {
        return Ok(None);
    };
    let limit = match v.strip_suffix('m') {
        Some(m) => m.parse().ok().filter(|&m| m > 0).map(Limit::Minutes),
        None => v.parse().ok().filter(|&n| n > 0).map(Limit::Passes),
    };
    limit.map(Some).ok_or_else(|| {
        format!("stress={v}: expected a number of passes (stress=3) or of minutes (stress=480m)")
    })
}

/// xorshift64*: the chaos round's choices. Seeded from the entropy source, and the
/// seed is printed, so the order of a failing round can be read back.
struct Rng {
    state: u64,
}

impl Rng {
    fn new() -> (Rng, u64) {
        let mut b = [0u8; 8];
        let seed = match sys::random(&mut b) {
            Ok(8) => u64::from_le_bytes(b),
            _ => sys::ticks_ms().wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ sys::clock_realtime_ms().unwrap_or(0),
        };
        (Rng { state: seed | 1 }, seed)
    }

    fn next(&mut self) -> u64 {
        let mut x = self.state;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.state = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n.max(1)
    }
}

/// What must come back after every pass.
#[derive(Clone, Copy, PartialEq, Eq)]
struct Snapshot {
    frames_free: u64,
    heap_used: u64,
    processes: u64,
    threads: u64,
    init_heap: usize,
    handles: usize,
}

impl Snapshot {
    fn take() -> Result<Snapshot, String> {
        let s = stats()?;
        Ok(Snapshot {
            frames_free: s.frames_free,
            heap_used: s.heap_used,
            processes: s.processes_live,
            threads: s.threads_live,
            init_heap: heap::stats().1,
            handles: (0..MAX_HANDLES as Handle).filter(|&h| sys::handle_info(h).is_ok()).count(),
        })
    }

    /// Take a snapshot, giving the machine up to [`SETTLE_MS`] to come back to
    /// `base`: a process killed a moment ago may leave a child that is still
    /// exiting.
    fn settled(base: &Snapshot) -> Result<Snapshot, String> {
        let deadline = sys::ticks_ms() + SETTLE_MS;
        loop {
            let now = Snapshot::take()?;
            if now == *base || sys::ticks_ms() >= deadline {
                return Ok(now);
            }
            sys::sleep_ms(50);
        }
    }

    /// Every figure that differs from `base`, with the difference.
    fn diff(&self, base: &Snapshot) -> String {
        let d = |name: &str, now: i128, was: i128| {
            if now == was { None } else { Some(format!("{name} {:+}", now - was)) }
        };
        [
            d("free frames", self.frames_free as i128, base.frames_free as i128),
            d("kernel heap bytes", self.heap_used as i128, base.heap_used as i128),
            d("processes", self.processes as i128, base.processes as i128),
            d("threads", self.threads as i128, base.threads as i128),
            d("init heap bytes", self.init_heap as i128, base.init_heap as i128),
            d("init handles", self.handles as i128, base.handles as i128),
        ]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join(", ")
    }
}

/// The run itself. Never returns while the machine is up: it ends in a shutdown
/// whose code says how it went.
pub fn run(hw: Hardware, limit: Limit) -> i32 {
    let start = sys::ticks_ms();
    let (mut rng, seed) = Rng::new();
    match limit {
        Limit::Passes(n) => {
            println!("[stress] stability run: {n} passes of the acceptance suite; seed {seed:#018x}")
        }
        Limit::Minutes(m) => {
            println!(
                "[stress] stability run: passes of the acceptance suite for {m} minutes; seed {seed:#018x}"
            )
        }
    }
    let mut base: Option<Snapshot> = None;
    let mut pass = 0u32;
    let mut failed_passes = 0u32;
    let mut leaks = 0u32;
    let mut tests = 0u64;
    let mut killed = 0u64;
    loop {
        let more = match limit {
            Limit::Passes(n) => pass < n,
            Limit::Minutes(m) => sys::ticks_ms() - start < m * 60_000,
        };
        if !more || failed_passes >= FAILED_PASSES_MAX {
            break;
        }
        pass += 1;
        let t0 = sys::ticks_ms();
        println!("[stress] pass {pass} starting, {} s into the run", (t0 - start) / 1000);
        let r = suite(hw, pass);
        let (passed, failed, skipped) = (r.passed, r.failed.len(), r.skipped.len());
        for f in &r.failed {
            println!("[stress] pass {pass}: FAIL {f}");
        }
        drop(r);
        tests += u64::from(passed);
        // Said, then dropped: the measurement below includes this process's own heap,
        // so nothing of this pass may still be held when it is taken.
        let chaos_ok = match chaos(&mut rng, hw) {
            Ok((n, text)) => {
                killed += n as u64;
                println!("[stress] pass {pass} chaos: {text}");
                true
            }
            Err(e) => {
                println!("[stress] pass {pass} chaos FAILED: {e}");
                false
            }
        };
        let snap = match &base {
            Some(b) => Snapshot::settled(b),
            None => Snapshot::take(),
        };
        let k = stats();
        let (snap, k) = match (snap, k) {
            (Ok(s), Ok(k)) => (s, k),
            (Err(e), _) | (_, Err(e)) => {
                println!("[stress] pass {pass}: cannot measure: {e}");
                failed_passes += 1;
                continue;
            }
        };
        let leak = base.as_ref().map(|b| snap.diff(b)).filter(|d| !d.is_empty());
        let mut verdict = Vec::new();
        if failed > 0 {
            verdict.push(format!("{failed} test(s) failed"));
        }
        if !chaos_ok {
            verdict.push(String::from("the chaos round failed"));
        }
        if let Some(d) = &leak {
            verdict.push(format!("LEAK against pass 1: {d}"));
            leaks += 1;
        }
        let outcome = if verdict.is_empty() { String::from("ok") } else { verdict.join("; ") };
        println!(
            "[stress] pass {pass}: {outcome}; {passed}/{} tests{} in {:.1} s; \
             frames_free={} frames_free_min={} heap_used={} heap_used_peak={} processes={} threads={} \
             init_heap={} handles={} uptime_ms={}",
            passed as usize + failed,
            if skipped > 0 { format!(" ({skipped} skipped)") } else { String::new() },
            (sys::ticks_ms() - t0) as f64 / 1000.0,
            snap.frames_free,
            k.frames_free_min,
            snap.heap_used,
            k.heap_used_peak,
            snap.processes,
            snap.threads,
            snap.init_heap,
            snap.handles,
            k.uptime_ms
        );
        if !verdict.is_empty() {
            failed_passes += 1;
        }
        // The first pass sets the mark every later one is held to; after a leak the
        // mark moves, so each leak is counted once rather than in every pass after.
        if base.is_none() {
            println!(
                "[stress] baseline after pass 1: {} free frames, {} kernel heap bytes, {} process(es), \
                 {} thread(s), {} init heap bytes, {} init handles",
                snap.frames_free, snap.heap_used, snap.processes, snap.threads, snap.init_heap, snap.handles
            );
        }
        if base.is_none() || leak.is_some() {
            base = Some(snap);
        }
    }
    let minutes = (sys::ticks_ms() - start) as f64 / 60_000.0;
    let k = stats().ok();
    println!(
        "[stress] done: {pass} passes in {minutes:.1} min, {tests} tests passed, {killed} processes killed at random; \
         {failed_passes} failed pass(es), {leaks} leak(s); lowest free frames {}, peak kernel heap {} bytes",
        k.map_or(0, |k| k.frames_free_min),
        k.map_or(0, |k| k.heap_used_peak)
    );
    let ok = failed_passes == 0 && pass > 0;
    if ok {
        println!("[stress] STRESS PASSED");
    } else {
        println!("[stress] STRESS FAILED");
    }
    let _ = sys::shutdown(ROOT, if ok { 0 } else { 1 });
    0
}

/// A process the chaos round started and will kill.
struct Victim {
    what: String,
    process: Handle,
    control: Handle,
}

impl Victim {
    fn kill(&self) {
        sys::kill(self.process).ok();
    }

    /// Reap it; it must have died of the kill and of nothing else.
    fn reap(self) -> Result<(), String> {
        let st = sys::wait(self.process);
        sys::handle_close(self.process).ok();
        sys::handle_close(self.control).ok();
        match st {
            Ok(st) if st.is_killed_by(kill_reason::SIGNAL) => Ok(()),
            other => Err(format!("{} ended with {other:?} before it was killed", self.what)),
        }
    }
}

/// Start `bin/churn` in `mode` (moving `carry` to it) and wait until it is working.
fn churn(mode: &str, carry: Option<Handle>) -> Result<Victim, String> {
    let (mine, theirs) = sys::channel_create().map_err(|e| format!("channel: {e}"))?;
    let process = match sys::spawn(ROOT, "bin/churn", CHURN_QUOTA, Some(theirs)) {
        Ok(p) => p,
        Err(e) => {
            sys::handle_close(mine).ok();
            if let Some(h) = carry {
                sys::handle_close(h).ok();
            }
            return Err(format!("spawn churn {mode}: {e}"));
        }
    };
    let v = Victim { what: String::from(mode), process, control: mine };
    let started = sys::send(mine, mode.as_bytes(), carry).map_err(|e| format!("send: {e}")).and_then(|()| {
        let mut buf = [0u8; 8];
        sys::wait_any(&[mine], 5000).map_err(|e| format!("no word: {e}"))?;
        match sys::recv(mine, &mut buf, true) {
            Ok((2, _)) if &buf[..2] == b"go" => Ok(()),
            other => Err(format!("it said {other:?}")),
        }
    });
    match started {
        Ok(()) => Ok(v),
        Err(e) => {
            v.kill();
            v.reap().ok();
            Err(format!("churn {mode}: {e}"))
        }
    }
}

/// An echo connection kept busy until the service under it dies.
struct Transfer {
    stream: TcpStream,
    sent: u64,
    echoed: u64,
    alive: bool,
}

impl Transfer {
    /// Send a little and take back what has come, without waiting long for either.
    fn pump(&mut self) -> Result<(), String> {
        if !self.alive {
            return Ok(());
        }
        let mut chunk = [0u8; 1024];
        for (i, b) in chunk.iter_mut().enumerate() {
            *b = (self.sent as usize + i) as u8;
        }
        match self.stream.write_all(&chunk, 20) {
            Ok(()) => self.sent += chunk.len() as u64,
            Err(Error::TimedOut) => {}
            Err(_) => {
                self.alive = false;
                return Ok(());
            }
        }
        let mut buf = [0u8; 1024];
        loop {
            match self.stream.try_read(&mut buf) {
                Ok(Some(0)) | Ok(None) => return Ok(()),
                Ok(Some(n)) => {
                    let at = self.echoed as usize;
                    if buf[..n].iter().enumerate().any(|(i, &b)| b != (at + i) as u8) {
                        return Err(format!("the echo came back changed after {} bytes", self.echoed));
                    }
                    self.echoed += n as u64;
                }
                Err(_) => {
                    self.alive = false;
                    return Ok(());
                }
            }
        }
    }

    /// Wait `ms`, keeping the transfer going meanwhile.
    fn busy_for(&mut self, ms: u64) -> Result<(), String> {
        let until = sys::ticks_ms() + ms;
        while sys::ticks_ms() < until {
            self.pump()?;
            sys::sleep_ms(2);
        }
        Ok(())
    }
}

/// Start every kind of workload at once, let them run for a random time, and kill
/// them in a random order at random intervals. Returns how many were killed and
/// what happened.
fn chaos(rng: &mut Rng, hw: Hardware) -> Result<(usize, String), String> {
    let mut victims: Vec<Victim> = Vec::new();
    let started = start_victims(hw, &mut victims);
    // The network service, with a transfer running through it.
    let mut net: Option<(nettest::Lab, Transfer)> = None;
    let started = started.and_then(|()| {
        if !hw.nic {
            return Ok(());
        }
        let lab = nettest::start(ROOT)?;
        match lab.session.connect("echo.lab.test", 7, 3000) {
            Ok(stream) => {
                net = Some((lab, Transfer { stream, sent: 0, echoed: 0, alive: true }));
                Ok(())
            }
            Err(e) => {
                sys::kill(lab.process).ok();
                sys::wait(lab.process).ok();
                lab.release();
                Err(format!("connect echo: {e}"))
            }
        }
    });
    if let Err(e) = started {
        for v in &victims {
            v.kill();
        }
        for v in victims {
            v.reap().ok();
        }
        if let Some((lab, t)) = net {
            drop(t);
            sys::kill(lab.process).ok();
            sys::wait(lab.process).ok();
            lab.release();
        }
        return Err(e);
    }

    /// Let `ms` pass, keeping the transfer going if there is one.
    fn busy(net: &mut Option<(nettest::Lab, Transfer)>, ms: u64) -> Result<(), String> {
        match net {
            Some((_, t)) => t.busy_for(ms),
            None => {
                sys::sleep_ms(ms);
                Ok(())
            }
        }
    }
    let run_ms = 100 + rng.below(RUN_MS_MAX);
    let t0 = sys::ticks_ms();
    let mut problems = Vec::new();
    if let Err(e) = busy(&mut net, run_ms) {
        problems.push(e);
    }
    // Kill in a random order: index `victims.len()` stands for the network service.
    let slots = victims.len() + usize::from(net.is_some());
    let mut order: Vec<usize> = (0..slots).collect();
    for i in (1..order.len()).rev() {
        order.swap(i, rng.below(i as u64 + 1) as usize);
    }
    let mut log = Vec::new();
    let mut net_killed_at = None;
    for (n, &i) in order.iter().enumerate() {
        if n > 0 {
            let gap = rng.below(GAP_MS_MAX + 1);
            if let Err(e) = busy(&mut net, gap) {
                problems.push(e);
            }
        }
        let at = sys::ticks_ms() - t0;
        if i < victims.len() {
            victims[i].kill();
            log.push(format!("{} at {at} ms", victims[i].what));
        } else if let Some((lab, t)) = &net {
            sys::kill(lab.process).ok();
            net_killed_at = Some((at, t.sent, t.echoed));
            log.push(format!("spacenet at {at} ms"));
        }
    }
    let count = slots;
    for v in victims {
        if let Err(e) = v.reap() {
            problems.push(e);
        }
    }
    let mut net_text = String::new();
    if let Some((lab, mut t)) = net {
        match sys::wait(lab.process) {
            Ok(st) if st.is_killed_by(kill_reason::SIGNAL) => {}
            other => problems.push(format!("spacenet ended with {other:?} before it was killed")),
        }
        // Whatever was already queued may still be read; then the connection must
        // report that its service is gone, not hang and not make data up.
        let mut buf = [0u8; 1024];
        let mut ended = false;
        for _ in 0..1000 {
            match t.stream.read(&mut buf, 1000) {
                Ok(0) | Err(_) => {
                    ended = true;
                    break;
                }
                Ok(_) => {}
            }
        }
        if !ended {
            problems.push(String::from("a connection of the killed service kept delivering data"));
        }
        drop(t.stream);
        lab.release();
        if let Some((_, sent, echoed)) = net_killed_at {
            net_text = format!(" (mid-transfer: {sent} bytes sent, {echoed} echoed)");
        }
        if let Err(e) = nettest::recover(ROOT) {
            problems.push(format!("the network did not come back: {e}"));
        } else {
            net_text.push_str("; network back");
        }
    }
    if problems.is_empty() {
        Ok((count, format!("{count} killed after {run_ms} ms: {}{net_text}", log.join(", "))))
    } else {
        Err(problems.join("; "))
    }
}

/// The churn workloads this machine can run; each is pushed as it starts.
fn start_victims(hw: Hardware, victims: &mut Vec<Victim>) -> Result<(), String> {
    let dup = |r: u32| sys::handle_dup(ROOT, r | rights::TRANSFER).map_err(|e| format!("dup root: {e}"));
    if hw.disk {
        for n in ["disk 1", "disk 2"] {
            victims.push(churn(n, Some(dup(rights::FS | rights::FS_WRITE)?))?);
        }
    }
    victims.push(churn("mem", None)?);
    victims.push(churn("ipc", None)?);
    victims.push(churn("spawn", Some(dup(rights::SPAWN)?))?);
    Ok(())
}
