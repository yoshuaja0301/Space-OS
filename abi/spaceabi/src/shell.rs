//! Control protocol of `spaceshell`, the supervising session service (U01).
//!
//! The shell owns the console session and the lifetime of its worker. Everything a
//! front end does - ask for status, list a directory, start a job, stop it - is one
//! fixed-size message over one channel, so the shell's only blocking point is its
//! own control channel and a wedged worker can never take the session with it.

/// Protocol version, negotiated by [`cmd::HELLO`] before anything else is accepted.
pub const ABI_VERSION: u32 = 0;

/// Bytes of the text field of a command (a path or a job name).
pub const TEXT_MAX: usize = 64;
/// Bytes of the text field of a reply.
pub const REPLY_TEXT_MAX: usize = 160;

pub mod cmd {
    /// Negotiate the version and hand the shell the capabilities it runs with.
    pub const HELLO: u32 = 0;
    /// Current worker state and the last job's outcome.
    pub const STATUS: u32 = 1;
    /// List a directory (the file manager view); `text` is the path.
    pub const LIST: u32 = 2;
    /// Start a job; `text` names it (see the `job` module).
    pub const RUN: u32 = 3;
    /// Stop the running job. Succeeds whether the job is computing, blocked or wedged.
    pub const STOP: u32 = 4;
    /// Close the session; the shell exits 0.
    pub const QUIT: u32 = 5;
    /// What the running (or last) job has done so far: answered with a [`Progress`]
    /// instead of a [`Reply`].
    pub const PROGRESS: u32 = 6;
    /// Show that the session can do what an OS is for (PRD v0.2 §7.4, "OS usable"):
    /// read a file, run a program to its end, and stop another while it runs. The
    /// shell says the outcome on the console as the boot's status; the reply's
    /// `value` is 1 when it could and 0 when it could not, with the account in
    /// `text`. The programs are the check's own, never the session's worker.
    pub const CHECK: u32 = 7;
}

/// Job names `cmd::RUN` accepts. [`job::INFER`] is the real thing; the others exist
/// so a test can put a worker into each state an inference job can reach.
pub mod job {
    /// Generate text with the model on the guest disk, through the compute service
    /// (`bin/spaceai` in session mode). Reports every token and stops between two
    /// compute steps when asked. `infer <path>` runs on another file instead, which
    /// must still be the model the manifest names: a damaged copy is refused, with
    /// the reason, before a single step runs (A02).
    pub const INFER: &str = "infer";
    /// Finishes quickly and exits 0.
    pub const OK: &str = "ok";
    /// Dereferences NULL: killed by the kernel, the shell must survive it.
    pub const CRASH: &str = "crash";
    /// Spins without ever entering the kernel: only preemption keeps the shell alive.
    pub const HANG: &str = "hang";
    /// Sleeps for far longer than any test waits: stopped while blocked.
    pub const SLOW: &str = "slow";
    /// Takes memory until it is refused, then dies the way a program that does not
    /// handle running out does: a panic, exit code 101 (A02).
    pub const OOM: &str = "oom";
    /// Takes memory until it is refused, says [`FULL`] on its job channel, and holds
    /// on to all of it until it is stopped (A02).
    pub const HOG: &str = "hog";
    /// What a [`HOG`] says once the kernel has refused it a single page.
    pub const FULL: &[u8] = b"full";
}

pub mod worker_state {
    /// No worker has run yet.
    pub const IDLE: u32 = 0;
    /// A worker is running now.
    pub const RUNNING: u32 = 1;
    /// The last worker exited on its own.
    pub const DONE: u32 = 2;
    /// The last worker was killed by the kernel (fault).
    pub const CRASHED: u32 = 3;
    /// The last worker was stopped on request.
    pub const STOPPED: u32 = 4;
}

/// A command from a front end to the shell.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct Command {
    pub kind: u32,
    pub abi_version: u32,
    pub text_len: u32,
    pub _pad: u32,
    pub text: [u8; TEXT_MAX],
}

