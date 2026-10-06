//! `spacetask` -- the Task Service (PRD v0.2 §13: T01, T02, G02; ADR-0036).
//!
//! Tasks live here, not in whatever window started them: a client (a terminal, the
//! desktop's Task Center, an agent's worker) opens a session, and the task outlives
//! the session, the client, this process and the machine's next boot. Every change
//! is written to the journal before it is answered, so an answer is a promise the
//! disk keeps.
//!
//! What the service does not do is guess. When it starts again it replays the
//! journal, and a task that was running when it went away is not running any more:
//! it is paused (nothing was half-done that matters), cancelled (a stop had been
//! asked for), or -- when it had told the world something and never heard back --
//! `needs_reconciliation`, where it stays until someone says whether that effect
//! happened. Nothing is retried blind.
#![no_std]
#![no_main]

extern crate alloc;

use alloc::string::String;
use alloc::vec::Vec;

use libspace::spaceabi::error::Error;
use libspace::spaceabi::syscall::{WAIT_FOREVER, WAIT_MAX};
use libspace::spaceabi::task::{
    ABI_VERSION, BACKOFF_MS, COMPACT_RECORDS, DEFAULT_ATTEMPTS, JOURNAL_PATHS, MAX_ATTEMPTS, MAX_RUNNING,
    MAX_TASKS, RECORD_BYTES, Record, TaskReply, TaskRequest, actor, flags, record, req, state, text,
};
use libspace::{Handle, handle, println, sys};

/// Heap for the task table, the journal replay buffer and a compaction.
const HEAP_PAGES: usize = 96;
/// Sessions served at once, beside the operator's channel.
const SESSIONS_MAX: usize = WAIT_MAX - 1;
/// Bytes read from the journal at a time.
const READ_CHUNK: usize = 16 * 1024;
const RECORD: u64 = RECORD_BYTES as u64;

struct Task {
    id: u32,
    title: String,
    owner: u32,
    parent: u32,
    workspace: String,
    created_ms: u64,
    state: u32,
    attempt: u32,
    max_attempts: u32,
    effects_done: u32,
    /// The key of the external effect that began and has not been settled.
    open_effect: Option<String>,
    checkpoint: String,
    transitions: u32,
    stop_requested: bool,
    actor: u32,
    reason: String,
    changed_ms: u64,
    /// Not dispatched before this uptime: the backoff after a failed attempt.
    not_before_ms: u64,
}

impl Task {
    /// The records that rebuild this task as it stands, for a compacted journal.
    fn snapshot(&self, out: &mut Vec<Record>) {
        let mut c = Record::new(self.id, record::CREATE, self.created_ms).with_text(&self.title);
        c.actor = self.owner as u8;
        c.aux = self.parent;
        c.max_attempts = self.max_attempts as u16;
        out.push(c);
        if !self.workspace.is_empty() {
            out.push(Record::new(self.id, record::WORKSPACE, self.created_ms).with_text(&self.workspace));
        }
        let mut s = Record::new(self.id, record::SNAPSHOT, self.changed_ms).with_text(&self.reason);
        s.state = self.state as u8;
        s.actor = self.actor as u8;
        s.attempt = self.attempt as u16;
        s.max_attempts = self.max_attempts as u16;
        s.aux = (self.transitions.min(0xffff) << 16) | self.effects_done.min(0xffff);
        out.push(s);
        if let Some(key) = &self.open_effect {
            out.push(Record::new(self.id, record::EFFECT_BEGIN, self.changed_ms).with_text(key));
        }
        if !self.checkpoint.is_empty() {
            out.push(Record::new(self.id, record::CHECKPOINT, self.changed_ms).with_text(&self.checkpoint));
        }
        if self.stop_requested {
            out.push(Record::new(self.id, record::STOP, self.changed_ms));
        }
    }
}

/// The journal file in use: where the next record goes, and its number.
struct Journal {
    file: Handle,
    /// Index into [`JOURNAL_PATHS`].
    which: usize,
    epoch: u32,
    offset: u64,
    seq: u32,
    /// Records in this file, and how many it may hold before the next compaction.
    records: u32,
    compact_at: u32,
}

/// What reading the journal back found.
#[derive(Default)]
struct Replay {
    records: u32,
    /// Records that were not records: cut short, damaged, or out of order.
    damaged: u32,
    /// Tasks moved because the service had gone away while they ran.
    settled: u32,
}

struct Session {
    ch: Handle,
    actor: u32,
}

struct Service {
    root: Option<Handle>,
    journal: Option<Journal>,
    tasks: Vec<Task>,
    next_id: u32,
    halted: bool,
    replay: Replay,
    sessions: Vec<Session>,
}

fn now_ms() -> u64 {
    // Wall-clock time for the record; uptime where the clock cannot be read.
    sys::clock_realtime_ms().unwrap_or_else(|_| sys::ticks_ms())
}

