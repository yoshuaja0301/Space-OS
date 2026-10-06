//! The Task Service (T01, T02, G02; ADR-0036), driven as its operator and as the
//! user, agents and workers it serves.
//!
//! Every test starts its own `bin/spacetask` on the one journal the volume holds,
//! which is the point: what one service wrote, the next one reads. That journal is
//! shared with every earlier test, pass, boot and scenario on this volume, so no
//! test assumes an empty table: each knows its own tasks by id, and the ones that
//! claim work first cancel whatever an earlier run left queued.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use libspace::spaceabi::error::Error;
use libspace::spaceabi::handle::rights;
use libspace::spaceabi::syscall::sched_class;
use libspace::spaceabi::task::{
    BACKOFF_MS, JOURNAL_PATHS, MAX_TASKS, RECORD_BYTES, Record, TaskReply, actor, flags, record, state, text,
};
use libspace::task::{TaskService, Tasks};
use libspace::{Handle, println, sys};

use crate::ROOT;

/// Pages for the service: its heap (96 pages), code and stack.
const QUOTA: u64 = 256;
const WORKER_QUOTA: u64 = 128;
/// The title of the task each pass leaves running for the next boot (or pass).
const PROBE: &str = "reboot probe";
/// Stop: dispatch stops within this of the request (PRD §18), and a worker that
/// does not stop is ended this long after it was asked (U01).
const STOP_DISPATCH_MS: u64 = 1000;
const STOP_WORKER_MS: u64 = 2000;

fn start() -> Result<(TaskService, TaskReply), String> {
    let fs = sys::handle_dup(ROOT, rights::FS | rights::FS_WRITE | rights::TRANSFER)
        .map_err(|e| format!("dup root: {e}"))?;
    TaskService::start(ROOT, fs, QUOTA).map_err(|e| format!("start spacetask: {e}"))
}

fn session(svc: &TaskService, who: u32) -> Result<Tasks, String> {
    svc.session(who).map_err(|e| format!("{} session: {e}", actor::name(who)))
}

fn ok<T>(what: &str, r: Result<T, Error>) -> Result<T, String> {
    r.map_err(|e| format!("{what}: {e}"))
}

fn refused<T>(what: &str, r: Result<T, Error>, want: Error) -> Result<(), String> {
    match r {
        Err(e) if e == want => Ok(()),
        Err(e) => Err(format!("{what}: {e}, expected {want}")),
        Ok(_) => Err(format!("{what}: accepted, expected {want}")),
    }
}

fn expect_state(t: &Tasks, id: u32, want: u32, what: &str) -> Result<TaskReply, String> {
    let r = ok(&format!("{what}: get task {id}"), t.get(id))?;
    if r.state != want {
        return Err(format!(
            "{what}: task {id} is {} ({}), expected {}",
            state::name(r.state),
            r.reason(),
            state::name(want)
        ));
    }
    Ok(r)
}

/// Every task the service holds, oldest first.
fn all(t: &Tasks) -> Result<Vec<TaskReply>, String> {
    let mut out = Vec::new();
    loop {
        match t.list(out.len() as u32) {
            Ok(r) => out.push(r),
            Err(Error::NotFound) => return Ok(out),
            Err(e) => return Err(format!("list: {e}")),
        }
    }
}

/// What earlier runs left queued must not be what a claim here takes: cancel it,
/// and make sure dispatch runs.
fn clear_queue(user: &Tasks) -> Result<(), String> {
    if ok("stats", user.stats())?.has(flags::HALTED) {
        ok("resume", user.resume())?;
    }
    for t in all(user)?.iter().filter(|t| t.state == state::QUEUED) {
        ok("cancel a leftover", user.stop(t.id))?;
    }
    Ok(())
}

/// Claim, and insist the task claimed is `id`.
fn claim(worker: &Tasks, id: u32, attempt: u32) -> Result<TaskReply, String> {
    let c = ok("claim", worker.claim())?;
    if c.id != id || c.state != state::RUNNING || c.attempt != attempt {
        return Err(format!(
            "claim gave task {} ({}, attempt {}), expected task {id} on attempt {attempt}",
            c.id,
            state::name(c.state),
            c.attempt
        ));
    }
    Ok(c)
}

/// A `bin/taskworker` with a session of its own (as an agent).
struct Worker {
    process: Handle,
    chan: Handle,
}

