//! Space OS package format (requirement P01).
//!
//! A package is a fixed-size header followed by its payload. The header names the
//! package, its version and the digest of the payload; the MAC covers everything in
//! the header up to itself, so the name, the version and the payload digest are all
//! authenticated together. Changing any of them - or the payload - invalidates the
//! package, and the verifier says which check failed rather than a bare "bad
//! package".
//!
//! Authentication is HMAC-SHA256 under a release key, not a public-key signature.
//! See ADR-0014 for why, and for what that does and does not buy.

use crate::hmac::{hmac_sha256, verify};
use crate::sha256;

pub const MAGIC: [u8; 8] = *b"SPACEPKG";
/// Format version of the header itself.
pub const FORMAT: u32 = 0;
/// Longest package name.
pub const NAME_MAX: usize = 16;
/// Largest payload this build accepts.
pub const PAYLOAD_MAX: u64 = 64 * 1024;

/// The release key of this build.
///
/// It sits in the source because the whole toolchain here is one repository: the
/// host signs with it and the guest verifies with it. A real release would keep the
/// signing half out of the image entirely - which is exactly what a public-key
/// scheme buys and this one does not.
pub const RELEASE_KEY: &[u8] = b"space-os-dev-release-key-v0";

/// Bytes of the header the MAC covers: everything before the MAC field.
pub const SIGNED_BYTES: usize = 8 + 4 + 4 + NAME_MAX + 8 + 32;

#[repr(C)]
#[derive(Clone, Copy)]
pub struct Header {
    pub magic: [u8; 8],
    pub format: u32,
    /// Package version. Higher is newer; installing an older one is a downgrade.
    pub version: u32,
    pub name: [u8; NAME_MAX],
    pub payload_len: u64,
    /// SHA-256 of the payload.
    pub payload_digest: [u8; 32],
    /// HMAC-SHA256 over the first [`SIGNED_BYTES`] bytes of this header.
    pub mac: [u8; 32],
}

impl Default for Header {
    fn default() -> Self {
        Header {
            magic: MAGIC,
            format: FORMAT,
            version: 0,
            name: [0; NAME_MAX],
            payload_len: 0,
            payload_digest: [0; 32],
            mac: [0; 32],
        }
    }
}

/// Why a package was refused. Reported to the caller so an operator can tell a
/// corrupted download from a forged one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reject {
    /// Not a Space OS package, or a format this build does not know.
    Format,
    /// Header is shorter than one header, or the payload length disagrees with the
    /// file.
    Truncated,
    /// The payload does not hash to what the header claims.
    Payload,
    /// The MAC does not match: the header was altered, or it was made with another key.
    Mac,
    /// The package's own fields are not usable (empty name, oversized payload).
    Fields,
}

impl Reject {
    pub const fn message(self) -> &'static str {
        match self {
            Reject::Format => "not a Space OS package",
            Reject::Truncated => "package is truncated",
            Reject::Payload => "payload does not match its digest",
            Reject::Mac => "authentication failed",
            Reject::Fields => "package fields are not usable",
        }
    }
}

impl Header {
    pub fn name(&self) -> &str {
        let end = self.name.iter().position(|b| *b == 0).unwrap_or(NAME_MAX);
        core::str::from_utf8(&self.name[..end]).unwrap_or("")
    }

    pub fn set_name(&mut self, name: &str) {
        self.name = [0; NAME_MAX];
        let n = name.len().min(NAME_MAX);
        self.name[..n].copy_from_slice(&name.as_bytes()[..n]);
    }

    /// The MAC this header should carry under `key`.
    pub fn expected_mac(&self, key: &[u8]) -> [u8; 32] {
        // SAFETY: `Header` is `repr(C)` plain data; only the bytes before `mac` are
        // read, which is exactly the range the MAC is defined over.
        let bytes = unsafe {
            core::slice::from_raw_parts(self as *const Header as *const u8, core::mem::size_of::<Header>())
        };
        hmac_sha256(key, &bytes[..SIGNED_BYTES])
    }

    pub fn sign(&mut self, key: &[u8]) {
        self.mac = self.expected_mac(key);
    }
}

/// Parse and authenticate a package image.
///
/// Returns the header and the payload on success. The order of the checks is part of
/// the contract: shape first, then authentication, then content - so a forged
/// package is never hashed as if it were trusted, and a truncated one is never
/// reported as a MAC failure.
pub fn open<'a>(image: &'a [u8], key: &[u8]) -> Result<(Header, &'a [u8]), Reject> {
    let size = core::mem::size_of::<Header>();
    if image.len() < size {
        return Err(Reject::Truncated);
    }
    // SAFETY: `image` holds at least `size` bytes and `Header` is plain `repr(C)`
    // data with no invalid bit patterns; read unaligned because `image` is a slice.
    let header: Header = unsafe { core::ptr::read_unaligned(image.as_ptr() as *const Header) };
    if header.magic != MAGIC || header.format != FORMAT {
        return Err(Reject::Format);
    }
    if header.name().is_empty() || header.payload_len > PAYLOAD_MAX {
        return Err(Reject::Fields);
    }
    let end = size.checked_add(header.payload_len as usize).ok_or(Reject::Fields)?;
    if image.len() < end {
        return Err(Reject::Truncated);
    }
    if !verify(&header.expected_mac(key), &header.mac) {
        return Err(Reject::Mac);
    }
    let payload = &image[size..end];
    if sha256::digest(payload) != header.payload_digest {
        return Err(Reject::Payload);
    }
    Ok((header, payload))
}