/// Write all of `bytes` at `offset`. The file system writes all of it or reports an
/// error; anything else is not a write to build on.
fn write_all(file: Handle, offset: u64, bytes: &[u8]) -> Result<(), Error> {
    match sys::fs_write(file, offset, bytes)? {
        n if n == bytes.len() => Ok(()),
        _ => Err(Error::Invalid),
    }
}

fn header(epoch: u32, next_id: u32) -> Record {
    let mut h = Record::new(next_id, record::HEADER, now_ms()).with_text("Space OS task journal");
    h.aux = epoch;
    h
}

/// The epoch of the journal file `f`, if its first record is a header.
fn epoch_of(f: Handle, size: u64) -> Option<(u32, u32)> {
    if size < RECORD {
        return None;
    }
    let mut b = [0u8; RECORD_BYTES];
    match sys::fs_read(f, 0, &mut b) {
        Ok(RECORD_BYTES) => {}
        _ => return None,
    }
    Record::open(&b).filter(|r| r.kind == record::HEADER && r.seq == 0).map(|r| (r.aux, r.task))
}

impl Service {
    fn new() -> Service {
        Service {
            root: None,
            journal: None,
            tasks: Vec::new(),
            next_id: 1,
            halted: false,
            replay: Replay::default(),
            sessions: Vec::new(),
        }
    }

    fn task(&self, id: u32) -> Result<&Task, Error> {
        self.tasks.iter().find(|t| t.id == id).ok_or(Error::NotFound)
    }

    fn task_mut(&mut self, id: u32) -> Result<&mut Task, Error> {
        self.tasks.iter_mut().find(|t| t.id == id).ok_or(Error::NotFound)
    }

    fn reply_for(&self, id: u32) -> Result<TaskReply, Error> {
        let t = self.task(id)?;
        let mut f = 0;
        if t.stop_requested {
            f |= flags::STOP_REQUESTED;
        }
        if t.open_effect.is_some() {
            f |= flags::EFFECT_OPEN;
        }
        if self.halted {
            f |= flags::HALTED;
        }
        let mut r = TaskReply {
            id: t.id,
            state: t.state,
            attempt: t.attempt,
            max_attempts: t.max_attempts,
            effects_done: t.effects_done,
            transitions: t.transitions,
            flags: f,
            actor: t.actor,
            owner: t.owner,
            parent: t.parent,
            total: self.tasks.len() as u32,
            changed_ms: t.changed_ms,
            ..Default::default()
        };
        r.set_title(&t.title);
        r.set_reason(&t.reason);
        Ok(r)
    }

    // ---- the journal ------------------------------------------------------------

    /// Find the journal with the file capability the operator handed over -- of the
    /// two files, the one with a header, and of two with one the later epoch -- read
    /// it back, and keep it open for appending. A volume without one gets a new one.
    fn open_journal(&mut self, root: Handle) -> Result<(), Error> {
        let mut opened: [Option<(Handle, u64)>; 2] = [None, None];
        let close_all = |opened: &[Option<(Handle, u64)>; 2]| {
            for (f, _) in opened.iter().flatten() {
                sys::handle_close(*f).ok();
            }
        };
        for (i, path) in JOURNAL_PATHS.iter().enumerate() {
            match sys::fs_open_write(root, path) {
                Ok(f) => match sys::fs_stat(f) {
                    Ok(st) => opened[i] = Some((f, st.size)),
                    Err(e) => {
                        sys::handle_close(f).ok();
                        close_all(&opened);
                        return Err(e);
                    }
                },
                Err(Error::NotFound) => {}
                Err(e) => {
                    close_all(&opened);
                    return Err(e);
                }
            }
        }
        let headers = [
            opened[0].and_then(|(f, size)| epoch_of(f, size)),
            opened[1].and_then(|(f, size)| epoch_of(f, size)),
        ];
        let chosen = match (headers[0], headers[1]) {
            (Some((a, _)), Some((b, _))) => Some(usize::from(b > a)),
            (Some(_), None) => Some(0),
            (None, Some(_)) => Some(1),
            (None, None) => None,
        };
        let Some(i) = chosen else {
            // Nothing a header vouches for. A file that holds bytes anyway is left as
            // it was if the other one is free; the new journal never writes over
            // more than it must.
            let i = match opened {
                [Some((_, a)), Some((_, b))] if a > 0 && b == 0 => 1,
                [Some((_, a)), None] if a > 0 => 1,
                _ => 0,
            };
            close_all(&opened);
            if let Some((_, size)) = opened[i].filter(|(_, size)| *size > 0) {
                println!(
                    "[task] {} holds {size} bytes but no journal header; starting a new journal in it",
                    JOURNAL_PATHS[i]
                );
            }
            let file = sys::fs_create(root, JOURNAL_PATHS[i])?;
            if let Err(e) = write_all(file, 0, &header(1, self.next_id).sealed()) {
                sys::handle_close(file).ok();
                return Err(e);
            }
            self.journal = Some(Journal {
                file,
                which: i,
                epoch: 1,
                offset: RECORD,
                seq: 1,
                records: 0,
                compact_at: COMPACT_RECORDS,
            });
            return Ok(());
        };
        if let Some((f, _)) = opened[1 - i] {
            sys::handle_close(f).ok();
        }
        let (file, size) = opened[i].expect("the chosen file was opened");
        let (epoch, next_id) = headers[i].expect("the chosen file has a header");
        self.next_id = self.next_id.max(next_id);
        let last_seq = match self.replay_from(file, size) {
            Ok(s) => s,
            Err(e) => {
                sys::handle_close(file).ok();
                return Err(e);
            }
        };
        // A record cut short at the end is left where it is; the next one starts at
        // the next whole record, and replay skips the remains.
        let offset = size.div_ceil(RECORD).max(1) * RECORD;
        let records = ((offset / RECORD) - 1) as u32;
        self.journal = Some(Journal {
            file,
            which: i,
            epoch,
            offset,
            seq: last_seq + 1,
            records,
            compact_at: COMPACT_RECORDS.max(records + 1),
        });
        self.settle_after_restart()?;
        if records >= COMPACT_RECORDS
            && let Err(e) = self.compact()
        {
            println!("[task] compaction failed: {e}; the journal stays where it is");
        }
        Ok(())
    }