impl Worker {
    fn start(svc: &TaskService, mode: &str) -> Result<Worker, String> {
        let s = session(svc, actor::AGENT)?.into_channel();
        let (mine, theirs) = match sys::channel_create() {
            Ok(p) => p,
            Err(e) => {
                sys::handle_close(s).ok();
                return Err(format!("channel: {e}"));
            }
        };
        let process = match sys::spawn_in(
            ROOT,
            "bin/taskworker",
            WORKER_QUOTA,
            Some(theirs),
            sched_class::BACKGROUND,
        ) {
            Ok(p) => p,
            Err(e) => {
                for h in [s, mine, theirs] {
                    sys::handle_close(h).ok();
                }
                return Err(format!("spawn taskworker: {e}"));
            }
        };
        let w = Worker { process, chan: mine };
        if let Err(e) = sys::send(mine, mode.as_bytes(), Some(s)) {
            sys::handle_close(s).ok();
            return Err(format!("send the mode: {e}"));
        }
        Ok(w)
    }

    /// The next thing the worker says, within `ms`.
    fn said(&self, ms: u64) -> Result<String, String> {
        match sys::wait_any(&[self.chan], ms) {
            Ok(_) => {}
            Err(Error::TimedOut) => return Err(format!("the worker said nothing for {ms} ms")),
            Err(e) => return Err(format!("wait for the worker: {e}")),
        }
        let mut buf = [0u8; 64];
        let (n, h) = sys::recv(self.chan, &mut buf, true).map_err(|e| format!("the worker: {e}"))?;
        if let Some(h) = h {
            sys::handle_close(h).ok();
        }
        Ok(String::from(core::str::from_utf8(&buf[..n]).unwrap_or("?")))
    }

    fn claimed(&self) -> Result<u32, String> {
        let s = self.said(3000)?;
        s.strip_prefix("claimed ")
            .and_then(|v| v.parse().ok())
            .ok_or_else(|| format!("the worker said '{s}'"))
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        sys::kill(self.process).ok();
        sys::wait(self.process).ok();
        sys::handle_close(self.process).ok();
        sys::handle_close(self.chan).ok();
    }
}

/// T01, first in every pass: the task the pass before (or the boot before) left
/// running with an effect open is still there, and the service that started since
/// did not decide how that effect ended.
pub fn reboot_check(pass: u32) -> Result<(), String> {
    let (svc, stats) = start()?;
    let user = session(&svc, actor::USER)?;
    let tasks = all(&user)?;
    let Some(p) = tasks.iter().rev().find(|t| t.title() == PROBE) else {
        println!(
            "[init] tasks: no task left on this volume by an earlier boot ({} task(s) held); this pass leaves one",
            stats.total
        );
        drop(user);
        svc.quit();
        return Ok(());
    };
    if p.state != state::NEEDS_RECONCILIATION || p.actor != actor::SERVICE || !p.has(flags::EFFECT_OPEN) {
        return Err(format!(
            "task {} '{PROBE}' is {} by {} ({}), expected needs_reconciliation by the service with its effect open",
            p.id,
            state::name(p.state),
            actor::name(p.actor),
            p.reason()
        ));
    }
    let key = ok("its effect", user.text(p.id, text::EFFECT))?;
    let saved = ok("its checkpoint", user.text(p.id, text::CHECKPOINT))?;
    let r = ok("reconcile", user.reconcile(p.id, false, "checked: the other side never saw it"))?;
    if r.state != state::FAILED {
        return Err(format!(
            "reconciled to {} ({}), expected failed: it had one attempt",
            state::name(r.state),
            r.reason()
        ));
    }
    println!(
        "[init] tasks: task {} from the previous {} was {} with effect '{key}' open (checkpoint '{saved}'): {}; the user said it never happened: {}",
        p.id,
        if pass == 1 { "boot" } else { "pass" },
        state::name(p.state),
        p.reason(),
        state::name(r.state)
    );
    drop(user);
    svc.quit();
    Ok(())
}

