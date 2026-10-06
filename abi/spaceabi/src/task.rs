//! Task Service protocol (T01, T02, G02; ADR-0036).
//!
//! A task's identity and state live outside any window: in `spacetask`, which writes
//! every change to a journal on the data volume before it answers. Closing the
//! client that made a task, restarting the service or rebooting the machine leaves
//! the task where it was -- with one exception, and it is the point: what was running
//! when the service went away is not assumed to have finished, failed or never
//! happened. A task that had started an external effect and not confirmed it is
//! `NEEDS_RECONCILIATION`, and nothing retries it until someone says what happened.
//!
//! The journal is a sequence of fixed 128-byte [`Record`]s, each with its own
//! checksum, so a record cut short by a crash is recognised and skipped rather than
//! read as something it is not. It lives in one of two files ([`JOURNAL_PATHS`]);
//! the one whose [`record::HEADER`] carries the higher epoch is the journal, and
//! compaction writes the other one whole before its header makes it so.

pub const ABI_VERSION: u32 = 0;

/// Bytes of text (a title, a reason, an effect's key) a request or record carries.
pub const TEXT_MAX: usize = 88;
/// Tasks the service holds at once. A new task takes the place of the oldest
/// finished one when they are all taken; when none has finished, `Quota`.
pub const MAX_TASKS: usize = 64;
/// Tasks that may be running at once: dispatch stops there (PRD §13).
pub const MAX_RUNNING: usize = 2;
/// Attempts a task gets unless it asks for others, and the most it may ask for.
pub const DEFAULT_ATTEMPTS: u32 = 3;
pub const MAX_ATTEMPTS: u32 = 100;
/// The first retry waits this long; each further one twice as long as the last.
pub const BACKOFF_MS: u64 = 100;
/// The journal's two files on the data volume, used in turn.
pub const JOURNAL_PATHS: [&str; 2] = ["/spaceos/var/tasks0.log", "/spaceos/var/tasks1.log"];
/// Size of one journal record.
pub const RECORD_BYTES: usize = 128;
/// First bytes of every record.
pub const RECORD_MAGIC: u32 = u32::from_le_bytes(*b"TSK1");
/// Records a journal file holds before what it says is compacted into the other.
pub const COMPACT_RECORDS: u32 = 1024;

/// The states of a task (PRD §13).
pub mod state {
    pub const QUEUED: u32 = 1;
    pub const RUNNING: u32 = 2;
    pub const WAITING_APPROVAL: u32 = 3;
    pub const PAUSED: u32 = 4;
    pub const SUCCEEDED: u32 = 5;
    pub const FAILED: u32 = 6;
    pub const CANCELLED: u32 = 7;
    pub const NEEDS_RECONCILIATION: u32 = 8;

    pub fn name(s: u32) -> &'static str {
        match s {
            QUEUED => "queued",
            RUNNING => "running",
            WAITING_APPROVAL => "waiting_approval",
            PAUSED => "paused",
            SUCCEEDED => "succeeded",
            FAILED => "failed",
            CANCELLED => "cancelled",
            NEEDS_RECONCILIATION => "needs_reconciliation",
            _ => "unknown",
        }
    }

    /// Nothing leaves these.
    pub fn is_final(s: u32) -> bool {
        matches!(s, SUCCEEDED | FAILED | CANCELLED)
    }

    /// The moves a client may ask for with `req::MOVE`. Dispatch (`QUEUED` to
    /// `RUNNING`) happens only through `req::CLAIM`, and leaving
    /// `NEEDS_RECONCILIATION` other than by giving up only through `req::RECONCILE`,
    /// which say more than a state can. Approval (`WAITING_APPROVAL` to `RUNNING`)
    /// is the user's alone; the service enforces that, not this table.
    pub fn allowed(from: u32, to: u32) -> bool {
        match from {
            QUEUED => matches!(to, PAUSED | CANCELLED),
            RUNNING => matches!(to, WAITING_APPROVAL | PAUSED | SUCCEEDED | FAILED | CANCELLED),
            WAITING_APPROVAL => matches!(to, RUNNING | PAUSED | CANCELLED),
            PAUSED => matches!(to, QUEUED | CANCELLED),
            NEEDS_RECONCILIATION => matches!(to, CANCELLED),
            _ => false,
        }
    }
}

