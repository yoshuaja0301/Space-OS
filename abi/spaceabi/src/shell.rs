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
}

/// Job names `cmd::RUN` accepts. They exist so a test can put the worker into each
/// state a real inference job can reach.
pub mod job {
    /// Finishes quickly and exits 0.
    pub const OK: &str = "ok";
    /// Dereferences NULL: killed by the kernel, the shell must survive it.
    pub const CRASH: &str = "crash";
    /// Spins without ever entering the kernel: only preemption keeps the shell alive.
    pub const HANG: &str = "hang";
    /// Sleeps for far longer than any test waits: stopped while blocked.
    pub const SLOW: &str = "slow";
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