/// T01: closing the window that made a task does not end it.
pub fn outlives_session() -> Result<(), String> {
    let (svc, _) = start()?;
    let first = session(&svc, actor::USER)?;
    let t = ok("create", first.create("write the weekly report", "/spaceos/ws", 0, 0))?;
    ok("pause", first.move_to(t.id, state::PAUSED, "the window was closed to come back later"))?;
    drop(first);
    let second = session(&svc, actor::USER)?;
    let r = expect_state(&second, t.id, state::PAUSED, "in the next session")?;
    let workspace = ok("workspace", second.text(t.id, text::WORKSPACE))?;
    if r.title() != "write the weekly report"
        || r.owner != actor::USER
        || r.transitions != 1
        || workspace != "/spaceos/ws"
    {
        return Err(format!(
            "the next session sees '{}', owner {}, {} transition(s), workspace '{workspace}'",
            r.title(),
            actor::name(r.owner),
            r.transitions
        ));
    }
    ok("resume", second.move_to(t.id, state::QUEUED, "resumed from another window"))?;
    let end = ok("cancel", second.stop(t.id))?;
    println!(
        "[init] tasks: task {} '{}' outlived the session that made it: {} in the next one ({}), owner {}, workspace {workspace}; resumed and cancelled there",
        t.id,
        r.title(),
        state::name(r.state),
        r.reason(),
        actor::name(r.owner)
    );
    if end.state != state::CANCELLED {
        return Err(format!("stopping a queued task left it {}", state::name(end.state)));
    }
    drop(second);
    svc.quit();
    Ok(())
}

/// T01: every transition is written with its reason, time and actor, and a new
/// service gives the task back exactly as the old one left it.
pub fn history_and_restart() -> Result<(), String> {
    let (svc, _) = start()?;
    // A compaction folds a task's history into its last transition. One now leaves
    // room for everything this test writes, so none comes in the middle of it.
    ok("compact", svc.compact())?;
    let user = session(&svc, actor::USER)?;
    let agent = session(&svc, actor::AGENT)?;
    clear_queue(&user)?;
    let t = ok("create", user.create("tidy the downloads folder", "/spaceos/ws", 0, 0))?;
    claim(&agent, t.id, 1)?;
    ok("ask", agent.move_to(t.id, state::WAITING_APPROVAL, "about to delete 12 files"))?;
    // Approval is the user's: an agent does not wave its own work through.
    refused(
        "the agent approving itself",
        agent.move_to(t.id, state::RUNNING, "approved by me"),
        Error::Denied,
    )?;
    ok("approve", user.move_to(t.id, state::RUNNING, "approved: the 12 files are duplicates"))?;
    ok("finish", agent.move_to(t.id, state::SUCCEEDED, "12 duplicates deleted"))?;
    refused("a move out of a final state", user.move_to(t.id, state::QUEUED, "again"), Error::Invalid)?;
    drop(user);
    drop(agent);
    svc.quit();

    let (svc, _) = start()?;
    let user = session(&svc, actor::USER)?;
    let r = expect_state(&user, t.id, state::SUCCEEDED, "after a restart")?;
    if r.transitions != 4 || r.actor != actor::AGENT || r.owner != actor::USER || r.attempt != 1 {
        return Err(format!(
            "after a restart: {} transition(s), last by {}, owner {}, attempt {}",
            r.transitions,
            actor::name(r.actor),
            actor::name(r.owner),
            r.attempt
        ));
    }
    // The history, read back from the disk.
    let first = ok("history 0", user.history(t.id, 0))?;
    if first.value != u32::from(record::CREATE) || first.actor != actor::USER || first.reason() != r.title() {
        return Err(format!(
            "the first record is {} by {}",
            record::name(first.value as u8),
            actor::name(first.actor)
        ));
    }
    let want = [
        (state::RUNNING, actor::SERVICE),
        (state::WAITING_APPROVAL, actor::AGENT),
        (state::RUNNING, actor::USER),
        (state::SUCCEEDED, actor::AGENT),
    ];
    let mut moves = Vec::new();
    let mut last_ms = first.changed_ms;
    for n in 0..first.total {
        let h = ok("history", user.history(t.id, n))?;
        if h.changed_ms < last_ms || h.changed_ms == 0 {
            return Err(format!("record {n} has time {} after {last_ms}", h.changed_ms));
        }
        last_ms = h.changed_ms;
        if h.value == u32::from(record::MOVE) {
            if h.reason().is_empty() {
                return Err(format!("transition {} has no reason", moves.len()));
            }
            moves.push((h.state, h.actor, String::from(h.reason())));
        }
    }
    if moves.len() != want.len() || moves.iter().zip(want).any(|(m, w)| (m.0, m.1) != w) {
        let got: Vec<String> =
            moves.iter().map(|m| format!("{} by {}", state::name(m.0), actor::name(m.1))).collect();
        return Err(format!("the transitions read back are [{}]", got.join(", ")));
    }
    let shown: Vec<String> =
        moves.iter().map(|m| format!("{} by {} ({})", state::name(m.0), actor::name(m.1), m.2)).collect();
    println!(
        "[init] tasks: task {} after a restart: {}, {} records read back from the journal; transitions: {}",
        t.id,
        state::name(r.state),
        first.total,
        shown.join("; ")
    );
    drop(user);
    svc.quit();
    Ok(())
}