    /// Apply every record in `file` after its header, in order; return the last
    /// sequence number.
    fn replay_from(&mut self, file: Handle, size: u64) -> Result<u32, Error> {
        let mut last_seq = 0u32;
        let mut buf = alloc::vec![0u8; READ_CHUNK];
        let mut offset = RECORD;
        while offset + RECORD <= size {
            let want = (((size - offset) as usize).min(READ_CHUNK) / RECORD_BYTES) * RECORD_BYTES;
            let n = sys::fs_read(file, offset, &mut buf[..want])?;
            let whole = n - n % RECORD_BYTES;
            if whole == 0 {
                break;
            }
            for chunk in buf[..whole].chunks_exact(RECORD_BYTES) {
                match Record::open(chunk) {
                    Some(r) if r.seq > last_seq && r.kind != record::HEADER => {
                        last_seq = r.seq;
                        self.replay.records += 1;
                        self.apply(&r);
                    }
                    _ => self.replay.damaged += 1,
                }
            }
            offset += whole as u64;
        }
        // A tail shorter than a record is what a crash left of one.
        if !size.is_multiple_of(RECORD) {
            self.replay.damaged += 1;
        }
        Ok(last_seq)
    }

    /// One record's effect on the table, as it was when it was written.
    fn apply(&mut self, r: &Record) {
        let text = String::from(r.text());
        match r.kind {
            record::CREATE => {
                self.next_id = self.next_id.max(r.task.saturating_add(1));
                if self.tasks.iter().any(|t| t.id == r.task) {
                    return;
                }
                self.tasks.push(Task {
                    id: r.task,
                    title: text,
                    owner: u32::from(r.actor),
                    parent: r.aux,
                    workspace: String::new(),
                    created_ms: r.time_ms,
                    state: state::QUEUED,
                    attempt: 0,
                    max_attempts: u32::from(r.max_attempts),
                    effects_done: 0,
                    open_effect: None,
                    checkpoint: String::new(),
                    transitions: 0,
                    stop_requested: false,
                    actor: u32::from(r.actor),
                    reason: String::from("created"),
                    changed_ms: r.time_ms,
                    not_before_ms: 0,
                });
            }
            record::HALT => self.halted = r.state == 1,
            record::FORGET => self.tasks.retain(|t| t.id != r.task),
            _ => {
                let Ok(t) = self.task_mut(r.task) else { return };
                match r.kind {
                    record::MOVE => {
                        t.state = u32::from(r.state);
                        t.attempt = u32::from(r.attempt);
                        t.actor = u32::from(r.actor);
                        t.reason = text;
                        t.changed_ms = r.time_ms;
                        t.transitions += 1;
                        // A stop is answered once the task is no longer running.
                        if t.state != state::RUNNING {
                            t.stop_requested = false;
                        }
                    }
                    record::EFFECT_BEGIN => t.open_effect = Some(text),
                    record::EFFECT_END => {
                        t.open_effect = None;
                        if r.state == 1 {
                            t.effects_done += 1;
                        }
                    }
                    record::STOP => t.stop_requested = true,
                    record::WORKSPACE => t.workspace = text,
                    record::CHECKPOINT => t.checkpoint = text,
                    record::SNAPSHOT => {
                        t.state = u32::from(r.state);
                        t.actor = u32::from(r.actor);
                        t.attempt = u32::from(r.attempt);
                        t.max_attempts = u32::from(r.max_attempts);
                        t.reason = text;
                        t.changed_ms = r.time_ms;
                        t.transitions = r.aux >> 16;
                        t.effects_done = r.aux & 0xffff;
                    }
                    _ => {}
                }
            }
        }
    }