/// Who moved a task. A session's actor is fixed when the operator opens it; a
/// client cannot say it is someone else.
pub mod actor {
    pub const USER: u32 = 1;
    pub const AGENT: u32 = 2;
    /// The service itself: dispatch, retry, the policy after a restart.
    pub const SERVICE: u32 = 3;

    pub fn name(a: u32) -> &'static str {
        match a {
            USER => "user",
            AGENT => "agent",
            SERVICE => "service",
            _ => "unknown",
        }
    }
}

/// Bits of [`TaskReply::flags`].
pub mod flags {
    /// A stop was asked for and the worker has not yet said it stopped.
    pub const STOP_REQUESTED: u32 = 1;
    /// An external effect began and has not been settled.
    pub const EFFECT_OPEN: u32 = 2;
    /// Dispatch is stopped for every task (`req::STOP` of task 0).
    pub const HALTED: u32 = 4;
}

/// The texts a task holds, for `req::TEXT`.
pub mod text {
    pub const TITLE: u32 = 0;
    /// Why the last transition happened.
    pub const REASON: u32 = 1;
    /// The workspace the task works in, empty when none was given.
    pub const WORKSPACE: u32 = 2;
    /// What the worker last saved to resume from.
    pub const CHECKPOINT: u32 = 3;
    /// The key of the open external effect.
    pub const EFFECT: u32 = 4;
}

