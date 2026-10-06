//! The lab network's services.
//!
//! The guest's network is QEMU user-mode networking with `restrict=on`: it reaches
//! nothing on the host and nothing beyond it. The services below are the only
//! exceptions, each an explicit forwarding rule (`guestfwd=...-cmd:`): for every
//! connection the guest opens to one of the addresses in [`SERVICES`], QEMU starts
//! `xtask lab <name>` with the connection as its standard input and output. So each
//! service is a few lines of blocking I/O, and nothing listens on a host port.
//!
//! The DNS server is written independently of the guest's resolver
//! (`spaceabi::dns`): the two agreeing proves more than one codec agreeing with
//! itself.

use std::io::{self, Read, Write};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use rustls::CipherSuite;

use super::pki;

/// Address, port and service name of every lab service.
pub const SERVICES: &[(&str, u16, &str)] = &[
    // DNS over TCP for the lab zone below.
    ("10.0.2.53", 53, "dns"),
    // Echoes every byte back until the guest finishes sending, then finishes too.
    ("10.0.2.101", 7, "echo"),
    // Leaves what it is sent unread and goes away: the guest sees a reset.
    ("10.0.2.102", 9, "reset"),
    // Character generator (RFC 864): sends until the guest makes it stop.
    ("10.0.2.101", 19, "chargen"),
    // TLS 1.3 echo, with a certificate from the lab authority for tls.lab.test.
    ("10.0.2.103", 443, "tls-echo"),
    // The same, offering one cipher suite only: every suite the guest offers is
    // used for real.
    ("10.0.2.103", 449, "tls-aes128"),
    ("10.0.2.103", 450, "tls-aes256"),
    // The same service behind each kind of certificate a client must refuse.
    ("10.0.2.103", 444, "tls-expired"),
    ("10.0.2.103", 445, "tls-wrongname"),
    ("10.0.2.103", 446, "tls-untrusted"),
    // A good handshake, then one bit flipped in the first record of data it sends.
    ("10.0.2.103", 447, "tls-tamper"),
    // A good handshake, a partial answer, then the connection closed without the
    // TLS close_notify.
    ("10.0.2.103", 448, "tls-truncate"),
    // The cloud model provider -- a mock (xtask/src/cloud.rs, ADR-0018).
    ("10.0.2.100", 443, "cloud"),
];

/// The zone the lab DNS server answers for. Nothing answers at 10.0.2.77 (not even
/// ARP): a connection there is never answered, which is how a timeout is tested.
const ZONE: &[(&str, [u8; 4])] = &[
    ("echo.lab.test", [10, 0, 2, 101]),
    // The echo service's address, used on a port nothing is forwarded to: QEMU
    // answers the SYN with a reset.
    ("closed.lab.test", [10, 0, 2, 101]),
    ("reset.lab.test", [10, 0, 2, 102]),
    ("chargen.lab.test", [10, 0, 2, 101]),
    ("blackhole.lab.test", [10, 0, 2, 77]),
    ("api.cloud.test", [10, 0, 2, 100]),
    ("tls.lab.test", [10, 0, 2, 103]),
];
/// Names answered with a CNAME to a name in [`ZONE`], and that name's address.
const ALIASES: &[(&str, &str)] = &[("alias.lab.test", "echo.lab.test")];

/// The value QEMU's `-netdev` option needs for the lab network: restricted
/// user-mode networking plus one forwarding rule per service, each running this
/// very program.
pub fn netdev() -> String {
    let exe = std::env::current_exe().map(|p| p.display().to_string()).unwrap_or_else(|_| "xtask".into());
    let mut s = String::from("user,id=spacenet,restrict=on");
    for (addr, port, name) in SERVICES {
        // QEMU splits options at commas (a literal one is doubled), and splits the
        // command into arguments the way a shell would.
        let cmd = format!("{} lab {name}", shell_quote(&exe)).replace(',', ",,");
        s.push_str(&format!(",guestfwd=tcp:{addr}:{port}-cmd:{cmd}"));
    }
    s
}

fn shell_quote(s: &str) -> String {
    if s.bytes().all(|b| b.is_ascii_alphanumeric() || b"/._-+".contains(&b)) {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', "'\\''"))
    }
}

