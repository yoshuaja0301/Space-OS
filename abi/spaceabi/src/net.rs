//! Network service protocol (`spacenet`).
//!
//! The kernel moves Ethernet frames; everything above them -- ARP, IPv4, DHCP, DNS,
//! TCP -- is one user-space service that holds the device lease. Other programs
//! reach the network only through a *session* with that service, and every session
//! carries an allowlist of destinations (PRD §5: an agent's capability names the
//! network destinations it may use; default deny). A destination not on the list is
//! refused before a single packet leaves, and the refusal is counted.
//!
//! Three kinds of channel are involved:
//!
//! * the **operator channel** (the service's bootstrap handle): the process that
//!   started the service hands it the device lease, configures it, and creates
//!   sessions with their allowlists;
//! * a **session channel**: `HELLO`, `RESOLVE`, `CONNECT`, `STATS`;
//! * a **socket channel**, one per TCP connection, created by the client and passed
//!   in with `CONNECT`: data flows both ways as [`sock`] messages. Closing it closes
//!   the connection.
//!
//! Requests and replies are fixed-layout records ([`NetRequest`], [`NetReply`]), so
//! a malformed message is a wrong length, never a parse ambiguity.

pub const ABI_VERSION: u32 = 0;

/// Longest host name in a request or an allowlist entry.
pub const HOST_MAX: usize = 64;
/// Allowlist entries one session may hold.
pub const ALLOW_MAX: usize = 8;
/// Payload bytes carried by one socket data message: a channel message less the
/// one-byte tag.
pub const DATA_MAX: usize = crate::syscall::MSG_MAX - 1;
/// Sessions one service holds, operator's own excluded.
pub const SESSIONS_MAX: usize = 8;
/// Open TCP connections across all sessions.
pub const SOCKETS_MAX: usize = 8;
/// Open TCP connections one session may hold, so no session can take every slot.
pub const SOCKETS_PER_SESSION: usize = 4;

/// An allowlist entry matching any host, or any port.
pub const ANY_HOST: &str = "*";
pub const ANY_PORT: u16 = 0;

pub mod req {
    // ---- session channel -------------------------------------------------------
    /// Version check; the reply carries the interface configuration.
    pub const HELLO: u32 = 0;
    /// Resolve `host` to an IPv4 address (A record). Checked against the allowlist.
    pub const RESOLVE: u32 = 1;
    /// Open a TCP connection to `host` (or `addr` when `host` is empty) on `port`.
    /// The request must carry a channel endpoint: it becomes the socket channel.
    /// The reply comes at once (accepted or refused); the outcome of the handshake
    /// arrives on the socket channel as [`super::sock::CONNECTED`] or
    /// [`super::sock::ERROR`] within `timeout_ms`.
    pub const CONNECT: u32 = 2;
    /// Counters: connections opened, destinations refused by the allowlist, names
    /// resolved. On the operator channel: the same, summed over every session.
    pub const STATS: u32 = 3;

    // ---- operator channel ------------------------------------------------------
    /// First message from the operator: carries the device lease. The reply comes
    /// once the interface is configured (DHCP), or with `TimedOut`.
    pub const ATTACH: u32 = 16;
    /// Use `addr:port` as the DNS server instead of the one DHCP named.
    pub const SET_DNS: u32 = 17;
    /// Make the carried channel endpoint a new session with an empty allowlist.
    pub const SESSION: u32 = 18;
    /// Add `host:port` to the allowlist of the session created last.
    pub const ALLOW: u32 = 19;
    /// Close every socket and session and exit.
    pub const QUIT: u32 = 20;
}

/// A request on the operator or a session channel.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct NetRequest {
    pub kind: u32,
    pub abi_version: u32,
    /// TCP port for `CONNECT` / `ALLOW` / `SET_DNS`.
    pub port: u16,
    pub _pad: u16,
    /// Handshake deadline for `CONNECT`; lookup deadline for `RESOLVE`.
    pub timeout_ms: u32,
    /// IPv4 address when `host` is empty.
    pub addr: [u8; 4],
    pub host_len: u32,
    pub host: [u8; HOST_MAX],
}

impl Default for NetRequest {
    fn default() -> Self {
        NetRequest {
            kind: 0,
            abi_version: ABI_VERSION,
            port: 0,
            _pad: 0,
            timeout_ms: 0,
            addr: [0; 4],
            host_len: 0,
            host: [0; HOST_MAX],
        }
    }
}