    /// Write `recs` to the journal in one write, then apply them. Nothing changes
    /// when the write fails: a full or failing disk is an error, never a success
    /// (PRD §10.1).
    fn commit(&mut self, recs: &[Record]) -> Result<(), Error> {
        let j = self.journal.as_mut().ok_or(Error::Denied)?;
        let mut bytes = Vec::new();
        bytes.try_reserve_exact(recs.len() * RECORD_BYTES).map_err(|_| Error::NoMemory)?;
        let mut sealed = Vec::new();
        sealed.try_reserve_exact(recs.len()).map_err(|_| Error::NoMemory)?;
        for (i, r) in recs.iter().enumerate() {
            let mut r = *r;
            r.seq = j.seq + i as u32;
            bytes.extend_from_slice(&r.sealed());
            sealed.push(r);
        }
        write_all(j.file, j.offset, &bytes)?;
        j.offset += bytes.len() as u64;
        j.seq += recs.len() as u32;
        j.records += recs.len() as u32;
        let compact = j.records >= j.compact_at;
        for r in &sealed {
            self.apply(r);
        }
        if compact && let Err(e) = self.compact() {
            // The journal in use is still whole; try again after as many records more.
            println!("[task] compaction failed: {e}; the journal stays where it is");
            if let Some(j) = self.journal.as_mut() {
                j.compact_at = j.records + COMPACT_RECORDS;
            }
        }
        Ok(())
    }

    /// Write what the table holds into the other journal file -- its records first,
    /// behind a header slot that is not a header yet, then the header with the next
    /// epoch -- and append there from now on. Until the header is down, the journal
    /// in use is the old one, whole; after, the new one is.
    fn compact(&mut self) -> Result<(), Error> {
        let root = self.root.ok_or(Error::Denied)?;
        let (which, epoch, before) = {
            let j = self.journal.as_ref().ok_or(Error::Denied)?;
            (j.which, j.epoch, j.records)
        };
        let other = 1 - which;
        let mut recs = Vec::new();
        for t in &self.tasks {
            t.snapshot(&mut recs);
        }
        if self.halted {
            let mut h = Record::new(0, record::HALT, now_ms());
            h.state = 1;
            h.actor = actor::SERVICE as u8;
            recs.push(h);
        }
        let mut bytes = Vec::new();
        bytes.try_reserve_exact((recs.len() + 1) * RECORD_BYTES).map_err(|_| Error::NoMemory)?;
        bytes.resize(RECORD_BYTES, 0);
        for (i, r) in recs.iter_mut().enumerate() {
            r.seq = 1 + i as u32;
            bytes.extend_from_slice(&r.sealed());
        }
        let file = sys::fs_create(root, JOURNAL_PATHS[other])?;
        let written = write_all(file, 0, &bytes)
            .and_then(|()| write_all(file, 0, &header(epoch + 1, self.next_id).sealed()));
        if let Err(e) = written {
            sys::handle_close(file).ok();
            return Err(e);
        }
        let n = recs.len() as u32;
        let old = self.journal.replace(Journal {
            file,
            which: other,
            epoch: epoch + 1,
            offset: bytes.len() as u64,
            seq: n + 1,
            records: n,
            compact_at: n + COMPACT_RECORDS,
        });
        if let Some(old) = old {
            sys::handle_close(old.file).ok();
        }
        println!(
            "[task] journal compacted into {} (epoch {}): {before} record(s) became {n}",
            JOURNAL_PATHS[other],
            epoch + 1
        );
        Ok(())
    }

    fn mv(&self, id: u32, to: u32, by: u32, attempt: u32, why: &str) -> Record {
        let mut r = Record::new(id, record::MOVE, now_ms()).with_text(why);
        r.state = to as u8;
        r.actor = by as u8;
        r.attempt = attempt as u16;
        r
    }

    /// The service went away while some tasks ran. None of them is running now, and
    /// none is assumed to have finished.
    fn settle_after_restart(&mut self) -> Result<(), Error> {
        let mut recs = Vec::new();
        for t in self.tasks.iter().filter(|t| t.state == state::RUNNING) {
            let (to, why) = match (&t.open_effect, t.stop_requested) {
                (Some(key), _) => (
                    state::NEEDS_RECONCILIATION,
                    alloc::format!("service restarted; effect '{key}' may or may not have happened"),
                ),
                (None, true) => {
                    (state::CANCELLED, String::from("stopped: service restarted before the worker said so"))
                }
                (None, false) => {
                    (state::PAUSED, String::from("interrupted: service restarted; resume to go on"))
                }
            };
            println!("[task] task {} '{}' was running: now {}", t.id, t.title, state::name(to));
            recs.push(self.mv(t.id, to, actor::SERVICE, t.attempt, &why));
        }
        if !recs.is_empty() {
            self.commit(&recs)?;
            self.replay.settled += recs.len() as u32;
        }
        Ok(())
    }