pub mod req {
    /// Negotiate the version. From the operator (bootstrap channel) it carries the
    /// file capability (`FS | FS_WRITE`) the journal is kept with.
    pub const HELLO: u32 = 0;
    /// A new task titled `text`, working in the workspace `scope` (may be empty),
    /// part of task `id` (0: none), allowed `value` attempts (0: the default). Its
    /// owner is the session's actor.
    pub const CREATE: u32 = 1;
    /// Task `id` as it stands.
    pub const GET: u32 = 2;
    /// The `value`-th task the service holds (oldest first); `total` is how many.
    pub const LIST: u32 = 3;
    /// Move task `id` to state `to`, because `text`.
    pub const MOVE: u32 = 4;
    /// A worker asks for work: the oldest queued task whose retry time has come is
    /// running from now, on its next attempt. `Denied` while dispatch is stopped,
    /// `WouldBlock` when [`super::MAX_RUNNING`] tasks already run, `NotFound` when
    /// nothing is ready.
    pub const CLAIM: u32 = 5;
    /// The worker running task `id` is about to cause an effect outside the machine,
    /// keyed `text` (an idempotency key where the other side understands one).
    /// `Busy` while another is open or once a stop was asked for.
    pub const EFFECT_BEGIN: u32 = 6;
    /// The effect keyed `text` of task `id` is over: it happened (`value` 1) or it
    /// did not (0).
    pub const EFFECT_END: u32 = 7;
    /// Stop task `id`. Queued, paused, waiting or in need of reconciliation:
    /// cancelled at once. Running: its worker is asked (`CHECK`) and may begin no
    /// further effect; the task is cancelled when the worker says it stopped. The
    /// answer's `value` is how many of the task's effects happened and are not
    /// undone. Task 0 is every task: dispatch stops, every running task is asked to
    /// stop, and queued ones are held, not cancelled; `total` is how many running
    /// tasks were asked, `value` how many queued ones are held, `effects_done` the
    /// effects those running tasks already caused.
    pub const STOP: u32 = 8;
    /// A worker asks whether task `id` should stop: `value` 1 when it should (a stop
    /// was asked for, or the task is no longer running or waiting for approval).
    pub const CHECK: u32 = 9;
    /// Settle task `id` in `NEEDS_RECONCILIATION`: its open effect did happen
    /// (`value` 1: counted, and the task is paused to be resumed past it) or it did
    /// not (0: queued again as a further attempt if it has one left, failed if not).
    /// `text` says how that was found out.
    pub const RECONCILE: u32 = 10;
    /// What the service found when it read the journal and where it writes now:
    /// `total` tasks, `value` records read, `attempt` damaged records skipped,
    /// `transitions` tasks moved because the service had gone away while they ran,
    /// `max_attempts` the journal file in use (an index into [`super::JOURNAL_PATHS`]),
    /// `effects_done` its epoch, `changed_ms` its length in bytes, `flags` with
    /// [`super::flags::HALTED`].
    pub const STATS: u32 = 11;
    /// The operator ends the service.
    pub const QUIT: u32 = 12;
    /// The operator: a session of its own, acting as `actor` (`USER` or `AGENT`),
    /// for the client at the end of the channel sent with this request.
    pub const SESSION: u32 = 13;
    /// Dispatch again after a stop of every task.
    pub const RESUME: u32 = 14;
    /// The `value`-th record the journal holds for task `id` (0: how it began):
    /// `value` its [`super::record`] kind, `state`, `actor`, `attempt`, `changed_ms`
    /// and `reason` as written; `total` how many there are.
    pub const HISTORY: u32 = 15;
    /// Text `value` ([`super::text`]) of task `id`, in `reason`.
    pub const TEXT: u32 = 16;
    /// The worker running task `id` saves `text` to resume from.
    pub const CHECKPOINT: u32 = 17;
    /// The operator: compact the journal into its other file now.
    pub const COMPACT: u32 = 18;
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct TaskRequest {
    pub kind: u32,
    pub abi_version: u32,
    pub id: u32,
    /// Target state for `MOVE`.
    pub to: u32,
    /// The session's actor, for `SESSION`; ignored otherwise.
    pub actor: u32,
    pub value: u32,
    pub text_len: u32,
    pub scope_len: u32,
    pub text: [u8; TEXT_MAX],
    /// `CREATE`: the workspace.
    pub scope: [u8; TEXT_MAX],
}

impl Default for TaskRequest {
    fn default() -> Self {
        TaskRequest {
            kind: 0,
            abi_version: ABI_VERSION,
            id: 0,
            to: 0,
            actor: 0,
            value: 0,
            text_len: 0,
            scope_len: 0,
            text: [0; TEXT_MAX],
            scope: [0; TEXT_MAX],
        }
    }
}

impl TaskRequest {
    pub fn new(kind: u32) -> TaskRequest {
        TaskRequest { kind, ..Default::default() }
    }

    pub fn for_task(kind: u32, id: u32) -> TaskRequest {
        TaskRequest { kind, id, ..Default::default() }
    }

    pub fn with_text(mut self, t: &str) -> TaskRequest {
        self.text_len = copy_text(&mut self.text, t);
        self
    }

    pub fn with_scope(mut self, t: &str) -> TaskRequest {
        self.scope_len = copy_text(&mut self.scope, t);
        self
    }

    pub fn text(&self) -> &str {
        text_of(&self.text, self.text_len)
    }

    pub fn scope(&self) -> &str {
        text_of(&self.scope, self.scope_len)
    }

    pub fn as_bytes(&self) -> &[u8] {
        // SAFETY: `repr(C)` plain data without padding (sizes checked in the tests).
        unsafe { core::slice::from_raw_parts(self as *const Self as *const u8, core::mem::size_of::<Self>()) }
    }