/// Build a package image into `out` (header followed by payload).
pub fn build(name: &str, version: u32, payload: &[u8], key: &[u8], out: &mut [u8]) -> Option<usize> {
    let size = core::mem::size_of::<Header>();
    if out.len() < size + payload.len() || payload.len() as u64 > PAYLOAD_MAX {
        return None;
    }
    let mut header = Header { version, payload_len: payload.len() as u64, ..Default::default() };
    header.set_name(name);
    header.payload_digest = sha256::digest(payload);
    header.sign(key);
    // SAFETY: `Header` is `repr(C)` plain data.
    let bytes = unsafe {
        core::slice::from_raw_parts(&header as *const Header as *const u8, core::mem::size_of::<Header>())
    };
    out[..size].copy_from_slice(bytes);
    out[size..size + payload.len()].copy_from_slice(payload);
    Some(size + payload.len())
}

// The MAC is defined as "the header up to the MAC", so the constant and the layout
// must not drift apart. If a field is ever added, this stops the build rather than
// silently signing a different range.
const _: () = assert!(SIGNED_BYTES == core::mem::offset_of!(Header, mac));
const _: () = assert!(core::mem::size_of::<Header>() == SIGNED_BYTES + 32);

/// Control protocol of the package service.
pub mod req {
    /// Negotiate the version; from the operator it carries the file capability.
    pub const HELLO: u32 = 0;
    /// Verify and install the package at `path`.
    pub const INSTALL: u32 = 1;
    /// Verify the package at `path` without installing it.
    pub const VERIFY: u32 = 2;
    /// Go back to the previously installed version.
    pub const ROLLBACK: u32 = 3;
    /// Active package: name, version, previous version, payload digest.
    pub const STATUS: u32 = 4;
    /// Read the active payload at `offset`.
    pub const READ: u32 = 5;
    /// Close the service.
    pub const QUIT: u32 = 6;
}

/// Longest path in a package request.
pub const PATH_MAX: usize = 48;
/// Payload bytes carried by one reply.
pub const CHUNK_MAX: usize = 128;

#[repr(C)]
#[derive(Clone, Copy)]
pub struct PkgRequest {
    pub kind: u32,
    pub abi_version: u32,
    pub offset: u32,
    pub path_len: u32,
    pub path: [u8; PATH_MAX],
}

impl Default for PkgRequest {
    fn default() -> Self {
        PkgRequest { kind: 0, abi_version: 0, offset: 0, path_len: 0, path: [0; PATH_MAX] }
    }
}

impl PkgRequest {
    pub fn new(kind: u32) -> PkgRequest {
        PkgRequest { kind, ..Default::default() }
    }

    pub fn with_path(kind: u32, path: &str) -> PkgRequest {
        let mut r = PkgRequest::new(kind);
        let n = path.len().min(PATH_MAX);
        r.path[..n].copy_from_slice(&path.as_bytes()[..n]);
        r.path_len = n as u32;
        r
    }

    pub fn path(&self) -> &str {
        let n = (self.path_len as usize).min(PATH_MAX);
        core::str::from_utf8(&self.path[..n]).unwrap_or("")
    }
}

/// `reject` codes carried in a reply. `NONE` means the refusal (if any) was not a
/// package-format refusal.
pub mod reject_code {
    pub const NONE: i32 = -1;
    pub const FORMAT: i32 = 0;
    pub const TRUNCATED: i32 = 1;
    pub const PAYLOAD: i32 = 2;
    pub const MAC: i32 = 3;
    pub const FIELDS: i32 = 4;
}

impl Reject {
    pub const fn code(self) -> i32 {
        match self {
            Reject::Format => reject_code::FORMAT,
            Reject::Truncated => reject_code::TRUNCATED,
            Reject::Payload => reject_code::PAYLOAD,
            Reject::Mac => reject_code::MAC,
            Reject::Fields => reject_code::FIELDS,
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct PkgReply {
    /// 0 or a negated [`crate::error::Error`].
    pub status: i32,
    /// Which package check failed, or [`reject_code::NONE`].
    pub reject: i32,
    /// Version now active (0 when nothing is installed).
    pub version: u32,
    /// Version a rollback would return to (0 when there is none).
    pub previous: u32,
    /// Payload length of the active package, or the bytes in `data`.
    pub len: u32,
    /// Versions the service is holding.
    pub history: u32,
    pub digest: [u8; 32],
    pub name: [u8; NAME_MAX],
    pub data: [u8; CHUNK_MAX],
}

impl Default for PkgReply {
    fn default() -> Self {
        PkgReply {
            status: 0,
            reject: reject_code::NONE,
            version: 0,
            previous: 0,
            len: 0,
            history: 0,
            digest: [0; 32],
            name: [0; NAME_MAX],
            data: [0; CHUNK_MAX],
        }
    }
}

impl PkgReply {
    pub fn name(&self) -> &str {
        let end = self.name.iter().position(|b| *b == 0).unwrap_or(NAME_MAX);
        core::str::from_utf8(&self.name[..end]).unwrap_or("")
    }

    pub fn set_name(&mut self, name: &str) {
        self.name = [0; NAME_MAX];
        let n = name.len().min(NAME_MAX);
        self.name[..n].copy_from_slice(&name.as_bytes()[..n]);
    }

    pub fn data(&self) -> &[u8] {
        &self.data[..(self.len as usize).min(CHUNK_MAX)]
    }

    pub fn result(&self) -> Result<u32, crate::error::Error> {
        if self.status == 0 {
            Ok(self.version)
        } else {
            Err(crate::error::Error::from_code((-self.status) as u32).unwrap_or(crate::error::Error::Invalid))
        }
    }
}