    // ---- requests ---------------------------------------------------------------

    fn handle(&mut self, r: &TaskRequest, by: u32) -> Result<TaskReply, Error> {
        if r.abi_version != ABI_VERSION {
            return Err(Error::Invalid);
        }
        if self.journal.is_none() {
            return Err(Error::Denied);
        }
        match r.kind {
            req::CREATE => self.create(r, by),
            req::GET => self.reply_for(r.id),
            req::LIST => {
                let t = self.tasks.get(r.value as usize).ok_or(Error::NotFound)?;
                self.reply_for(t.id)
            }
            req::MOVE => self.client_move(r, by),
            req::CLAIM => self.claim(),
            req::EFFECT_BEGIN => self.effect_begin(r),
            req::EFFECT_END => self.effect_end(r),
            req::STOP if r.id == 0 => self.stop_all(by),
            req::STOP => self.stop(r.id, by),
            req::CHECK => {
                let t = self.task(r.id)?;
                let go_on = matches!(t.state, state::RUNNING | state::WAITING_APPROVAL) && !t.stop_requested;
                Ok(TaskReply { value: u32::from(!go_on), ..self.reply_for(r.id)? })
            }
            req::RECONCILE => self.reconcile(r, by),
            req::STATS => Ok(self.stats()),
            req::RESUME => self.resume(by),
            req::HISTORY => self.history(r.id, r.value),
            req::TEXT => {
                let t = self.task(r.id)?;
                let s = match r.value {
                    text::TITLE => t.title.as_str(),
                    text::REASON => t.reason.as_str(),
                    text::WORKSPACE => t.workspace.as_str(),
                    text::CHECKPOINT => t.checkpoint.as_str(),
                    text::EFFECT => t.open_effect.as_deref().unwrap_or(""),
                    _ => return Err(Error::Invalid),
                };
                let s = String::from(s);
                let mut reply = self.reply_for(r.id)?;
                reply.set_reason(&s);
                Ok(reply)
            }
            req::CHECKPOINT => {
                let t = self.task(r.id)?;
                if t.state != state::RUNNING {
                    return Err(Error::Invalid);
                }
                let rec = Record::new(r.id, record::CHECKPOINT, now_ms()).with_text(r.text());
                self.commit(&[rec])?;
                self.reply_for(r.id)
            }
            _ => Err(Error::NoSys),
        }
    }

    fn stats(&self) -> TaskReply {
        let (which, epoch, bytes) =
            self.journal.as_ref().map(|j| (j.which, j.epoch, j.offset)).unwrap_or((0, 0, 0));
        TaskReply {
            total: self.tasks.len() as u32,
            value: self.replay.records,
            attempt: self.replay.damaged,
            transitions: self.replay.settled,
            max_attempts: which as u32,
            effects_done: epoch,
            changed_ms: bytes,
            flags: if self.halted { flags::HALTED } else { 0 },
            ..Default::default()
        }
    }

    fn create(&mut self, r: &TaskRequest, by: u32) -> Result<TaskReply, Error> {
        let title = r.text();
        if title.is_empty() {
            return Err(Error::Invalid);
        }
        let attempts = match r.value {
            0 => DEFAULT_ATTEMPTS,
            n @ 1..=MAX_ATTEMPTS => n,
            _ => return Err(Error::Invalid),
        };
        if r.id != 0 {
            self.task(r.id)?;
        }
        let now = now_ms();
        let mut recs = Vec::new();
        if self.tasks.len() >= MAX_TASKS {
            // The oldest finished task makes room; with none finished, the table is
            // full of work someone still cares about.
            let oldest = self
                .tasks
                .iter()
                .filter(|t| state::is_final(t.state))
                .map(|t| t.id)
                .min()
                .ok_or(Error::Quota)?;
            recs.push(Record::new(oldest, record::FORGET, now));
        }
        let id = self.next_id;
        let mut c = Record::new(id, record::CREATE, now).with_text(title);
        c.actor = by as u8;
        c.aux = r.id;
        c.max_attempts = attempts as u16;
        recs.push(c);
        if !r.scope().is_empty() {
            let mut w = Record::new(id, record::WORKSPACE, now).with_text(r.scope());
            w.actor = by as u8;
            recs.push(w);
        }
        self.commit(&recs)?;
        self.reply_for(id)
    }