    pub fn from_bytes(b: &[u8]) -> Option<TaskRequest> {
        (b.len() == core::mem::size_of::<Self>())
            // SAFETY: the length matches, and every bit pattern is a valid request.
            .then(|| unsafe { core::ptr::read_unaligned(b.as_ptr() as *const TaskRequest) })
    }
}

/// A task as the service holds it, or a short answer.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct TaskReply {
    /// 0 or a negated [`crate::error::Error`].
    pub status: i32,
    pub id: u32,
    pub state: u32,
    /// Attempts made so far, and allowed.
    pub attempt: u32,
    pub max_attempts: u32,
    /// External effects that happened.
    pub effects_done: u32,
    /// Transitions in the task's history.
    pub transitions: u32,
    /// [`flags`].
    pub flags: u32,
    /// The actor of the last transition.
    pub actor: u32,
    /// Who made the task, and the task it is part of (0: none).
    pub owner: u32,
    pub parent: u32,
    /// Tasks held (`LIST`, `STATS`), or the request's own count.
    pub total: u32,
    /// The request's own answer (`CHECK`, `STOP`, `HISTORY`, `STATS`).
    pub value: u32,
    pub _pad: u32,
    /// Wall-clock time of the last transition (ms since the Unix epoch).
    pub changed_ms: u64,
    pub title_len: u32,
    pub reason_len: u32,
    pub title: [u8; TEXT_MAX],
    /// Why the last transition happened (or the text `TEXT` asked for).
    pub reason: [u8; TEXT_MAX],
}

impl Default for TaskReply {
    fn default() -> Self {
        TaskReply {
            status: 0,
            id: 0,
            state: 0,
            attempt: 0,
            max_attempts: 0,
            effects_done: 0,
            transitions: 0,
            flags: 0,
            actor: 0,
            owner: 0,
            parent: 0,
            total: 0,
            value: 0,
            _pad: 0,
            changed_ms: 0,
            title_len: 0,
            reason_len: 0,
            title: [0; TEXT_MAX],
            reason: [0; TEXT_MAX],
        }
    }
}

impl TaskReply {
    pub fn failed(e: crate::error::Error) -> TaskReply {
        TaskReply { status: -(e as i32), ..Default::default() }
    }

    pub fn set_title(&mut self, t: &str) {
        self.title_len = copy_text(&mut self.title, t);
    }

    pub fn set_reason(&mut self, t: &str) {
        self.reason_len = copy_text(&mut self.reason, t);
    }

    pub fn title(&self) -> &str {
        text_of(&self.title, self.title_len)
    }

    pub fn reason(&self) -> &str {
        text_of(&self.reason, self.reason_len)
    }

    pub fn has(&self, flag: u32) -> bool {
        self.flags & flag != 0
    }

    pub fn result(&self) -> Result<(), crate::error::Error> {
        if self.status == 0 {
            Ok(())
        } else {
            Err(crate::error::Error::from_code(self.status.unsigned_abs())
                .unwrap_or(crate::error::Error::Invalid))
        }
    }

    pub fn as_bytes(&self) -> &[u8] {
        // SAFETY: `repr(C)` plain data without padding (sizes checked in the tests).
        unsafe { core::slice::from_raw_parts(self as *const Self as *const u8, core::mem::size_of::<Self>()) }
    }

    pub fn from_bytes(b: &[u8]) -> Option<TaskReply> {
        (b.len() == core::mem::size_of::<Self>())
            // SAFETY: the length matches, and every bit pattern is a valid reply.
            .then(|| unsafe { core::ptr::read_unaligned(b.as_ptr() as *const TaskReply) })
    }
}

/// Kinds of journal record.
pub mod record {
    /// A task is born: `text` its title, `actor` its owner, `aux` its parent,
    /// `max_attempts` its allowance.
    pub const CREATE: u8 = 1;
    /// A task moved to `state`, by `actor`, on `attempt`, because `text`.
    pub const MOVE: u8 = 2;
    /// An external effect keyed `text` is about to happen.
    pub const EFFECT_BEGIN: u8 = 3;
    /// The effect keyed `text` happened (`state` 1) or did not (0).
    pub const EFFECT_END: u8 = 4;
    /// `actor` asked the task to stop.
    pub const STOP: u8 = 5;
    /// The task's workspace is `text`.
    pub const WORKSPACE: u8 = 6;
    /// The worker saved `text` to resume from.
    pub const CHECKPOINT: u8 = 7;
    /// Compaction: the task as it stood -- `state`, `actor`, `attempt`, `time_ms` and
    /// `text` of its last transition; `aux` its transitions (high 16 bits) and the
    /// effects that happened (low 16 bits).
    pub const SNAPSHOT: u8 = 8;
    /// A finished task made room for a new one.
    pub const FORGET: u8 = 9;
    /// `actor` stopped (`state` 1) or resumed (0) dispatch for every task.
    pub const HALT: u8 = 10;
    /// The first record of a journal file; `aux` is its epoch.
    pub const HEADER: u8 = 11;