/// T01: a record cut short by a crash is skipped, not misread, and what is written
/// after it is kept.
pub fn torn_record() -> Result<(), String> {
    let (svc, _) = start()?;
    // No compaction may move the journal between here and the tear.
    let pinned = ok("compact", svc.compact())?;
    let user = session(&svc, actor::USER)?;
    let t = ok("create", user.create("torn record witness", "", 0, 0))?;
    drop(user);
    let journal = JOURNAL_PATHS[pinned.max_attempts as usize & 1];
    svc.quit();

    // What a power cut in the middle of a write leaves: the first 60 bytes of a
    // record that would have said the task succeeded.
    let f = sys::fs_open_write(ROOT, journal).map_err(|e| format!("open {journal}: {e}"))?;
    let torn = sys::fs_stat(f).map(|s| s.size).and_then(|at| {
        let mut r = Record::new(t.id, record::MOVE, 0).with_text("this move was never finished");
        r.state = state::SUCCEEDED as u8;
        r.seq = u32::MAX;
        sys::fs_write(f, at, &r.sealed()[..60]).map(|_| at)
    });
    sys::handle_close(f).ok();
    let at = torn.map_err(|e| format!("tear a record: {e}"))?;

    let (svc, after) = start()?;
    let user = session(&svc, actor::USER)?;
    if after.attempt == 0 {
        return Err(String::from("the service found no damaged record"));
    }
    expect_state(&user, t.id, state::QUEUED, "after the torn record")?;
    let later = ok("create after the tear", user.create("written after the tear", "", 0, 0))?;
    ok("cancel", user.stop(later.id))?;
    ok("cancel", user.stop(t.id))?;
    drop(user);
    svc.quit();

    let (svc, again) = start()?;
    let user = session(&svc, actor::USER)?;
    expect_state(&user, later.id, state::CANCELLED, "past the tear, after a restart")?;
    expect_state(&user, t.id, state::CANCELLED, "before the tear, after a restart")?;
    println!(
        "[init] tasks: 60 bytes of a record at byte {at} of {journal} were skipped ({} damaged record(s) seen); task {} stayed queued, and task {} written after the tear came back after a restart",
        again.attempt, t.id, later.id
    );
    drop(user);
    svc.quit();
    Ok(())
}

/// T01: the journal is compacted into its other file; the later epoch wins, and a
/// compaction cut short before its header is written changes nothing.
pub fn compaction() -> Result<(), String> {
    let (svc, _) = start()?;
    let user = session(&svc, actor::USER)?;
    let t = ok("create", user.create("compaction witness", "/spaceos/ws", 0, 0))?;
    ok("pause", user.move_to(t.id, state::PAUSED, "parked across a compaction"))?;
    // As the journal stands right before: nothing is written in between.
    let before = ok("stats", user.stats())?;
    let after = ok("compact", svc.compact())?;
    if after.max_attempts == before.max_attempts || after.effects_done <= before.effects_done {
        return Err(format!(
            "compaction stayed in file {} (epoch {} -> {})",
            after.max_attempts, before.effects_done, after.effects_done
        ));
    }
    let r = expect_state(&user, t.id, state::PAUSED, "after compaction")?;
    if r.transitions != 1 || r.reason() != "parked across a compaction" {
        return Err(format!("after compaction: {} transition(s), '{}'", r.transitions, r.reason()));
    }
    drop(user);
    svc.quit();

    // Both files have a header now; the later epoch is the journal.
    let (svc, restarted) = start()?;
    if restarted.max_attempts != after.max_attempts {
        return Err(String::from("after a restart the service went back to the file it had compacted"));
    }
    svc.quit();

    // A compaction cut short: the other file holds records, but its header slot is
    // still empty.
    let current = after.max_attempts as usize & 1;
    let other = JOURNAL_PATHS[1 - current];
    let f = sys::fs_create(ROOT, other).map_err(|e| format!("create {other}: {e}"))?;
    let mut lie = Record::new(t.id, record::SNAPSHOT, 1).with_text("a compaction that never finished");
    lie.state = state::SUCCEEDED as u8;
    lie.seq = 1;
    let mut bytes = alloc::vec![0u8; RECORD_BYTES];
    bytes.extend_from_slice(&lie.sealed());
    let written = sys::fs_write(f, 0, &bytes);
    sys::handle_close(f).ok();
    written.map_err(|e| format!("write {other}: {e}"))?;

    let (svc, last) = start()?;
    let user = session(&svc, actor::USER)?;
    if last.max_attempts as usize != current {
        return Err(String::from("the service took up a journal file without a header"));
    }
    expect_state(&user, t.id, state::PAUSED, "after a compaction cut short")?;
    ok("cancel", user.stop(t.id))?;
    println!(
        "[init] tasks: journal compacted from {} ({} bytes) into {} (epoch {}, {} bytes) with task {} still paused; a compaction cut short before its header was ignored",
        JOURNAL_PATHS[before.max_attempts as usize & 1],
        before.changed_ms,
        JOURNAL_PATHS[current],
        after.effects_done,
        after.changed_ms,
        t.id
    );
    drop(user);
    svc.quit();
    Ok(())
}

