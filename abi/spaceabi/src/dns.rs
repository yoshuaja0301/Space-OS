//! DNS messages for the network service's resolver: one A query out, the answer
//! in. Only what a stub resolver needs (RFC 1035 §4), allocation free.
//!
//! The resolver speaks DNS over TCP (RFC 1035 §4.2.2, RFC 7766), where every
//! message is preceded by its length as two big-endian bytes; [`build_query`]
//! writes that prefix too. An answer is accepted only when it carries our
//! identifier and repeats our question, and an address only when it belongs to the
//! name asked for or to the end of the CNAME chain starting there.

/// Longest name on the wire (RFC 1035 §2.3.4), dots included.
pub const NAME_MAX: usize = 253;
const LABEL_MAX: usize = 63;
/// Pointer hops followed while decompressing one name.
const JUMPS_MAX: usize = 16;
/// Answer records examined; CNAME chains longer than this are not followed.
const ANSWERS_MAX: usize = 32;

const TYPE_A: u16 = 1;
const TYPE_CNAME: u16 = 5;
const CLASS_IN: u16 = 1;
const FLAG_QR: u16 = 0x8000;
const FLAG_TC: u16 = 0x0200;
const FLAG_RD: u16 = 0x0100;
const RCODE_NXDOMAIN: u16 = 3;

/// Why an answer was not an address.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DnsError {
    /// The name does not exist (NXDOMAIN), or has no A record.
    NotFound,
    /// The server answered with an error code other than NXDOMAIN.
    ServerFailure(u8),
    /// Not an answer to our question: wrong identifier, question or opcode.
    Mismatch,
    /// Truncated, out of bounds, a pointer loop, an oversized name...
    Malformed,
}

