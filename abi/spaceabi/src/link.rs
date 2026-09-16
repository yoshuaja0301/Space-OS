//! SpaceLink protocol: index, revocation and context bundles (L01-L03).
//!
//! SpaceLink answers one question - "which parts of the corpus should a model be
//! shown for this request, and where did they come from" - and it has to answer it
//! in a way the caller can check. Every chunk it returns carries its source path,
//! byte range and SHA-256, so a caller can re-read the file and verify that the
//! text it was given is really what is on disk.
//!
//! Revocation is part of the contract, not a cleanup job: a revoked document must
//! not appear in any later result, including bundles, and must not come back when
//! the corpus is indexed again.

pub const ABI_VERSION: u32 = 0;

/// Longest path in a request or a result entry.
pub const PATH_MAX: usize = 48;
/// Longest query string.
pub const QUERY_MAX: usize = 64;
/// Bytes of chunk text carried by one reply.
pub const TEXT_MAX: usize = 128;
/// Documents the index holds.
pub const MAX_DOCS: usize = 16;
/// Chunks the index holds in total.
pub const MAX_CHUNKS: usize = 128;
/// Entries one bundle may contain.
pub const MAX_BUNDLE: usize = 8;

pub mod req {
    /// Negotiate the version; from the operator it carries the file capability.
    pub const HELLO: u32 = 0;
    /// Index every text file directly inside `path`.
    pub const INDEX: u32 = 1;
    /// Rank chunks for `query`; `offset` selects which result to return.
    pub const QUERY: u32 = 2;
    /// Revoke the document at `path`. Permanent for this service's lifetime.
    pub const REVOKE: u32 = 3;
    /// Build a context bundle for `query` under a byte budget in `budget`.
    pub const BUNDLE: u32 = 4;
    /// Read entry `offset` of the bundle built by the last `BUNDLE`.
    pub const BUNDLE_ENTRY: u32 = 5;
    /// Counts: documents, revoked documents, chunks.
    pub const STATS: u32 = 6;
    /// Close the service.
    pub const QUIT: u32 = 7;
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct LinkRequest {
    pub kind: u32,
    pub abi_version: u32,
    /// Result index for `QUERY` / `BUNDLE_ENTRY`.
    pub offset: u32,
    /// Byte budget for `BUNDLE`.
    pub budget: u32,
    pub path_len: u32,
    pub query_len: u32,
    pub path: [u8; PATH_MAX],
    pub query: [u8; QUERY_MAX],
}

impl Default for LinkRequest {
    fn default() -> Self {
        LinkRequest {
            kind: 0,
            abi_version: 0,
            offset: 0,
            budget: 0,
            path_len: 0,
            query_len: 0,
            path: [0; PATH_MAX],
            query: [0; QUERY_MAX],
        }
    }
}

impl LinkRequest {
    pub fn new(kind: u32) -> LinkRequest {
        LinkRequest { kind, ..Default::default() }
    }

    pub fn with_path(kind: u32, path: &str) -> LinkRequest {
        let mut r = LinkRequest::new(kind);
        let n = path.len().min(PATH_MAX);
        r.path[..n].copy_from_slice(&path.as_bytes()[..n]);
        r.path_len = n as u32;
        r
    }

    pub fn with_query(kind: u32, query: &str) -> LinkRequest {
        let mut r = LinkRequest::new(kind);
        let n = query.len().min(QUERY_MAX);
        r.query[..n].copy_from_slice(&query.as_bytes()[..n]);
        r.query_len = n as u32;
        r
    }

    pub fn path(&self) -> &str {
        let n = (self.path_len as usize).min(PATH_MAX);
        core::str::from_utf8(&self.path[..n]).unwrap_or("")
    }

    pub fn query(&self) -> &str {
        let n = (self.query_len as usize).min(QUERY_MAX);
        core::str::from_utf8(&self.query[..n]).unwrap_or("")
    }
}

/// One result: a chunk of a document, with everything needed to verify it.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct LinkReply {
    /// 0 or a negated [`crate::error::Error`].
    pub status: i32,
    /// Results available for this query (or entries in the bundle).
    pub total: u32,
    /// Score of this chunk; higher is more relevant.
    pub score: u32,
    /// Byte offset of the chunk inside its document.
    pub offset: u32,
    /// Length of the chunk in bytes.
    pub len: u32,
    pub path_len: u32,
    pub text_len: u32,
    /// Documents, revoked documents, chunks - filled by `STATS`; also carries the
    /// bundle's total byte count after `BUNDLE`.
    pub value: u32,
    pub value2: u32,
    pub value3: u32,
    /// SHA-256 of the chunk's bytes as they are on disk.
    pub digest: [u8; 32],
    pub path: [u8; PATH_MAX],
    pub text: [u8; TEXT_MAX],
}

impl Default for LinkReply {
    fn default() -> Self {
        LinkReply {
            status: 0,
            total: 0,
            score: 0,
            offset: 0,
            len: 0,
            path_len: 0,
            text_len: 0,
            value: 0,
            value2: 0,
            value3: 0,
            digest: [0; 32],
            path: [0; PATH_MAX],
            text: [0; TEXT_MAX],
        }
    }
}

impl LinkReply {
    pub fn set_path(&mut self, p: &str) {
        let n = p.len().min(PATH_MAX);
        self.path[..n].copy_from_slice(&p.as_bytes()[..n]);
        self.path_len = n as u32;
    }

    pub fn path(&self) -> &str {
        let n = (self.path_len as usize).min(PATH_MAX);
        core::str::from_utf8(&self.path[..n]).unwrap_or("")
    }

    pub fn set_text(&mut self, t: &[u8]) {
        let n = t.len().min(TEXT_MAX);
        self.text[..n].copy_from_slice(&t[..n]);
        self.text_len = n as u32;
    }

    pub fn text(&self) -> &[u8] {
        &self.text[..(self.text_len as usize).min(TEXT_MAX)]
    }

    pub fn result(&self) -> Result<u32, crate::error::Error> {
        if self.status == 0 {
            Ok(self.total)
        } else {
            Err(crate::error::Error::from_code((-self.status) as u32).unwrap_or(crate::error::Error::Invalid))
        }
    }
}