/// T01: unfinished tasks fill the table only so far; a finished one makes room.
pub fn retention() -> Result<(), String> {
    let (svc, _) = start()?;
    let user = session(&svc, actor::USER)?;
    // This test counts unfinished tasks, so what earlier runs left is ended first.
    for t in all(&user)?.iter().filter(|t| !state::is_final(t.state)) {
        ok("end a leftover", user.stop(t.id))?;
    }
    let mut mine = Vec::new();
    let refusal = loop {
        match user.create("retention filler", "", 0, 1) {
            Ok(t) => mine.push(t.id),
            Err(e) => break e,
        }
        if mine.len() > MAX_TASKS {
            return Err(format!("{} unfinished tasks were accepted", mine.len()));
        }
    };
    if refusal != Error::Quota || mine.len() != MAX_TASKS {
        return Err(format!(
            "{} unfinished tasks, then {refusal}; expected {MAX_TASKS}, then quota",
            mine.len()
        ));
    }
    // One finishes: the next task takes the place of the oldest finished one.
    ok("cancel the first", user.stop(mine[0]))?;
    let newcomer = ok("create after one finished", user.create("retention newcomer", "", 0, 1))?;
    refused("the finished task that made room", user.get(mine[0]), Error::NotFound)?;
    let held = ok("stats", user.stats())?.total;
    for id in mine[1..].iter().copied().chain([newcomer.id]) {
        ok("cancel", user.stop(id))?;
    }
    println!(
        "[init] tasks: {MAX_TASKS} unfinished tasks filled the table and the next was refused ({refusal}); when one finished, task {} took its place ({held} held)",
        newcomer.id
    );
    drop(user);
    svc.quit();
    Ok(())
}