/// Where the services write what they did. The standard streams are the guest's
/// connection, so nothing else may ever be written to them.
fn log_path() -> PathBuf {
    match std::env::var_os("SPACEOS_LAB_LOG") {
        Some(p) => PathBuf::from(p),
        None => super::root().join("build/logs/lab.log"),
    }
}

pub(super) fn log(line: &str) {
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(log_path()) {
        let _ = f.write_all(format!("{line}\n").as_bytes());
    }
}

/// `xtask lab <name>`: serve one connection.
pub fn main(name: Option<&str>) -> i32 {
    // A panic message on stderr would land in the guest's connection.
    std::panic::set_hook(Box::new(|info| log(&format!("lab: panic: {info}"))));
    let r = match name {
        Some("dns") => dns(),
        Some("echo") => echo(),
        Some("reset") => reset(),
        Some("chargen") => chargen(),
        Some(s @ "tls-echo") => tls_serve(s, "good", TlsMode::Echo, None),
        Some(s @ "tls-aes128") => {
            tls_serve(s, "good", TlsMode::Echo, Some(CipherSuite::TLS13_AES_128_GCM_SHA256))
        }
        Some(s @ "tls-aes256") => {
            tls_serve(s, "good", TlsMode::Echo, Some(CipherSuite::TLS13_AES_256_GCM_SHA384))
        }
        Some(s @ "tls-expired") => tls_serve(s, "expired", TlsMode::Echo, None),
        Some(s @ "tls-wrongname") => tls_serve(s, "wrongname", TlsMode::Echo, None),
        Some(s @ "tls-untrusted") => tls_serve(s, "untrusted", TlsMode::Echo, None),
        Some(s @ "tls-tamper") => tls_serve(s, "good", TlsMode::Tamper, None),
        Some(s @ "tls-truncate") => tls_serve(s, "good", TlsMode::Truncate, None),
        Some("cloud") => super::cloud::serve(),
        other => {
            log(&format!("lab: unknown service {other:?}"));
            return 2;
        }
    };
    match r {
        Ok(()) => 0,
        Err(e) => {
            log(&format!("lab {}: {e}", name.unwrap_or("?")));
            1
        }
    }
}

/// How long the echo service waits for a guest that has gone quiet. A guest whose
/// network service was killed can never close its connections, and nothing else
/// would end them: over a long run each would be one more process and one more
/// descriptor in QEMU, for good.
const ECHO_IDLE: Duration = Duration::from_secs(30);

fn echo() -> io::Result<()> {
    use std::sync::atomic::{AtomicU64, Ordering};
    let t0 = std::time::Instant::now();
    // Milliseconds since `t0` of the last byte from the guest, and bytes echoed.
    let last = Arc::new(AtomicU64::new(0));
    let total = Arc::new(AtomicU64::new(0));
    {
        let (last, total) = (last.clone(), total.clone());
        std::thread::spawn(move || {
            loop {
                std::thread::sleep(Duration::from_secs(1));
                let quiet = t0.elapsed().as_millis() as u64 - last.load(Ordering::Relaxed);
                if quiet >= ECHO_IDLE.as_millis() as u64 {
                    log(&format!(
                        "echo: nothing from the guest for {} s after {} bytes; closing",
                        quiet / 1000,
                        total.load(Ordering::Relaxed)
                    ));
                    std::process::exit(0);
                }
            }
        });
    }
    let mut input = io::stdin().lock();
    let mut output = io::stdout().lock();
    let mut buf = [0u8; 4096];
    loop {
        let n = input.read(&mut buf)?;
        last.store(t0.elapsed().as_millis() as u64, Ordering::Relaxed);
        if n == 0 {
            log(&format!("echo: {} bytes echoed; the guest finished sending", total.load(Ordering::Relaxed)));
            return Ok(());
        }
        output.write_all(&buf[..n])?;
        output.flush()?;
        total.fetch_add(n as u64, Ordering::Relaxed);
    }
}

/// Line `n` of the character generator: 72 of the 94 visible characters, starting
/// one further along each line, then CRLF (after RFC 864).
fn chargen_line(n: usize) -> [u8; 74] {
    let mut line = [0u8; 74];
    for (i, b) in line[..72].iter_mut().enumerate() {
        *b = b' ' + 1 + ((n + i) % 94) as u8;
    }
    line[72] = b'\r';
    line[73] = b'\n';
    line
}