    fn client_move(&mut self, r: &TaskRequest, by: u32) -> Result<TaskReply, Error> {
        let t = self.task(r.id)?;
        let (from, attempt, max, stop, effect) =
            (t.state, t.attempt, t.max_attempts, t.stop_requested, t.open_effect.clone());
        let mut to = r.to;
        if !state::allowed(from, to) {
            return Err(Error::Invalid);
        }
        // Approval is the user's: an agent does not wave its own work through.
        if from == state::WAITING_APPROVAL && to == state::RUNNING && by != actor::USER {
            return Err(Error::Denied);
        }
        let mut why = String::from(if r.text().is_empty() { "no reason given" } else { r.text() });
        if stop && from == state::RUNNING && matches!(to, state::PAUSED | state::WAITING_APPROVAL) {
            // A stop was asked for: the worker letting go means stopped, not parked.
            to = state::CANCELLED;
            why = alloc::format!("{why}; stopped, as asked");
        }
        if let Some(key) = effect {
            match to {
                // Nothing ends well, or waits, with an effect nobody confirmed.
                state::SUCCEEDED | state::PAUSED | state::WAITING_APPROVAL => return Err(Error::Busy),
                // Giving up now makes the effect a question to settle, not a failure
                // to retry or a cancellation to believe.
                state::FAILED | state::CANCELLED => {
                    let why = alloc::format!("{why}; effect '{key}' unconfirmed");
                    let rec = self.mv(r.id, state::NEEDS_RECONCILIATION, by, attempt, &why);
                    self.commit(&[rec])?;
                    return self.reply_for(r.id);
                }
                _ => {}
            }
        }
        let rec = if to == state::FAILED && attempt < max && !stop {
            // An attempt failed and the task has more: queued again, after a backoff
            // that doubles each time.
            let wait = BACKOFF_MS << attempt.saturating_sub(1).min(10);
            let why = alloc::format!(
                "attempt {attempt} of {max} failed ({}: {why}); again in {wait} ms",
                actor::name(by)
            );
            let rec = self.mv(r.id, state::QUEUED, actor::SERVICE, attempt, &why);
            self.commit(&[rec])?;
            self.task_mut(r.id)?.not_before_ms = sys::ticks_ms() + wait;
            return self.reply_for(r.id);
        } else if to == state::FAILED && stop {
            let why = alloc::format!("{why}; a stop was asked for, so it is not tried again");
            self.mv(r.id, to, by, attempt, &why)
        } else if to == state::FAILED {
            let why = alloc::format!("{why}; attempt {attempt} of {max}, none left");
            self.mv(r.id, to, by, attempt, &why)
        } else {
            self.mv(r.id, to, by, attempt, &why)
        };
        self.commit(&[rec])?;
        self.reply_for(r.id)
    }

    /// Dispatch: the oldest queued task whose time has come, unless dispatch is
    /// stopped or [`MAX_RUNNING`] tasks run.
    fn claim(&mut self) -> Result<TaskReply, Error> {
        if self.halted {
            return Err(Error::Denied);
        }
        if self.tasks.iter().filter(|t| t.state == state::RUNNING).count() >= MAX_RUNNING {
            return Err(Error::WouldBlock);
        }
        let now = sys::ticks_ms();
        let t = self
            .tasks
            .iter()
            .find(|t| t.state == state::QUEUED && t.not_before_ms <= now)
            .ok_or(Error::NotFound)?;
        let (id, attempt) = (t.id, t.attempt + 1);
        let why = alloc::format!("dispatched: attempt {attempt} of {}", t.max_attempts);
        let rec = self.mv(id, state::RUNNING, actor::SERVICE, attempt, &why);
        self.commit(&[rec])?;
        self.reply_for(id)
    }

    fn effect_begin(&mut self, r: &TaskRequest) -> Result<TaskReply, Error> {
        let t = self.task(r.id)?;
        if t.state != state::RUNNING || r.text().is_empty() {
            return Err(Error::Invalid);
        }
        if t.open_effect.is_some() || t.stop_requested {
            // One effect at a time, and none once a stop was asked for.
            return Err(Error::Busy);
        }
        let rec = Record::new(r.id, record::EFFECT_BEGIN, now_ms()).with_text(r.text());
        self.commit(&[rec])?;
        self.reply_for(r.id)
    }

    fn effect_end(&mut self, r: &TaskRequest) -> Result<TaskReply, Error> {
        let t = self.task(r.id)?;
        if t.open_effect.as_deref() != Some(r.text()) || r.value > 1 {
            return Err(Error::Invalid);
        }
        let mut rec = Record::new(r.id, record::EFFECT_END, now_ms()).with_text(r.text());
        rec.state = r.value as u8;
        self.commit(&[rec])?;
        self.reply_for(r.id)
    }