/// T02: an effect that was open when the service went away is a question, not a
/// failure to retry: nothing dispatches the task until someone says what happened.
pub fn uncertain_effect() -> Result<(), String> {
    const KEY: &str = "invoice-2026-10-mail";
    let (svc, _) = start()?;
    let user = session(&svc, actor::USER)?;
    let agent = session(&svc, actor::AGENT)?;
    clear_queue(&user)?;
    let t = ok("create", user.create("send the invoice to the customer", "/spaceos/ws", 0, 3))?;
    claim(&agent, t.id, 1)?;
    ok("begin the effect", agent.effect_begin(t.id, KEY))?;
    drop(user);
    drop(agent);
    // The service dies with the mail on its way.
    svc.crash();

    let (svc, stats) = start()?;
    let user = session(&svc, actor::USER)?;
    let agent = session(&svc, actor::AGENT)?;
    let r = expect_state(&user, t.id, state::NEEDS_RECONCILIATION, "after the restart")?;
    if r.actor != actor::SERVICE || !r.has(flags::EFFECT_OPEN) || stats.transitions == 0 {
        return Err(format!(
            "after the restart: by {}, effect open {}",
            actor::name(r.actor),
            r.has(flags::EFFECT_OPEN)
        ));
    }
    // Not retried blind: not dispatched, not queued again, not declared done.
    refused("a claim", agent.claim(), Error::NotFound)?;
    refused("queueing it again", user.move_to(t.id, state::QUEUED, "retry"), Error::Invalid)?;
    refused("declaring it done", user.move_to(t.id, state::SUCCEEDED, "surely"), Error::Invalid)?;
    // The user asks the other side, by the effect's key: it never arrived.
    let q = ok("reconcile", user.reconcile(t.id, false, "the mail server has no message with that key"))?;
    if q.state != state::QUEUED || q.effects_done != 0 {
        return Err(format!("reconciled to {} with {} effect(s)", state::name(q.state), q.effects_done));
    }
    claim(&agent, t.id, 2)?;
    ok("begin again", agent.effect_begin(t.id, KEY))?;
    ok("confirm", agent.effect_end(t.id, KEY, true))?;
    ok("finish", agent.move_to(t.id, state::SUCCEEDED, "invoice sent, once"))?;
    let done = expect_state(&user, t.id, state::SUCCEEDED, "at the end")?;
    if done.effects_done != 1 || done.attempt != 2 {
        return Err(format!("at the end: {} effect(s), attempt {}", done.effects_done, done.attempt));
    }
    println!(
        "[init] tasks: task {} was running with effect '{KEY}' open when its service was killed; after the restart it was {} ({}), and nothing dispatched or retried it until the user said it never happened; attempt 2 sent it once",
        t.id,
        state::name(r.state),
        r.reason()
    );
    drop(user);
    drop(agent);
    svc.quit();
    Ok(())
}

/// T02: a failed attempt is retried after a backoff that doubles, and not past the
/// last attempt.
pub fn retry_backoff() -> Result<(), String> {
    let (svc, _) = start()?;
    let user = session(&svc, actor::USER)?;
    let agent = session(&svc, actor::AGENT)?;
    clear_queue(&user)?;
    let t = ok("create", user.create("fetch the exchange rates", "", 0, 3))?;
    let mut waits = Vec::new();
    let mut failed_at = 0u64;
    for attempt in 1..=3u32 {
        let mut early = 0u32;
        let until = sys::ticks_ms() + 3000;
        loop {
            match agent.claim() {
                Ok(c) if c.id == t.id && c.attempt == attempt => break,
                Ok(c) => return Err(format!("claim gave task {} attempt {}", c.id, c.attempt)),
                Err(Error::NotFound) if sys::ticks_ms() < until => {
                    early += 1;
                    sys::sleep_ms(5);
                }
                Err(e) => return Err(format!("claim for attempt {attempt}: {e}")),
            }
        }
        if attempt > 1 {
            let waited = sys::ticks_ms() - failed_at;
            let backoff = BACKOFF_MS << (attempt - 2);
            if waited < backoff || early == 0 {
                return Err(format!(
                    "attempt {attempt} was dispatched {waited} ms after the failure ({early} claim(s) refused first); its backoff is {backoff} ms"
                ));
            }
            waits.push((waited, backoff));
        }
        // The backoff starts while the service handles the failure: no earlier than this.
        failed_at = sys::ticks_ms();
        ok("fail", agent.move_to(t.id, state::FAILED, "the rate server answered 503"))?;
    }
    let r = expect_state(&user, t.id, state::FAILED, "after three failures")?;
    if r.attempt != 3 || !r.reason().contains("none left") {
        return Err(format!("after three failures: attempt {}, '{}'", r.attempt, r.reason()));
    }
    refused("a claim after the last attempt", agent.claim(), Error::NotFound)?;
    let shown: Vec<String> = waits.iter().map(|(w, b)| format!("{w} ms (backoff {b})")).collect();
    println!(
        "[init] tasks: task {} failed three times: attempts 2 and 3 came {}; then it failed for good: {}",
        t.id,
        shown.join(" and "),
        r.reason()
    );
    drop(user);
    drop(agent);
    svc.quit();
    Ok(())
}