/// Send lines until the guest makes it stop -- which it must, by resetting the
/// connection once it has stopped reading -- or for 10 s at most.
fn chargen() -> io::Result<()> {
    let mut output = io::stdout().lock();
    let t0 = std::time::Instant::now();
    let mut sent = 0usize;
    let mut n = 0;
    while t0.elapsed() < Duration::from_secs(10) {
        let mut block = Vec::with_capacity(74 * 14);
        for _ in 0..14 {
            block.extend_from_slice(&chargen_line(n));
            n += 1;
        }
        if let Err(e) = output.write_all(&block).and_then(|_| output.flush()) {
            log(&format!("chargen: the guest went away after {sent} bytes ({})", e.kind()));
            return Ok(());
        }
        sent += block.len();
        std::thread::sleep(Duration::from_millis(20));
    }
    log(&format!("chargen: sent {sent} bytes over 10 s and nobody stopped it"));
    Ok(())
}

fn reset() -> io::Result<()> {
    // Whatever the guest sends in this time stays unread; leaving with unread data
    // makes the socket report a reset, which QEMU passes on to the guest as RST.
    std::thread::sleep(Duration::from_millis(300));
    log("reset: leaving with the guest's data unread");
    Ok(())
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum TlsMode {
    Echo,
    Tamper,
    Truncate,
}

/// Inverts one bit of the first application-data record written once armed. The
/// byte stream is followed record by record (a 5-byte header, then as many bytes
/// as it says), so the flip lands in a record's payload wherever the writes split.
#[derive(Default)]
struct Tamper {
    armed: bool,
    done: bool,
    flip_next: bool,
    header: [u8; 5],
    have: usize,
    left: usize,
}

impl Tamper {
    fn apply(&mut self, bytes: &mut [u8]) {
        let mut i = 0;
        while i < bytes.len() {
            if self.left > 0 {
                if self.flip_next {
                    bytes[i] ^= 0x01;
                    self.flip_next = false;
                    self.done = true;
                }
                let n = self.left.min(bytes.len() - i);
                i += n;
                self.left -= n;
            } else {
                self.header[self.have] = bytes[i];
                self.have += 1;
                i += 1;
                if self.have == 5 {
                    self.have = 0;
                    self.left = u16::from_be_bytes([self.header[3], self.header[4]]) as usize;
                    self.flip_next = self.armed && !self.done && self.header[0] == 0x17 && self.left > 0;
                }
            }
        }
    }
}

/// The guest's connection as one stream: stdin in, stdout out, every write sent at
/// once (and through the tamperer, if there is one).
pub(super) struct Wire {
    input: io::Stdin,
    output: io::Stdout,
    tamper: Option<Tamper>,
}

impl Wire {
    pub(super) fn plain() -> Wire {
        Wire { input: io::stdin(), output: io::stdout(), tamper: None }
    }
}

impl Read for Wire {
    fn read(&mut self, b: &mut [u8]) -> io::Result<usize> {
        self.input.read(b)
    }
}

impl Write for Wire {
    fn write(&mut self, b: &[u8]) -> io::Result<usize> {
        match &mut self.tamper {
            Some(t) => {
                let mut v = b.to_vec();
                t.apply(&mut v);
                self.output.write_all(&v)?;
            }
            None => self.output.write_all(b)?,
        }
        self.output.flush()?;
        Ok(b.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.output.flush()
    }
}

/// TLS 1.3 presenting the lab leaf `stem`, with every suite or `only` one, and no
/// session tickets: the first record after the handshake is then data -- the one the
/// tamper service alters.
pub(super) fn server_config(stem: &str, only: Option<CipherSuite>) -> io::Result<Arc<rustls::ServerConfig>> {
    let (cert, key) = pki::load(stem).map_err(io::Error::other)?;
    let mut provider = rustls::crypto::ring::default_provider();
    if let Some(only) = only {
        provider.cipher_suites.retain(|s| s.suite() == only);
    }
    let mut cfg = rustls::ServerConfig::builder_with_provider(Arc::new(provider))
        .with_protocol_versions(&[&rustls::version::TLS13])
        .map_err(io::Error::other)?
        .with_no_client_auth()
        .with_single_cert(vec![cert], key)
        .map_err(io::Error::other)?;
    cfg.send_tls13_tickets = 0;
    Ok(Arc::new(cfg))
}

/// A TLS 1.3 server presenting the lab leaf `stem`, echoing what it is sent (or,
/// per `mode`, misbehaving in one precise way), offering every suite or `only` one.
/// It logs how the client ended the connection: that is where a refusal and its
/// alert show up.
fn tls_serve(service: &str, stem: &str, mode: TlsMode, only: Option<CipherSuite>) -> io::Result<()> {
    let mut conn = rustls::ServerConnection::new(server_config(stem, only)?).map_err(io::Error::other)?;
    let mut wire = Wire {
        input: io::stdin(),
        output: io::stdout(),
        tamper: (mode == TlsMode::Tamper).then(Tamper::default),
    };
    while conn.is_handshaking() {
        if let Err(e) = conn.complete_io(&mut wire) {
            log(&format!("{service}: the handshake ended: {e}"));
            return Ok(());
        }
    }
    let suite = conn.negotiated_cipher_suite().map(|s| format!("{:?}", s.suite())).unwrap_or_default();
    log(&format!("{service}: handshake complete ({suite})"));
    if let Some(t) = &mut wire.tamper {
        t.armed = true;
    }
    let mut buf = vec![0u8; 16384];
    let mut total = 0usize;
    loop {
        let n = match rustls::Stream::new(&mut conn, &mut wire).read(&mut buf) {
            Ok(n) => n,
            Err(e) => {
                log(&format!("{service}: after {total} bytes: {e}"));
                return Ok(());
            }
        };
        if n == 0 {
            break; // the client's close_notify
        }
        total += n;
        let reply: &[u8] = if mode == TlsMode::Truncate { b"this answer is cut short" } else { &buf[..n] };
        let mut tls = rustls::Stream::new(&mut conn, &mut wire);
        tls.write_all(reply)?;
        tls.flush()?;
        if mode == TlsMode::Truncate {
            log(&format!("{service}: answered, and left without close_notify"));
            return Ok(());
        }
    }
    conn.send_close_notify();
    while conn.wants_write() {
        conn.write_tls(&mut wire)?;
    }
    log(&format!("{service}: {total} bytes echoed, closed with close_notify both ways"));
    Ok(())
}

fn dns() -> io::Result<()> {
    let mut input = io::stdin().lock();
    let mut output = io::stdout().lock();
    loop {
        let mut len = [0u8; 2];
        match input.read_exact(&mut len) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(()),
            Err(e) => return Err(e),
        }
        let mut query = vec![0u8; u16::from_be_bytes(len) as usize];
        input.read_exact(&mut query)?;
        let Some(answer) = answer(&query) else {
            log("dns: a malformed query; closing");
            return Ok(());
        };
        output.write_all(&(answer.len() as u16).to_be_bytes())?;
        output.write_all(&answer)?;
        output.flush()?;
    }
}

/// The answer to one query: the question echoed, then an A record, a CNAME and
/// an A record, or nothing (NXDOMAIN for names outside the zone).
fn answer(q: &[u8]) -> Option<Vec<u8>> {
    if q.len() < 12 || u16::from_be_bytes([q[4], q[5]]) != 1 {
        return None;
    }
    // The question: labels (no compression in a query), then type and class.
    let mut at = 12;
    let mut labels = Vec::new();
    loop {
        let n = *q.get(at)? as usize;
        at += 1;
        if n == 0 {
            break;
        }
        if n > 63 {
            return None;
        }
        labels.push(String::from_utf8_lossy(q.get(at..at + n)?).to_ascii_lowercase());
        at += n;
    }
    let qtype = u16::from_be_bytes([*q.get(at)?, *q.get(at + 1)?]);
    let question_end = at + 4;
    if q.len() < question_end {
        return None;
    }
    let name = labels.join(".");
    let wire = |name: &str| -> Vec<u8> {
        let mut w = Vec::new();
        for l in name.split('.') {
            w.push(l.len() as u8);
            w.extend_from_slice(l.as_bytes());
        }
        w.push(0);
        w
    };
    let a_record = |owner: &[u8], addr: [u8; 4]| -> Vec<u8> {
        let mut r = owner.to_vec();
        r.extend_from_slice(&[0, 1, 0, 1, 0, 0, 0, 60, 0, 4]);
        r.extend_from_slice(&addr);
        r
    };
    let mut records: Vec<Vec<u8>> = Vec::new();
    let mut rcode = 0u16;
    let alias = ALIASES.iter().find(|(n, _)| *n == name);
    let direct = ZONE.iter().find(|(n, _)| *n == name);
    match (alias, direct) {
        (Some((_, target)), _) => {
            let (_, addr) = ZONE.iter().find(|(n, _)| n == target)?;
            let mut cname = vec![0xC0, 12, 0, 5, 0, 1, 0, 0, 0, 60];
            let t = wire(target);
            cname.extend_from_slice(&(t.len() as u16).to_be_bytes());
            cname.extend_from_slice(&t);
            records.push(cname);
            if qtype == 1 {
                records.push(a_record(&t, *addr));
            }
        }
        (None, Some((_, addr))) => {
            if qtype == 1 {
                records.push(a_record(&[0xC0, 12], *addr));
            }
        }
        (None, None) => rcode = 3,
    }
    let rd = u16::from_be_bytes([q[2], q[3]]) & 0x0100;
    let mut out = Vec::new();
    out.extend_from_slice(&q[0..2]);
    out.extend_from_slice(&(0x8000 | 0x0400 | rd | rcode).to_be_bytes()); // QR, AA
    out.extend_from_slice(&1u16.to_be_bytes());
    out.extend_from_slice(&(records.len() as u16).to_be_bytes());
    out.extend_from_slice(&[0, 0, 0, 0]);
    out.extend_from_slice(&q[12..question_end]);
    for r in &records {
        out.extend_from_slice(r);
    }
    log(&match (rcode, records.is_empty()) {
        (3, _) => format!("dns: {name}: no such name"),
        (_, true) => format!("dns: {name}: no record of type {qtype}"),
        _ => format!("dns: {name}: {} record(s)", records.len()),
    });
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn query(name: &str) -> Vec<u8> {
        let mut q = vec![0xAB, 0xCD, 0x01, 0x00, 0, 1, 0, 0, 0, 0, 0, 0];
        for l in name.split('.') {
            q.push(l.len() as u8);
            q.extend_from_slice(l.as_bytes());
        }
        q.extend_from_slice(&[0, 0, 1, 0, 1]);
        q
    }

    #[test]
    fn answers_the_zone() {
        let a = answer(&query("Echo.Lab.Test")).unwrap();
        assert_eq!(&a[0..2], &[0xAB, 0xCD]);
        assert_eq!(u16::from_be_bytes([a[2], a[3]]) & 0x800F, 0x8000);
        assert_eq!(u16::from_be_bytes([a[6], a[7]]), 1);
        assert_eq!(&a[a.len() - 4..], &[10, 0, 2, 101]);
        let nx = answer(&query("nosuch.lab.test")).unwrap();
        assert_eq!(u16::from_be_bytes([nx[2], nx[3]]) & 0xF, 3);
        let alias = answer(&query("alias.lab.test")).unwrap();
        assert_eq!(u16::from_be_bytes([alias[6], alias[7]]), 2);
        assert_eq!(&alias[alias.len() - 4..], &[10, 0, 2, 101]);
        assert!(answer(&[0; 5]).is_none());
    }

    #[test]
    fn chargen_lines_rotate() {
        assert_eq!(&chargen_line(0)[..3], b"!\"#");
        assert_eq!(&chargen_line(1)[..3], b"\"#$");
        assert_eq!(chargen_line(0)[71], b'h');
        assert_eq!(&chargen_line(93)[..2], b"~!");
        assert_eq!(&chargen_line(5)[72..], b"\r\n");
    }

    #[test]
    fn netdev_escapes_commas() {
        assert_eq!(shell_quote("/a/b-c/x.y"), "/a/b-c/x.y");
        assert_eq!(shell_quote("/a b/it's"), "'/a b/it'\\''s'");
    }
}
