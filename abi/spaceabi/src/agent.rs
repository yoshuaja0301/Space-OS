//! Tool Broker protocol (requirement G01).
//!
//! An agent gets one channel and nothing else: no root capability, so it cannot
//! open a file, spawn a process or read kernel state on its own. Every effect it
//! has on the world is a request on this channel, which the broker checks against a
//! declared workspace scope and records in an append-only audit log.
//!
//! The point of the design is that "the agent stayed in scope" is not a promise the
//! agent makes. It is a property of what it was handed.

pub const ABI_VERSION: u32 = 0;

/// Longest path in a tool request.
pub const PATH_MAX: usize = 48;
/// Bytes of file content carried by one request or reply.
pub const CHUNK_MAX: usize = 128;
/// Audit entries the broker keeps.
pub const AUDIT_MAX: usize = 64;

pub mod tool {
    /// Negotiate the version. From the operator it also carries the root capability
    /// the broker runs with.
    pub const HELLO: u32 = 0;
    /// Operator only: hand the broker the channel it serves the agent on.
    pub const ATTACH: u32 = 1;
    /// List the workspace.
    pub const LIST: u32 = 2;
    /// Read `len` bytes at `offset` of `path`.
    pub const READ: u32 = 3;
    /// Write `len` bytes at `offset` of `path` into the workspace overlay.
    pub const WRITE: u32 = 4;
    /// Run the named check over the workspace as the agent has left it.
    pub const CHECK: u32 = 5;
    /// Read audit entry number `offset`.
    pub const AUDIT: u32 = 6;
    /// The agent has finished; the broker stops serving it.
    pub const DONE: u32 = 7;
    /// Operator only: the broker replies and exits.
    pub const QUIT: u32 = 8;
}

/// Why a request was allowed or refused. Recorded for every request, so a refusal
/// is as visible in the audit as a success.
pub mod verdict {
    pub const ALLOWED: u32 = 0;
    /// The path is outside the workspace the broker was given.
    pub const DENIED_SCOPE: u32 = 1;
    /// The tool itself is not one the agent may use.
    pub const DENIED_TOOL: u32 = 2;
    /// In scope and allowed, but the operation failed (missing file, bad range).
    pub const FAILED: u32 = 3;
}

/// A request from the agent (or the operator) to the broker.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct ToolRequest {
    pub tool: u32,
    pub abi_version: u32,
    pub offset: u32,
    pub len: u32,
    pub path_len: u32,
    pub _pad: u32,
    pub path: [u8; PATH_MAX],
    pub data: [u8; CHUNK_MAX],
}

impl Default for ToolRequest {
    fn default() -> Self {
        ToolRequest {
            tool: 0,
            abi_version: 0,
            offset: 0,
            len: 0,
            path_len: 0,
            _pad: 0,
            path: [0; PATH_MAX],
            data: [0; CHUNK_MAX],
        }
    }
}

impl ToolRequest {
    pub fn new(tool: u32, path: &str) -> ToolRequest {
        let mut r = ToolRequest { tool, ..Default::default() };
        r.set_path(path);
        r
    }

    pub fn set_path(&mut self, path: &str) {
        let n = path.len().min(PATH_MAX);
        self.path[..n].copy_from_slice(&path.as_bytes()[..n]);
        self.path_len = n as u32;
    }

    pub fn path(&self) -> &str {
        let n = (self.path_len as usize).min(PATH_MAX);
        core::str::from_utf8(&self.path[..n]).unwrap_or("")
    }

    pub fn set_data(&mut self, data: &[u8]) {
        let n = data.len().min(CHUNK_MAX);
        self.data[..n].copy_from_slice(&data[..n]);
        self.len = n as u32;
    }

    pub fn data(&self) -> &[u8] {
        &self.data[..(self.len as usize).min(CHUNK_MAX)]
    }
}

/// The broker's answer.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct ToolReply {
    /// 0 or a negated [`crate::error::Error`].
    pub status: i32,
    pub verdict: u32,
    pub len: u32,
    /// Audit sequence number this request was recorded under.
    pub seq: u32,
    /// Check result, entry count, or file size depending on the tool.
    pub value: u64,
    pub data: [u8; CHUNK_MAX],
}

impl Default for ToolReply {
    fn default() -> Self {
        ToolReply { status: 0, verdict: verdict::ALLOWED, len: 0, seq: 0, value: 0, data: [0; CHUNK_MAX] }
    }
}

impl ToolReply {
    pub fn set_data(&mut self, data: &[u8]) {
        let n = data.len().min(CHUNK_MAX);
        self.data[..n].copy_from_slice(&data[..n]);
        self.len = n as u32;
    }

    pub fn data(&self) -> &[u8] {
        &self.data[..(self.len as usize).min(CHUNK_MAX)]
    }

    pub fn text(&self) -> &str {
        core::str::from_utf8(self.data()).unwrap_or("")
    }

    pub fn result(&self) -> Result<u64, crate::error::Error> {
        if self.status == 0 {
            Ok(self.value)
        } else {
            Err(crate::error::Error::from_code((-self.status) as u32).unwrap_or(crate::error::Error::Invalid))
        }
    }
}

/// Name of the check the broker knows how to run.
pub const CHECK_VERIFY: &str = "verify";