/// T02: a worker that gives up with an effect open does not make the task fail or
/// retry: it needs reconciliation.
pub fn failed_with_effect_open() -> Result<(), String> {
    const KEY: &str = "upload-photos-batch-7";
    let (svc, _) = start()?;
    let user = session(&svc, actor::USER)?;
    let agent = session(&svc, actor::AGENT)?;
    clear_queue(&user)?;
    let t = ok("create", user.create("upload the holiday photos", "/spaceos/ws", 0, 3))?;
    claim(&agent, t.id, 1)?;
    ok("begin the effect", agent.effect_begin(t.id, KEY))?;
    refused(
        "succeeding with an effect unconfirmed",
        agent.move_to(t.id, state::SUCCEEDED, "done"),
        Error::Busy,
    )?;
    refused("a second effect at once", agent.effect_begin(t.id, "another"), Error::Busy)?;
    let r = ok("fail", agent.move_to(t.id, state::FAILED, "the connection dropped mid-upload"))?;
    if r.state != state::NEEDS_RECONCILIATION || r.actor != actor::AGENT {
        return Err(format!(
            "failing with an effect open gave {} by {}",
            state::name(r.state),
            actor::name(r.actor)
        ));
    }
    refused("a claim", agent.claim(), Error::NotFound)?;
    // The user looks: the photos are all there. The effect counts, and the task
    // waits to be resumed past it.
    let p = ok("reconcile", user.reconcile(t.id, true, "the album shows all 7 photos"))?;
    if p.state != state::PAUSED || p.effects_done != 1 || p.has(flags::EFFECT_OPEN) {
        return Err(format!("reconciled to {} with {} effect(s)", state::name(p.state), p.effects_done));
    }
    ok("resume", user.move_to(t.id, state::QUEUED, "resume after the upload"))?;
    claim(&agent, t.id, 2)?;
    ok("finish", agent.move_to(t.id, state::SUCCEEDED, "captions written"))?;
    println!(
        "[init] tasks: task {} failed with effect '{KEY}' open: {} ({}); the user confirmed it, and attempt 2 finished without uploading again",
        t.id,
        state::name(r.state),
        r.reason()
    );
    drop(user);
    drop(agent);
    svc.quit();
    Ok(())
}

/// G02: Stop stops dispatch within a second, asks every running task to stop, and
/// holds the queued ones; the workers stop their tasks; what already happened is
/// counted, not undone.
pub fn stop_all() -> Result<(), String> {
    let (svc, _) = start()?;
    let user = session(&svc, actor::USER)?;
    clear_queue(&user)?;
    let mut ids = Vec::new();
    for i in 1..=4 {
        ids.push(ok("create", user.create(&format!("batch job {i}"), "/spaceos/ws", 0, 0))?.id);
    }
    let w1 = Worker::start(&svc, "work")?;
    let a = w1.claimed()?;
    let w2 = Worker::start(&svc, "work")?;
    let b = w2.claimed()?;
    // Dispatch stops at two running tasks.
    let third = session(&svc, actor::AGENT)?;
    refused("a third claim while two run", third.claim(), Error::WouldBlock)?;
    let until = sys::ticks_ms() + 3000;
    while [a, b].iter().any(|&id| user.get(id).map(|t| t.effects_done == 0).unwrap_or(true)) {
        if sys::ticks_ms() > until {
            return Err(String::from("the workers caused no effect in 3 s"));
        }
        sys::sleep_ms(10);
    }

    let t0 = sys::ticks_ms();
    let r = ok("stop everything", user.stop_all())?;
    let dispatch_ms = sys::ticks_ms() - t0;
    refused("a claim after Stop", third.claim(), Error::Denied)?;
    if dispatch_ms > STOP_DISPATCH_MS || r.total != 2 || r.value != 2 || r.effects_done < 2 {
        return Err(format!(
            "Stop took {dispatch_ms} ms; {} running asked, {} queued held, {} effects done",
            r.total, r.value, r.effects_done
        ));
    }
    for (w, id) in [(&w1, a), (&w2, b)] {
        let said = w.said(STOP_WORKER_MS)?;
        if !said.starts_with(&format!("stopped {id} ")) {
            return Err(format!("worker of task {id} said '{said}'"));
        }
    }
    let workers_ms = sys::ticks_ms() - t0;
    if workers_ms > STOP_WORKER_MS {
        return Err(format!("the workers took {workers_ms} ms to stop"));
    }
    let mut effects = 0;
    for id in [a, b] {
        let t = expect_state(&user, id, state::CANCELLED, "after Stop")?;
        if t.actor != actor::AGENT || t.has(flags::EFFECT_OPEN) {
            return Err(format!(
                "task {id}: cancelled by {}, effect open {}",
                actor::name(t.actor),
                t.has(flags::EFFECT_OPEN)
            ));
        }
        effects += t.effects_done;
    }
    // The queued ones are held, not cancelled -- and not shown as cancelled.
    for &id in ids.iter().filter(|&&id| id != a && id != b) {
        expect_state(&user, id, state::QUEUED, "a queued task after Stop")?;
    }
    ok("resume", user.resume())?;
    let c = ok("a claim after resume", third.claim())?;
    ok("stop it", user.stop(c.id))?;
    ok("cancel it", third.move_to(c.id, state::CANCELLED, "stopped by hand"))?;
    clear_queue(&user)?;
    println!(
        "[init] tasks: Stop stopped dispatch in {dispatch_ms} ms (the next claim: denied); 2 running tasks were asked, 2 queued ones held, not cancelled; both workers stopped their tasks {workers_ms} ms after Stop, with {effects} effect(s) already done and kept"
    );
    drop(w1);
    drop(w2);
    drop(third);
    drop(user);
    svc.quit();
    Ok(())
}