    /// Stop one task. What has not started again is cancelled at once; what runs is
    /// asked, may begin no further effect, and its worker says when it stopped.
    fn stop(&mut self, id: u32, by: u32) -> Result<TaskReply, Error> {
        let t = self.task(id)?;
        let rec = match t.state {
            s if state::is_final(s) => None,
            state::RUNNING if t.stop_requested => None,
            state::RUNNING => {
                let mut s = Record::new(id, record::STOP, now_ms());
                s.actor = by as u8;
                Some(s)
            }
            state::NEEDS_RECONCILIATION => Some(self.mv(
                id,
                state::CANCELLED,
                by,
                t.attempt,
                "stopped; its open effect was never confirmed either way",
            )),
            state::WAITING_APPROVAL => {
                Some(self.mv(id, state::CANCELLED, by, t.attempt, "stopped while it waited for approval"))
            }
            _ => Some(self.mv(id, state::CANCELLED, by, t.attempt, "stopped before it ran again")),
        };
        if let Some(rec) = rec {
            self.commit(&[rec])?;
        }
        // What already happened outside the machine is not undone by a stop.
        let reply = self.reply_for(id)?;
        Ok(TaskReply { value: reply.effects_done, ..reply })
    }

    /// Stop everything: dispatch stops, every running task is asked to stop, and the
    /// queued ones are held where they are -- not cancelled, and not shown as such.
    fn stop_all(&mut self, by: u32) -> Result<TaskReply, Error> {
        let now = now_ms();
        let mut recs = Vec::new();
        if !self.halted {
            let mut h = Record::new(0, record::HALT, now);
            h.state = 1;
            h.actor = by as u8;
            recs.push(h);
        }
        let mut running = 0;
        let mut effects = 0;
        for t in self.tasks.iter().filter(|t| t.state == state::RUNNING) {
            running += 1;
            effects += t.effects_done;
            if !t.stop_requested {
                let mut s = Record::new(t.id, record::STOP, now);
                s.actor = by as u8;
                recs.push(s);
            }
        }
        if !recs.is_empty() {
            self.commit(&recs)?;
        }
        let held = self.tasks.iter().filter(|t| t.state == state::QUEUED).count() as u32;
        Ok(TaskReply {
            total: running,
            value: held,
            effects_done: effects,
            flags: flags::HALTED,
            ..Default::default()
        })
    }

    fn resume(&mut self, by: u32) -> Result<TaskReply, Error> {
        if self.halted {
            let mut h = Record::new(0, record::HALT, now_ms());
            h.state = 0;
            h.actor = by as u8;
            self.commit(&[h])?;
        }
        Ok(self.stats())
    }

    fn reconcile(&mut self, r: &TaskRequest, by: u32) -> Result<TaskReply, Error> {
        let t = self.task(r.id)?;
        if t.state != state::NEEDS_RECONCILIATION || r.value > 1 {
            return Err(Error::Invalid);
        }
        let key = t.open_effect.clone().unwrap_or_default();
        let (attempt, max) = (t.attempt, t.max_attempts);
        let how = if r.text().is_empty() { "no reason given" } else { r.text() };
        let mut end = Record::new(r.id, record::EFFECT_END, now_ms()).with_text(&key);
        end.state = r.value as u8;
        let mv = if r.value == 1 {
            let why = alloc::format!("reconciled, {how}: '{key}' happened");
            self.mv(r.id, state::PAUSED, by, attempt, &why)
        } else if attempt < max {
            let why =
                alloc::format!("reconciled, {how}: '{key}' did not happen; attempt {} next", attempt + 1);
            self.mv(r.id, state::QUEUED, by, attempt, &why)
        } else {
            let why = alloc::format!("reconciled, {how}: '{key}' did not happen; no attempt left");
            self.mv(r.id, state::FAILED, by, attempt, &why)
        };
        self.commit(&[end, mv])?;
        self.reply_for(r.id)
    }

    /// The `n`-th record the journal holds for task `id`, read back from the disk.
    fn history(&self, id: u32, n: u32) -> Result<TaskReply, Error> {
        self.task(id)?;
        let j = self.journal.as_ref().ok_or(Error::Denied)?;
        let mut buf = alloc::vec![0u8; READ_CHUNK];
        let mut offset = RECORD;
        let mut count = 0u32;
        let mut found: Option<Record> = None;
        while offset + RECORD <= j.offset {
            let want = (((j.offset - offset) as usize).min(READ_CHUNK) / RECORD_BYTES) * RECORD_BYTES;
            let got = sys::fs_read(j.file, offset, &mut buf[..want])?;
            let whole = got - got % RECORD_BYTES;
            if whole == 0 {
                break;
            }
            for r in buf[..whole].chunks_exact(RECORD_BYTES).filter_map(Record::open) {
                if r.task == id && r.kind != record::HALT && r.kind != record::HEADER {
                    if count == n {
                        found = Some(r);
                    }
                    count += 1;
                }
            }
            offset += whole as u64;
        }
        let r = found.ok_or(Error::NotFound)?;
        let mut reply = TaskReply {
            id,
            value: u32::from(r.kind),
            state: u32::from(r.state),
            actor: u32::from(r.actor),
            attempt: u32::from(r.attempt),
            max_attempts: u32::from(r.max_attempts),
            changed_ms: r.time_ms,
            total: count,
            ..Default::default()
        };
        reply.set_reason(r.text());
        Ok(reply)
    }
}