impl NetRequest {
    pub fn new(kind: u32) -> NetRequest {
        NetRequest { kind, ..Default::default() }
    }

    /// A request naming `host`. Names longer than [`HOST_MAX`] are refused by the
    /// service rather than truncated here: a truncated name is a different name.
    pub fn with_host(kind: u32, host: &str, port: u16) -> NetRequest {
        let mut r = NetRequest::new(kind);
        let n = host.len().min(HOST_MAX);
        r.host[..n].copy_from_slice(&host.as_bytes()[..n]);
        r.host_len = if host.len() > HOST_MAX { u32::MAX } else { n as u32 };
        r.port = port;
        r
    }

    pub fn with_addr(kind: u32, addr: [u8; 4], port: u16) -> NetRequest {
        let mut r = NetRequest::new(kind);
        r.addr = addr;
        r.port = port;
        r
    }

    /// The host name, or `None` when it is absent, too long or not text.
    pub fn host(&self) -> Option<&str> {
        let n = self.host_len as usize;
        if n == 0 || n > HOST_MAX {
            return None;
        }
        core::str::from_utf8(&self.host[..n]).ok()
    }

    pub fn as_bytes(&self) -> &[u8] {
        // SAFETY: plain `repr(C)` data with no padding bytes left uninitialised by
        // the constructors (every field is written).
        unsafe { core::slice::from_raw_parts(self as *const Self as *const u8, core::mem::size_of::<Self>()) }
    }

    pub fn from_bytes(b: &[u8]) -> Option<NetRequest> {
        if b.len() != core::mem::size_of::<NetRequest>() {
            return None;
        }
        // SAFETY: exact size; every bit pattern is a valid NetRequest.
        Some(unsafe { core::ptr::read_unaligned(b.as_ptr() as *const NetRequest) })
    }
}

/// A reply on the operator or a session channel.
#[repr(C)]
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub struct NetReply {
    /// 0 or a negated [`crate::error::Error`].
    pub status: i32,
    /// Prefix length of `addr`'s network (HELLO / ATTACH).
    pub prefix: u8,
    pub _pad: [u8; 3],
    /// Our address (HELLO / ATTACH) or the resolved one (RESOLVE).
    pub addr: [u8; 4],
    pub gateway: [u8; 4],
    pub dns: [u8; 4],
    pub mac: [u8; 6],
    /// Brings `value` to its natural 8-byte alignment, so no byte of the record is
    /// implicit padding.
    pub _pad2: [u8; 6],
    /// STATS: connections opened, destinations refused, names resolved. SESSION /
    /// CONNECT: the identifier of the new session / connection.
    pub value: u64,
    pub value2: u64,
    pub value3: u64,
}

impl NetReply {
    pub fn error(e: crate::error::Error) -> NetReply {
        NetReply { status: -(e as i32), ..Default::default() }
    }

    pub fn result(&self) -> Result<(), crate::error::Error> {
        if self.status == 0 {
            Ok(())
        } else {
            Err(crate::error::Error::from_code((-self.status) as u32).unwrap_or(crate::error::Error::Invalid))
        }
    }

    pub fn as_bytes(&self) -> &[u8] {
        // SAFETY: plain `repr(C)` data; all padding fields are explicit.
        unsafe { core::slice::from_raw_parts(self as *const Self as *const u8, core::mem::size_of::<Self>()) }
    }

    pub fn from_bytes(b: &[u8]) -> Option<NetReply> {
        if b.len() != core::mem::size_of::<NetReply>() {
            return None;
        }
        // SAFETY: exact size; every bit pattern is a valid NetReply.
        Some(unsafe { core::ptr::read_unaligned(b.as_ptr() as *const NetReply) })
    }
}

/// Socket channel messages. The first byte is the tag; the rest depends on it.
pub mod sock {
    // ---- client -> service ------------------------------------------------------
    /// Bytes to send, up to [`super::DATA_MAX`].
    pub const DATA: u8 = 1;
    /// No more data from this side (TCP FIN once everything queued has gone).
    pub const SHUTDOWN: u8 = 2;