/// Is `name` something we are willing to put in a query: 1 to [`NAME_MAX`] bytes of
/// dot-separated labels, each 1 to 63 letters, digits, `-` or `_`, not starting or
/// ending with `-`. No trailing dot.
pub fn valid_name(name: &str) -> bool {
    if name.is_empty() || name.len() > NAME_MAX {
        return false;
    }
    name.split('.').all(|label| {
        let b = label.as_bytes();
        !b.is_empty()
            && b.len() <= LABEL_MAX
            && b.iter().all(|&c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_')
            && b[0] != b'-'
            && b[b.len() - 1] != b'-'
    })
}

/// Write the query for `name`'s A record into `out`, preceded by the two-byte TCP
/// length. Returns the total length, or `None` if `name` is not valid or `out` is
/// too small.
pub fn build_query(id: u16, name: &str, out: &mut [u8]) -> Option<usize> {
    if !valid_name(name) {
        return None;
    }
    let msg_len = 12 + name.len() + 2 + 4;
    let total = 2 + msg_len;
    if out.len() < total {
        return None;
    }
    let o = &mut out[..total];
    o[0..2].copy_from_slice(&(msg_len as u16).to_be_bytes());
    o[2..4].copy_from_slice(&id.to_be_bytes());
    o[4..6].copy_from_slice(&FLAG_RD.to_be_bytes());
    o[6..8].copy_from_slice(&1u16.to_be_bytes()); // QDCOUNT
    o[8..14].fill(0); // ANCOUNT, NSCOUNT, ARCOUNT
    let mut at = 14;
    for label in name.split('.') {
        o[at] = label.len() as u8;
        o[at + 1..at + 1 + label.len()].copy_from_slice(label.as_bytes());
        at += 1 + label.len();
    }
    o[at] = 0;
    at += 1;
    o[at..at + 2].copy_from_slice(&TYPE_A.to_be_bytes());
    o[at + 2..at + 4].copy_from_slice(&CLASS_IN.to_be_bytes());
    debug_assert_eq!(at + 4, total);
    Some(total)
}

fn be16(m: &[u8], at: usize) -> Result<u16, DnsError> {
    match m.get(at..at + 2) {
        Some(b) => Ok(u16::from_be_bytes([b[0], b[1]])),
        None => Err(DnsError::Malformed),
    }
}

/// A name read from a message, decompressed.
struct Name {
    buf: [u8; NAME_MAX],
    len: usize,
}

impl Name {
    fn eq_ignore_case(&self, other: &[u8]) -> bool {
        self.buf[..self.len].eq_ignore_ascii_case(other)
    }
}

/// Read the name at `pos`, following compression pointers. Returns the name and
/// the position just after it in the record it started in.
fn read_name(m: &[u8], mut pos: usize) -> Result<(Name, usize), DnsError> {
    let mut name = Name { buf: [0; NAME_MAX], len: 0 };
    let mut next = None;
    let mut jumps = 0;
    loop {
        let len = *m.get(pos).ok_or(DnsError::Malformed)? as usize;
        match len & 0xC0 {
            0x00 if len == 0 => {
                return Ok((name, next.unwrap_or(pos + 1)));
            }
            0x00 => {
                let label = m.get(pos + 1..pos + 1 + len).ok_or(DnsError::Malformed)?;
                let sep = usize::from(name.len > 0);
                if name.len + sep + len > NAME_MAX {
                    return Err(DnsError::Malformed);
                }
                if sep == 1 {
                    name.buf[name.len] = b'.';
                }
                name.buf[name.len + sep..name.len + sep + len].copy_from_slice(label);
                name.len += sep + len;
                pos += 1 + len;
            }
            0xC0 => {
                let target = ((len & 0x3F) << 8) | *m.get(pos + 1).ok_or(DnsError::Malformed)? as usize;
                // Only backwards: a pointer to itself or ahead is how loops are built.
                if target >= pos || jumps == JUMPS_MAX {
                    return Err(DnsError::Malformed);
                }
                jumps += 1;
                next.get_or_insert(pos + 2);
                pos = target;
            }
            _ => return Err(DnsError::Malformed), // 0x40 / 0x80: reserved label types
        }
    }
}

/// The address `name` resolves to according to `msg` (a DNS message, without the
/// TCP length prefix), which must answer query `id` for `name`.
pub fn parse_answer(id: u16, name: &str, msg: &[u8]) -> Result<[u8; 4], DnsError> {
    if msg.len() < 12 {
        return Err(DnsError::Malformed);
    }
    let flags = be16(msg, 2)?;
    if be16(msg, 0)? != id || flags & FLAG_QR == 0 || (flags >> 11) & 0xF != 0 {
        return Err(DnsError::Mismatch);
    }
    if flags & FLAG_TC != 0 {
        return Err(DnsError::Malformed);
    }
    let qdcount = be16(msg, 4)?;
    let ancount = be16(msg, 6)?;
    if qdcount != 1 {
        return Err(DnsError::Mismatch);
    }
    let (qname, mut pos) = read_name(msg, 12)?;
    if !qname.eq_ignore_case(name.as_bytes()) || be16(msg, pos)? != TYPE_A || be16(msg, pos + 2)? != CLASS_IN
    {
        return Err(DnsError::Mismatch);
    }
    pos += 4;
    match (flags & 0xF) as u8 {
        0 => {}
        r if r as u16 == RCODE_NXDOMAIN => return Err(DnsError::NotFound),
        r => return Err(DnsError::ServerFailure(r)),
    }
    // The name whose address we want: the one asked for, then each CNAME target.
    let mut want = Name { buf: [0; NAME_MAX], len: name.len() };
    want.buf[..name.len()].copy_from_slice(name.as_bytes());
    for _ in 0..(ancount as usize).min(ANSWERS_MAX) {
        let (owner, after) = read_name(msg, pos)?;
        let rtype = be16(msg, after)?;
        let class = be16(msg, after + 2)?;
        let rdlen = be16(msg, after + 8)? as usize;
        let rdata = after + 10;
        if msg.len() < rdata + rdlen {
            return Err(DnsError::Malformed);
        }
        if class == CLASS_IN && owner.eq_ignore_case(&want.buf[..want.len]) {
            if rtype == TYPE_A && rdlen == 4 {
                return Ok([msg[rdata], msg[rdata + 1], msg[rdata + 2], msg[rdata + 3]]);
            }
            if rtype == TYPE_CNAME {
                let (target, end) = read_name(msg, rdata)?;
                if end > rdata + rdlen {
                    return Err(DnsError::Malformed);
                }
                want = target;
            }
        }
        pos = rdata + rdlen;
    }
    Err(DnsError::NotFound)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A response to `query` (with its TCP prefix) carrying `answers` verbatim.
    fn response(query: &[u8], rcode: u16, ancount: u16, answers: &[u8]) -> ([u8; 512], usize) {
        let q = &query[2..];
        let mut out = [0u8; 512];
        out[..q.len()].copy_from_slice(q);
        out[2..4].copy_from_slice(&(FLAG_QR | FLAG_RD | 0x0080 | rcode).to_be_bytes());
        out[6..8].copy_from_slice(&ancount.to_be_bytes());
        out[q.len()..q.len() + answers.len()].copy_from_slice(answers);
        (out, q.len() + answers.len())
    }

    fn a_record(owner: &[u8], addr: [u8; 4]) -> ([u8; 64], usize) {
        let mut r = [0u8; 64];
        r[..owner.len()].copy_from_slice(owner);
        let mut at = owner.len();
        r[at..at + 10].copy_from_slice(&[0, 1, 0, 1, 0, 0, 0, 60, 0, 4]);
        at += 10;
        r[at..at + 4].copy_from_slice(&addr);
        (r, at + 4)
    }

    #[test]
    fn names() {
        assert!(valid_name("api.cloud.test"));
        assert!(valid_name("a"));
        assert!(valid_name("_srv.x-y.test"));
        assert!(!valid_name(""));
        assert!(!valid_name("a..b"));
        assert!(!valid_name("a.b."));
        assert!(!valid_name("-a.test"));
        assert!(!valid_name("a-.test"));
        assert!(!valid_name("sp ace.test"));
        let long = [b'a'; 64];
        assert!(!valid_name(core::str::from_utf8(&long).unwrap()));
    }

    #[test]
    fn query_layout() {
        let mut q = [0u8; 128];
        let n = build_query(0x1234, "echo.lab.test", &mut q).unwrap();
        assert_eq!(n, 2 + 12 + 15 + 4);
        assert_eq!(&q[..2], &((n - 2) as u16).to_be_bytes());
        assert_eq!(&q[2..8], &[0x12, 0x34, 0x01, 0x00, 0, 1]);
        assert_eq!(&q[14..29], b"\x04echo\x03lab\x04test\x00");
        assert_eq!(&q[29..33], &[0, 1, 0, 1]);
        assert_eq!(build_query(1, "bad..name", &mut q), None);
        assert_eq!(build_query(1, "echo.lab.test", &mut q[..20]), None);
    }

    #[test]
    fn answers() {
        let mut q = [0u8; 128];
        let n = build_query(7, "echo.lab.test", &mut q).unwrap();
        // Owner as a pointer to the question name (offset 12).
        let (rec, rn) = a_record(&[0xC0, 12], [10, 0, 2, 101]);
        let (r, len) = response(&q[..n], 0, 1, &rec[..rn]);
        assert_eq!(parse_answer(7, "echo.lab.test", &r[..len]), Ok([10, 0, 2, 101]));
        assert_eq!(parse_answer(7, "ECHO.lab.TEST", &r[..len]), Ok([10, 0, 2, 101]));
        assert_eq!(parse_answer(8, "echo.lab.test", &r[..len]), Err(DnsError::Mismatch));
        assert_eq!(parse_answer(7, "other.lab.test", &r[..len]), Err(DnsError::Mismatch));
        assert_eq!(parse_answer(7, "echo.lab.test", &r[..len - 1]), Err(DnsError::Malformed));
        let (r, len) = response(&q[..n], 3, 0, &[]);
        assert_eq!(parse_answer(7, "echo.lab.test", &r[..len]), Err(DnsError::NotFound));
        let (r, len) = response(&q[..n], 2, 0, &[]);
        assert_eq!(parse_answer(7, "echo.lab.test", &r[..len]), Err(DnsError::ServerFailure(2)));
        // An address for some other name is not an answer.
        let (rec, rn) = a_record(b"\x05other\x04test\x00", [1, 2, 3, 4]);
        let (r, len) = response(&q[..n], 0, 1, &rec[..rn]);
        assert_eq!(parse_answer(7, "echo.lab.test", &r[..len]), Err(DnsError::NotFound));
    }

    #[test]
    fn cname_chain() {
        let mut q = [0u8; 128];
        let n = build_query(9, "www.lab.test", &mut q).unwrap();
        let qlen = n - 2;
        let mut ans = [0u8; 128];
        // www.lab.test CNAME real.lab.test (target: "real" + pointer to "lab.test" at 16).
        let cname: &[u8] = &[0xC0, 12, 0, 5, 0, 1, 0, 0, 0, 60, 0, 7, 4, b'r', b'e', b'a', b'l', 0xC0, 16];
        ans[..cname.len()].copy_from_slice(cname);
        // real.lab.test A 10.0.2.9, owner pointing at the CNAME target.
        let target_at = (qlen + 12) as u8;
        let (rec, rn) = a_record(&[0xC0, target_at], [10, 0, 2, 9]);
        ans[cname.len()..cname.len() + rn].copy_from_slice(&rec[..rn]);
        let (r, len) = response(&q[..n], 0, 2, &ans[..cname.len() + rn]);
        assert_eq!(parse_answer(9, "www.lab.test", &r[..len]), Ok([10, 0, 2, 9]));
    }

    #[test]
    fn hostile_pointers() {
        let mut q = [0u8; 128];
        let n = build_query(3, "a.test", &mut q).unwrap();
        let qlen = n - 2;
        // An owner name pointing at itself.
        let here = qlen as u8;
        let (r, len) = response(&q[..n], 0, 1, &[0xC0, here, 0, 1, 0, 1, 0, 0, 0, 1, 0, 4, 1, 2, 3, 4]);
        assert_eq!(parse_answer(3, "a.test", &r[..len]), Err(DnsError::Malformed));
        // A pointer past the end.
        let (r, len) = response(&q[..n], 0, 1, &[0xC0, 0xFF]);
        assert_eq!(parse_answer(3, "a.test", &r[..len]), Err(DnsError::Malformed));
        // A reserved label type.
        let (r, len) = response(&q[..n], 0, 1, &[0x40, 0]);
        assert_eq!(parse_answer(3, "a.test", &r[..len]), Err(DnsError::Malformed));
        // A record whose data runs past the message.
        let (r, len) = response(&q[..n], 0, 1, &[0xC0, 12, 0, 1, 0, 1, 0, 0, 0, 1, 0, 40, 1, 2]);
        assert_eq!(parse_answer(3, "a.test", &r[..len]), Err(DnsError::Malformed));
    }
}