    pub fn name(k: u8) -> &'static str {
        match k {
            CREATE => "create",
            MOVE => "move",
            EFFECT_BEGIN => "effect_begin",
            EFFECT_END => "effect_end",
            STOP => "stop",
            WORKSPACE => "workspace",
            CHECKPOINT => "checkpoint",
            SNAPSHOT => "snapshot",
            FORGET => "forget",
            HALT => "halt",
            HEADER => "header",
            _ => "unknown",
        }
    }
}

/// One journal record: fixed size, so a record never spans two sectors, and a
/// checksum over the rest, so one that was cut short is known for what it is.
#[repr(C)]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Record {
    pub magic: u32,
    /// Strictly increasing within a journal file.
    pub seq: u32,
    pub task: u32,
    pub kind: u8,
    pub state: u8,
    pub actor: u8,
    pub text_len: u8,
    pub attempt: u16,
    pub max_attempts: u16,
    /// Kind-specific (see [`record`]).
    pub aux: u32,
    /// Wall-clock time (ms since the Unix epoch).
    pub time_ms: u64,
    pub text: [u8; TEXT_MAX],
    /// The first 8 bytes of SHA-256 over everything before this field.
    pub check: [u8; 8],
}

const _: () = assert!(core::mem::size_of::<Record>() == RECORD_BYTES);

impl Record {
    pub fn new(task: u32, kind: u8, time_ms: u64) -> Record {
        Record {
            magic: RECORD_MAGIC,
            seq: 0,
            task,
            kind,
            state: 0,
            actor: 0,
            text_len: 0,
            attempt: 0,
            max_attempts: 0,
            aux: 0,
            time_ms,
            text: [0; TEXT_MAX],
            check: [0; 8],
        }
    }

    pub fn with_text(mut self, t: &str) -> Record {
        self.text_len = copy_text(&mut self.text, t) as u8;
        self
    }

    pub fn text(&self) -> &str {
        text_of(&self.text, u32::from(self.text_len))
    }

    fn body(&self) -> &[u8] {
        // SAFETY: `repr(C)` plain data without padding (size asserted above).
        let all = unsafe { core::slice::from_raw_parts(self as *const Self as *const u8, RECORD_BYTES) };
        &all[..RECORD_BYTES - 8]
    }

    fn checksum(&self) -> [u8; 8] {
        let d = crate::sha256::digest(self.body());
        let mut c = [0u8; 8];
        c.copy_from_slice(&d[..8]);
        c
    }

    /// The record with its checksum, as it goes to disk.
    pub fn sealed(mut self) -> [u8; RECORD_BYTES] {
        self.check = self.checksum();
        let mut out = [0u8; RECORD_BYTES];
        // SAFETY: as in `body`.
        out.copy_from_slice(unsafe {
            core::slice::from_raw_parts(&self as *const Self as *const u8, RECORD_BYTES)
        });
        out
    }

    /// A record read back from disk, if it is one: the magic, the checksum and the
    /// text length must all hold.
    pub fn open(bytes: &[u8]) -> Option<Record> {
        if bytes.len() != RECORD_BYTES {
            return None;
        }
        // SAFETY: the length matches, and every bit pattern is a valid `Record`.
        let r: Record = unsafe { core::ptr::read_unaligned(bytes.as_ptr() as *const Record) };
        (r.magic == RECORD_MAGIC && usize::from(r.text_len) <= TEXT_MAX && r.check == r.checksum())
            .then_some(r)
    }
}