    // ---- service -> client ------------------------------------------------------
    /// The handshake completed.
    pub const CONNECTED: u8 = 0x81;
    /// Bytes received, up to [`super::DATA_MAX`].
    pub const RECEIVED: u8 = 0x82;
    /// The peer finished sending and everything it sent has been delivered.
    pub const EOF: u8 = 0x83;
    /// The connection failed or broke; four little-endian bytes of negated
    /// [`crate::error::Error`] follow.
    pub const ERROR: u8 = 0x84;
}

/// Is `host:port` allowed by `entry_host:entry_port`? Host names compare without
/// regard to ASCII case; `*` matches any host and port 0 any port. A name never
/// matches an address and an address never matches a name: allowing a name is
/// not allowing whatever that name resolves to today.
pub fn allow_matches(entry_host: &str, entry_port: u16, host: &str, port: u16) -> bool {
    (entry_port == ANY_PORT || entry_port == port)
        && (entry_host == ANY_HOST || entry_host.eq_ignore_ascii_case(host))
}

/// Dotted-quad text for an IPv4 address, into `buf`; returns the used part.
pub fn fmt_ipv4(a: [u8; 4], buf: &mut [u8; 15]) -> &str {
    let mut n = 0;
    for (i, octet) in a.iter().enumerate() {
        if i > 0 {
            buf[n] = b'.';
            n += 1;
        }
        let mut v = *octet;
        let mut digits = [0u8; 3];
        let mut d = 0;
        loop {
            digits[d] = b'0' + v % 10;
            d += 1;
            v /= 10;
            if v == 0 {
                break;
            }
        }
        while d > 0 {
            d -= 1;
            buf[n] = digits[d];
            n += 1;
        }
    }
    core::str::from_utf8(&buf[..n]).unwrap_or("")
}

/// Parse dotted-quad text; `None` for anything else (including names).
pub fn parse_ipv4(s: &str) -> Option<[u8; 4]> {
    let mut out = [0u8; 4];
    let mut parts = s.split('.');
    for o in out.iter_mut() {
        let p = parts.next()?;
        if p.is_empty() || p.len() > 3 || !p.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        let v: u32 = p.parse().ok()?;
        if v > 255 {
            return None;
        }
        *o = v as u8;
    }
    if parts.next().is_some() {
        return None;
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ipv4_text_round_trips() {
        let mut b = [0u8; 15];
        assert_eq!(fmt_ipv4([10, 0, 2, 15], &mut b), "10.0.2.15");
        assert_eq!(fmt_ipv4([255, 255, 0, 1], &mut b), "255.255.0.1");
        assert_eq!(parse_ipv4("10.0.2.15"), Some([10, 0, 2, 15]));
        assert_eq!(parse_ipv4("10.0.2"), None);
        assert_eq!(parse_ipv4("10.0.2.256"), None);
        assert_eq!(parse_ipv4("10.0.2.1.1"), None);
        assert_eq!(parse_ipv4("api.cloud.test"), None);
    }

    #[test]
    fn records_have_no_implicit_padding() {
        // Every byte is a declared field: `as_bytes` never reads padding.
        assert_eq!(core::mem::size_of::<NetRequest>(), 4 + 4 + 2 + 2 + 4 + 4 + 4 + HOST_MAX);
        assert_eq!(core::mem::size_of::<NetReply>(), 4 + 1 + 3 + 4 + 4 + 4 + 6 + 6 + 8 * 3);
        let r = NetRequest::with_host(req::CONNECT, "echo.lab.test", 7);
        let back = NetRequest::from_bytes(r.as_bytes()).unwrap();
        assert_eq!((back.kind, back.port, back.host()), (req::CONNECT, 7, Some("echo.lab.test")));
        let long =
            NetRequest::with_host(req::RESOLVE, core::str::from_utf8(&[b'a'; HOST_MAX + 1]).unwrap(), 0);
        assert_eq!(long.host(), None);
        let e = NetReply::error(crate::error::Error::Denied);
        assert_eq!(NetReply::from_bytes(e.as_bytes()).unwrap().result(), Err(crate::error::Error::Denied));
    }

    #[test]
    fn allowlist_matching() {
        assert!(allow_matches("api.cloud.test", 80, "API.Cloud.Test", 80));
        assert!(!allow_matches("api.cloud.test", 80, "api.cloud.test", 81));
        assert!(allow_matches("api.cloud.test", ANY_PORT, "api.cloud.test", 81));
        assert!(allow_matches(ANY_HOST, 7, "anything", 7));
        assert!(!allow_matches("api.cloud.test", 80, "10.0.2.100", 80));
    }
}