/// G02: a worker that does not stop is ended from outside after 2 s, and its task
/// is not shown as cancelled while its effect may have happened.
pub fn hung_worker() -> Result<(), String> {
    let (svc, _) = start()?;
    let user = session(&svc, actor::USER)?;
    clear_queue(&user)?;
    let t = ok("create", user.create("charge the customer's card", "", 0, 3))?;
    let w = Worker::start(&svc, "hang")?;
    let id = w.claimed()?;
    if id != t.id {
        return Err(format!("the worker claimed task {id}, expected {}", t.id));
    }
    let t0 = sys::ticks_ms();
    let r = ok("stop", user.stop(id))?;
    if r.state != state::RUNNING
        || !r.has(flags::STOP_REQUESTED)
        || !r.has(flags::EFFECT_OPEN)
        || r.value != 0
    {
        return Err(format!(
            "after Stop: {}, stop asked {}, effect open {}, {} effect(s) done",
            state::name(r.state),
            r.has(flags::STOP_REQUESTED),
            r.has(flags::EFFECT_OPEN),
            r.value
        ));
    }
    // The worker never looks. Two seconds, then it is ended from outside.
    while sys::ticks_ms() - t0 < STOP_WORKER_MS {
        expect_state(&user, id, state::RUNNING, "while its worker hangs")?;
        sys::sleep_ms(50);
    }
    drop(w);
    let killed_ms = sys::ticks_ms() - t0;
    let after = ok(
        "cancel",
        user.move_to(id, state::CANCELLED, "its worker did not stop within 2 s and was killed"),
    )?;
    if after.state != state::NEEDS_RECONCILIATION || !after.has(flags::EFFECT_OPEN) {
        return Err(format!("after the kill: {}", state::name(after.state)));
    }
    let key = ok("its effect", user.text(id, text::EFFECT))?;
    // The user gives up on knowing: cancelled, with the effect still unconfirmed.
    let end = ok("stop", user.stop(id))?;
    if end.state != state::CANCELLED || !end.has(flags::EFFECT_OPEN) {
        return Err(format!("given up: {}", state::name(end.state)));
    }
    println!(
        "[init] tasks: a worker that did not stop was killed {killed_ms} ms after Stop; its task became {}, not cancelled: effect '{key}' may have happened; given up, it is cancelled with that effect still unconfirmed",
        state::name(after.state)
    );
    drop(user);
    svc.quit();
    Ok(())
}

/// T01, last in every pass: a task left running with an effect open, its service
/// killed the way a power cut would, for the next boot (or pass) to find.
pub fn leave_probe(pass: u32) -> Result<(), String> {
    let (svc, _) = start()?;
    let user = session(&svc, actor::USER)?;
    let agent = session(&svc, actor::AGENT)?;
    clear_queue(&user)?;
    let t = ok("create", user.create(PROBE, "", 0, 1))?;
    claim(&agent, t.id, 1)?;
    let key = format!("probe-pass-{pass}");
    ok("checkpoint", agent.checkpoint(t.id, "half way"))?;
    ok("begin the effect", agent.effect_begin(t.id, &key))?;
    drop(user);
    drop(agent);
    svc.crash();
    println!("[init] tasks: task {} left running with effect '{key}' open for the next boot to find", t.id);
    Ok(())
}