impl Default for Command {
    fn default() -> Self {
        Command { kind: 0, abi_version: 0, text_len: 0, _pad: 0, text: [0; TEXT_MAX] }
    }
}

impl Command {
    pub fn new(kind: u32, text: &str) -> Command {
        let mut c = Command { kind, ..Default::default() };
        let n = text.len().min(TEXT_MAX);
        c.text[..n].copy_from_slice(&text.as_bytes()[..n]);
        c.text_len = n as u32;
        c
    }

    pub fn text(&self) -> &str {
        let n = (self.text_len as usize).min(TEXT_MAX);
        core::str::from_utf8(&self.text[..n]).unwrap_or("")
    }
}

/// The shell's answer. `status` is 0 or a negated [`crate::error::Error`].
#[repr(C)]
#[derive(Clone, Copy)]
pub struct Reply {
    pub status: i32,
    pub state: u32,
    /// Exit code of the last job, or the entry count of a listing.
    pub value: u64,
    /// Kill reason of the last job (0 when it exited normally).
    pub reason: u32,
    /// Commands the shell has served, so a caller can prove it stayed alive.
    pub served: u32,
    pub text_len: u32,
    pub _pad: u32,
    pub text: [u8; REPLY_TEXT_MAX],
}

impl Default for Reply {
    fn default() -> Self {
        Reply {
            status: 0,
            state: worker_state::IDLE,
            value: 0,
            reason: 0,
            served: 0,
            text_len: 0,
            _pad: 0,
            text: [0; REPLY_TEXT_MAX],
        }
    }
}

impl Reply {
    pub fn text(&self) -> &str {
        let n = (self.text_len as usize).min(REPLY_TEXT_MAX);
        core::str::from_utf8(&self.text[..n]).unwrap_or("")
    }

    pub fn set_text(&mut self, s: &str) {
        let n = s.len().min(REPLY_TEXT_MAX);
        self.text[..n].copy_from_slice(&s.as_bytes()[..n]);
        self.text_len = n as u32;
    }

    pub fn result(&self) -> Result<u64, crate::error::Error> {
        if self.status == 0 {
            Ok(self.value)
        } else {
            Err(crate::error::Error::from_code((-self.status) as u32).unwrap_or(crate::error::Error::Invalid))
        }
    }
}

/// Bytes of generated text a [`Progress`] carries: the tail of what the job wrote.
pub const PROGRESS_TEXT_MAX: usize = 64;

/// What an inference job has done, for an Agent Center: sent in answer to
/// [`cmd::PROGRESS`]. Everything is zero for a job that reports nothing (the test
/// jobs), and stays at its last value once the job has ended.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct Progress {
    pub status: i32,
    pub state: u32,
    /// Tokens generated so far, how many of them match the pinned baseline, and how
    /// many the job set out to generate.
    pub done: u32,
    pub matched: u32,
    pub total: u32,
    /// Time to the first token, and time since the job started (milliseconds).
    pub ttft_ms: u32,
    pub elapsed_ms: u32,
    /// Pages the worker holds, and its quota.
    pub used_pages: u32,
    pub quota_pages: u32,
    /// Set when the last Stop ended the job between two compute steps, as asked;
    /// clear when the worker had to be killed.
    pub cooperative: u32,
    /// Milliseconds from Stop to the worker being gone (0 until a Stop).
    pub stop_ms: u32,
    pub served: u32,
    pub text_len: u32,
    pub _pad: u32,
    /// The tail of the generated text, printable ASCII (anything else as `.`).
    pub text: [u8; PROGRESS_TEXT_MAX],
}

impl Default for Progress {
    fn default() -> Self {
        Progress {
            status: 0,
            state: worker_state::IDLE,
            done: 0,
            matched: 0,
            total: 0,
            ttft_ms: 0,
            elapsed_ms: 0,
            used_pages: 0,
            quota_pages: 0,
            cooperative: 0,
            stop_ms: 0,
            served: 0,
            text_len: 0,
            _pad: 0,
            text: [0; PROGRESS_TEXT_MAX],
        }
    }
}