fn copy_text(dst: &mut [u8; TEXT_MAX], t: &str) -> u32 {
    // Whole characters only: a cut in the middle of one would not read back.
    let mut n = t.len().min(TEXT_MAX);
    while n > 0 && !t.is_char_boundary(n) {
        n -= 1;
    }
    dst[..n].copy_from_slice(&t.as_bytes()[..n]);
    n as u32
}

fn text_of(b: &[u8; TEXT_MAX], len: u32) -> &str {
    let n = (len as usize).min(TEXT_MAX);
    core::str::from_utf8(&b[..n]).unwrap_or("")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn messages_fit_and_have_no_padding() {
        assert_eq!(core::mem::size_of::<TaskRequest>(), 8 * 4 + 2 * TEXT_MAX);
        assert_eq!(core::mem::size_of::<TaskReply>(), 14 * 4 + 8 + 2 * 4 + 2 * TEXT_MAX);
        assert!(core::mem::size_of::<TaskReply>() <= crate::syscall::MSG_MAX);
        assert!(core::mem::size_of::<TaskRequest>() <= crate::syscall::MSG_MAX);
        assert_eq!(RECORD_BYTES % 128, 0);
        assert_eq!(512 % RECORD_BYTES, 0, "a record never spans two sectors");
    }

    #[test]
    fn a_record_reads_back_and_a_damaged_one_does_not() {
        let mut r = Record::new(3, record::MOVE, 1_790_000_000_000).with_text("approved by the user");
        r.seq = 7;
        let bytes = r.sealed();
        let back = Record::open(&bytes).expect("a sealed record opens");
        assert_eq!(back.seq, 7);
        assert_eq!(back.text(), "approved by the user");
        // Every single flipped bit is caught, and so is a record cut short.
        for i in 0..RECORD_BYTES * 8 {
            let mut d = bytes;
            d[i / 8] ^= 1 << (i % 8);
            assert!(Record::open(&d).is_none(), "bit {i} flipped and the record still opened");
        }
        assert!(Record::open(&bytes[..100]).is_none());
        let mut torn = [0u8; RECORD_BYTES];
        torn[..60].copy_from_slice(&bytes[..60]);
        assert!(Record::open(&torn).is_none());
        assert!(Record::open(&[0u8; RECORD_BYTES]).is_none(), "an empty slot is not a record");
    }

    #[test]
    fn text_is_cut_on_a_character_boundary() {
        let long = "é".repeat(60); // 120 bytes, two per character
        let r = TaskRequest::new(req::CREATE).with_text(&long).with_scope(&long);
        assert_eq!(r.text_len as usize, TEXT_MAX);
        assert_eq!(r.text().chars().count(), TEXT_MAX / 2);
        assert_eq!(r.scope(), r.text());
        let rec = Record::new(1, record::CREATE, 0).with_text(&long);
        assert_eq!(rec.text().chars().count(), TEXT_MAX / 2);
    }

    #[test]
    fn the_state_machine_has_no_way_out_of_a_final_state() {
        for from in 1..=8 {
            for to in 1..=8 {
                if state::is_final(from) {
                    assert!(!state::allowed(from, to), "{} -> {}", state::name(from), state::name(to));
                }
            }
            // Dispatch and reconciliation have requests of their own.
            assert!(!state::allowed(from, state::RUNNING) || from == state::WAITING_APPROVAL);
            assert!(!state::allowed(from, state::NEEDS_RECONCILIATION));
            assert!(!state::allowed(from, from), "{} -> itself", state::name(from));
        }
        // An uncertain effect is never retried by a move: only cancelled.
        assert!(!state::allowed(state::NEEDS_RECONCILIATION, state::QUEUED));
        assert!(!state::allowed(state::NEEDS_RECONCILIATION, state::SUCCEEDED));
    }

    #[test]
    fn an_error_travels_as_its_negated_code() {
        let r = TaskReply::failed(crate::error::Error::Busy);
        assert_eq!(r.result(), Err(crate::error::Error::Busy));
        assert_eq!(TaskReply::default().result(), Ok(()));
    }
}