fn reply(ch: Handle, r: &Result<TaskReply, Error>) {
    let out = match r {
        Ok(r) => *r,
        Err(e) => TaskReply::failed(*e),
    };
    let _ = sys::send(ch, out.as_bytes(), None);
}

#[unsafe(no_mangle)]
pub extern "C" fn space_main() -> i32 {
    libspace::heap::set_pages(HEAP_PAGES);
    println!("[task] Space OS Task Service, ABI v{ABI_VERSION}");
    let mut svc = Service::new();
    let mut buf = [0u8; core::mem::size_of::<TaskRequest>()];
    let mut handles: Vec<Handle> = Vec::new();
    loop {
        handles.clear();
        handles.push(handle::BOOTSTRAP);
        handles.extend(svc.sessions.iter().map(|s| s.ch));
        let ready = match sys::wait_any(&handles, WAIT_FOREVER) {
            Ok(i) => i,
            Err(e) => {
                println!("[task] wait failed: {e}");
                break;
            }
        };
        let ch = handles[ready];
        let operator = ready == 0;
        // The operator acts for the user; a session as whoever it was opened for.
        let by = if operator { actor::USER } else { svc.sessions[ready - 1].actor };
        match sys::recv(ch, &mut buf, true) {
            Ok((n, transferred)) => {
                let Some(r) = TaskRequest::from_bytes(&buf[..n]) else {
                    if let Some(h) = transferred {
                        sys::handle_close(h).ok();
                    }
                    reply(ch, &Err(Error::MsgSize));
                    continue;
                };
                let result = match (r.kind, transferred) {
                    (req::HELLO, Some(root)) if operator && svc.root.is_none() => {
                        svc.root = Some(root);
                        match svc.open_journal(root) {
                            Ok(()) => {
                                let j = svc.journal.as_ref().expect("the journal was just opened");
                                println!(
                                    "[task] journal {} (epoch {}): {} record(s), {} damaged and skipped, {} task(s), {} settled after a restart",
                                    JOURNAL_PATHS[j.which],
                                    j.epoch,
                                    svc.replay.records,
                                    svc.replay.damaged,
                                    svc.tasks.len(),
                                    svc.replay.settled
                                );
                                Ok(svc.stats())
                            }
                            Err(e) => {
                                println!("[task] cannot keep a journal: {e}");
                                Err(e)
                            }
                        }
                    }
                    (req::SESSION, Some(chan)) if operator => {
                        if !matches!(r.actor, actor::USER | actor::AGENT) {
                            sys::handle_close(chan).ok();
                            Err(Error::Invalid)
                        } else if svc.sessions.len() >= SESSIONS_MAX {
                            sys::handle_close(chan).ok();
                            Err(Error::Busy)
                        } else {
                            svc.sessions.push(Session { ch: chan, actor: r.actor });
                            Ok(TaskReply::default())
                        }
                    }
                    (_, Some(h)) => {
                        sys::handle_close(h).ok();
                        Err(Error::Invalid)
                    }
                    (req::QUIT, None) if operator => {
                        reply(ch, &Ok(TaskReply::default()));
                        break;
                    }
                    (req::COMPACT, None) if operator => svc.compact().map(|()| svc.stats()),
                    (req::HELLO, None) if r.abi_version == ABI_VERSION => {
                        Ok(TaskReply { value: ABI_VERSION, ..svc.stats() })
                    }
                    (req::HELLO, None) => Err(Error::Invalid),
                    (req::SESSION | req::QUIT | req::COMPACT, None) => Err(Error::Denied),
                    (_, None) => svc.handle(&r, by),
                };
                reply(ch, &result);
            }
            Err(Error::WouldBlock) => {}
            Err(Error::PeerClosed) if operator => {
                println!("[task] operator disconnected");
                break;
            }
            Err(e) if operator => {
                println!("[task] receive failed: {e}");
                break;
            }
            Err(_) => {
                // A client went away. Its tasks did not: that is the point.
                svc.sessions.retain(|s| s.ch != ch);
                sys::handle_close(ch).ok();
            }
        }
    }
    for s in svc.sessions.drain(..) {
        sys::handle_close(s.ch).ok();
    }
    if let Some(j) = svc.journal.take() {
        sys::handle_close(j.file).ok();
    }
    if let Some(r) = svc.root.take() {
        sys::handle_close(r).ok();
    }
    println!("[task] closing: {} task(s) kept in the journal", svc.tasks.len());
    0
}