impl Progress {
    pub fn text(&self) -> &str {
        let n = (self.text_len as usize).min(PROGRESS_TEXT_MAX);
        core::str::from_utf8(&self.text[..n]).unwrap_or("")
    }
}

/// Between the shell and an inference worker, over the worker's bootstrap channel.
///
/// The shell announces the session before it hands over any capability
/// ([`worker::SESSION`], no handle), so the worker knows from its first message that
/// someone is watching. The worker then reports with [`JobReport`]s; the shell asks it
/// to end with [`worker::STOP`], and kills it if it has not ended within
/// [`worker::STOP_GRACE_MS`].
pub mod worker {
    /// First message from the shell: report progress here and accept Stop.
    pub const SESSION: &[u8] = b"session";
    /// Optional, after [`SESSION`] and before any capability: run on another model
    /// file (`infer <path>`). This prefix, then the path (A02).
    pub const MODEL: &[u8] = b"model=";
    /// The longest model path a session may name.
    pub const MODEL_PATH_MAX: usize = 64;
    /// From the shell: end between two compute steps, now.
    pub const STOP: &[u8] = b"stop";
    /// How long the shell waits for a worker asked to stop before it kills it.
    pub const STOP_GRACE_MS: u64 = 1000;
    /// Exit code of a worker that stopped because it was asked to.
    pub const EXIT_STOPPED: i32 = 3;

    /// [`super::JobReport::kind`] values.
    pub mod report {
        /// Another token is out.
        pub const PROGRESS: u32 = 1;
        /// All tokens are out and matched the baseline.
        pub const DONE: u32 = 2;
        /// Stopped between two steps on request.
        pub const STOPPED: u32 = 3;
        /// Gave up; `text` says why.
        pub const FAILED: u32 = 4;
    }
}

/// Bytes of text in a [`JobReport`].
pub const REPORT_TEXT_MAX: usize = 48;

/// One report from an inference worker to its shell.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct JobReport {
    pub kind: u32,
    pub done: u32,
    pub matched: u32,
    pub total: u32,
    pub ttft_ms: u32,
    pub elapsed_ms: u32,
    pub used_pages: u32,
    pub text_len: u32,
    /// The latest generated text (printable ASCII), or why the job failed.
    pub text: [u8; REPORT_TEXT_MAX],
}

impl Default for JobReport {
    fn default() -> Self {
        JobReport {
            kind: 0,
            done: 0,
            matched: 0,
            total: 0,
            ttft_ms: 0,
            elapsed_ms: 0,
            used_pages: 0,
            text_len: 0,
            text: [0; REPORT_TEXT_MAX],
        }
    }
}

impl JobReport {
    pub fn text(&self) -> &str {
        let n = (self.text_len as usize).min(REPORT_TEXT_MAX);
        core::str::from_utf8(&self.text[..n]).unwrap_or("")
    }

    /// Keep the last [`REPORT_TEXT_MAX`] bytes of `s`, which must be ASCII.
    pub fn set_text(&mut self, s: &str) {
        let b = s.as_bytes();
        let tail = &b[b.len().saturating_sub(REPORT_TEXT_MAX)..];
        self.text[..tail.len()].copy_from_slice(tail);
        self.text_len = tail.len() as u32;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn messages_have_no_implicit_padding() {
        assert_eq!(core::mem::size_of::<Progress>(), 14 * 4 + PROGRESS_TEXT_MAX);
        assert_eq!(core::mem::size_of::<JobReport>(), 8 * 4 + REPORT_TEXT_MAX);
    }

    #[test]
    fn report_text_keeps_the_tail() {
        let mut r = JobReport::default();
        let mut long = [b'a'; REPORT_TEXT_MAX + 3];
        long[REPORT_TEXT_MAX..].copy_from_slice(b"xyz");
        r.set_text(core::str::from_utf8(&long).unwrap());
        assert_eq!(r.text().len(), REPORT_TEXT_MAX);
        assert!(r.text().ends_with("xyz"));
        r.set_text("short");
        assert_eq!(r.text(), "short");
    }
}
