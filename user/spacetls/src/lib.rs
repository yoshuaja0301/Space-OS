//! `spacetls` – a TLS 1.3 client for Space OS programs (ADR-0017).
//!
//! The protocol is rustls, used without std through its "unbuffered" connection
//! API, with the RustCrypto provider: pure Rust, forced onto its software
//! backends because user space has no SIMD. This crate is the plumbing between
//! that and this OS:
//!
//! * randomness comes from `SYS_RANDOM` (through `getrandom`'s custom hook); a
//!   machine with no entropy source cannot start a handshake at all;
//! * the time certificates are checked against comes from the real-time clock;
//! * the trust anchors are whatever the caller hands over -- nothing is built in;
//! * the transport is a TCP connection from the network service, so the session's
//!   allowlist decides where TLS may go before TLS is even involved.
//!
//! TLS 1.3 only. No session resumption, no early data, no client certificates.
#![no_std]

extern crate alloc;

use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use core::fmt;
use core::time::Duration;

use libspace::net::{Session, TcpStream};
use libspace::spaceabi::error::Error;
use libspace::spaceabi::syscall::RANDOM_MAX;
use libspace::sys;
use rustls::client::UnbufferedClientConnection;
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::time_provider::TimeProvider;
use rustls::unbuffered::{ConnectionState, EncodeError, EncryptError, UnbufferedStatus};
use rustls::{
    CertificateError, CipherSuiteCommon, ClientConfig, RootCertStore, SupportedCipherSuite, Tls13CipherSuite,
};

/// Largest TLS record on the wire: 2^14 bytes of plaintext, 256 of expansion, and
/// the 5-byte header. The receive buffer grows to hold one whole record at most
/// twice over; anything claiming to be bigger is refused by rustls first.
const RECORD_MAX: usize = 16384 + 256 + 5;
/// Plaintext bytes encrypted per record when writing.
const WRITE_CHUNK: usize = 4096;
/// Time allowed for telling the peer why the connection failed, on top of whatever
/// deadline the failure happened under.
const ALERT_MS: u64 = 1000;

/// Records one AES-GCM key may protect: rustls replaces the key (a TLS 1.3
/// KeyUpdate) before this many have been sent under it -- RFC 8446 §5.5, with the
/// bound rustls itself uses (2^24 full records: an attacker's advantage of at most
/// 2^-60). The provider sets no limit at all; ChaCha20-Poly1305 needs none.
const AES_GCM_RECORDS: u64 = 1 << 24;

/// A copy of the provider's TLS 1.3 suite `s`, held to [`AES_GCM_RECORDS`].
const fn aes_gcm_limited(s: SupportedCipherSuite) -> Tls13CipherSuite {
    let Some(t) = s.tls13() else { panic!("not a TLS 1.3 suite") };
    Tls13CipherSuite {
        common: CipherSuiteCommon {
            suite: t.common.suite,
            hash_provider: t.common.hash_provider,
            confidentiality_limit: AES_GCM_RECORDS,
        },
        hkdf_provider: t.hkdf_provider,
        aead_alg: t.aead_alg,
        quic: t.quic,
    }
}

static AES_128_GCM: Tls13CipherSuite = aes_gcm_limited(rustls_rustcrypto::TLS13_AES_128_GCM_SHA256);
static AES_256_GCM: Tls13CipherSuite = aes_gcm_limited(rustls_rustcrypto::TLS13_AES_256_GCM_SHA384);

fn space_getrandom(dest: &mut [u8]) -> Result<(), getrandom::Error> {
    for chunk in dest.chunks_mut(RANDOM_MAX) {
        match sys::random(chunk) {
            Ok(n) if n == chunk.len() => {}
            _ => return Err(getrandom::Error::UNSUPPORTED),
        }
    }
    Ok(())
}
getrandom::register_custom_getrandom!(space_getrandom);

#[derive(Debug)]
struct RealTimeClock;

impl TimeProvider for RealTimeClock {
    fn current_time(&self) -> Option<UnixTime> {
        sys::clock_realtime_ms().ok().map(|ms| UnixTime::since_unix_epoch(Duration::from_millis(ms)))
    }
}

/// Why a TLS connection failed.
#[derive(Debug)]
pub enum TlsError {
    /// The connection underneath failed, or could not be opened (a refusal by the
    /// session's allowlist arrives here as `Net(Denied)`).
    Net(Error),
    /// TLS refused: a certificate that is untrusted, expired or for another name, a
    /// record that does not authenticate, a protocol violation, an alert.
    Tls(rustls::Error),
    /// The peer's end closed without a TLS close_notify: what arrived may have been
    /// cut short by someone other than the peer.
    Truncated,
    /// The machine has no entropy source: no key can be made.
    NoEntropy,
    /// The machine has no real-time clock: certificate validity cannot be checked.
    NoClock,
    /// Nothing within the time allowed.
    TimedOut,
}

impl TlsError {
    /// A stable one-word name for the failure, for logs and for tests that must tell
    /// the refusals apart.
    pub fn kind(&self) -> &'static str {
        match self {
            TlsError::Net(_) => "net",
            TlsError::Truncated => "truncated",
            TlsError::NoEntropy => "no-entropy",
            TlsError::NoClock => "no-clock",
            TlsError::TimedOut => "timeout",
            TlsError::Tls(rustls::Error::InvalidCertificate(c)) => match c {
                CertificateError::Expired | CertificateError::ExpiredContext { .. } => "expired",
                CertificateError::NotValidYet | CertificateError::NotValidYetContext { .. } => {
                    "not-yet-valid"
                }
                CertificateError::NotValidForName | CertificateError::NotValidForNameContext { .. } => {
                    "wrong-name"
                }
                CertificateError::UnknownIssuer => "unknown-issuer",
                _ => "bad-certificate",
            },
            TlsError::Tls(rustls::Error::DecryptError) => "bad-record",
            TlsError::Tls(_) => "tls",
        }
    }
}

impl fmt::Display for TlsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TlsError::Net(e) => write!(f, "connection: {e}"),
            TlsError::Tls(e) => write!(f, "TLS: {e}"),
            TlsError::Truncated => f.write_str("the connection ended without a TLS close_notify"),
            TlsError::NoEntropy => f.write_str("no entropy source: TLS cannot make keys"),
            TlsError::NoClock => f.write_str("no clock: certificate validity cannot be checked"),
            TlsError::TimedOut => f.write_str("timed out"),
        }
    }
}

impl From<rustls::Error> for TlsError {
    fn from(e: rustls::Error) -> Self {
        TlsError::Tls(e)
    }
}

/// Who a client trusts, and how it talks.
pub struct TlsConfig {
    config: Arc<ClientConfig>,
}

impl TlsConfig {
    /// Trust exactly these certificate authorities (DER): nothing else, and nothing
    /// built in.
    pub fn with_roots(roots_der: &[&[u8]]) -> Result<TlsConfig, TlsError> {
        let mut roots = RootCertStore::empty();
        for der in roots_der {
            roots.add(CertificateDer::from(der.to_vec()))?;
        }
        let mut provider = rustls_rustcrypto::provider();
        // ChaCha20-Poly1305 first: the fastest of the three without AES or vector
        // instructions, and a server that honours the client's order picks it.
        provider.cipher_suites = vec![
            rustls_rustcrypto::TLS13_CHACHA20_POLY1305_SHA256,
            SupportedCipherSuite::Tls13(&AES_128_GCM),
            SupportedCipherSuite::Tls13(&AES_256_GCM),
        ];
        let mut config = ClientConfig::builder_with_details(Arc::new(provider), Arc::new(RealTimeClock))
            .with_protocol_versions(&[&rustls::version::TLS13])?
            .with_root_certificates(roots)
            .with_no_client_auth();
        config.resumption = rustls::client::Resumption::disabled();
        Ok(TlsConfig { config: Arc::new(config) })
    }
}

/// One TLS connection over one TCP connection of a session.
pub struct TlsStream {
    tcp: TcpStream,
    conn: UnbufferedClientConnection,
    /// TLS bytes received and not yet consumed by rustls.
    incoming: Vec<u8>,
    in_len: usize,
    /// Scratch for records on their way out.
    outgoing: Vec<u8>,
    out_len: usize,
    /// Decrypted bytes not yet handed to the reader.
    plain: Vec<u8>,
    plain_at: usize,
    peer_closed: bool,
    /// The cipher suite agreed on, for the caller's log.
    pub suite: Option<rustls::CipherSuite>,
    /// Milliseconds it took to open the TCP connection, name lookup included.
    pub tcp_ms: u64,
    /// Milliseconds the TLS handshake took after that.
    pub handshake_ms: u64,
}

impl TlsStream {
    /// Open a TCP connection to `host:port` through `session` and complete a TLS 1.3
    /// handshake that authenticates the server as `host` under `config`.
    pub fn connect(
        session: &Session,
        host: &str,
        port: u16,
        config: &TlsConfig,
        timeout_ms: u32,
    ) -> Result<TlsStream, TlsError> {
        // Fail plainly before any handshake rather than deep inside one.
        if sys::random(&mut [0u8; 1]).is_err() {
            return Err(TlsError::NoEntropy);
        }
        if sys::clock_realtime_ms().is_err() {
            return Err(TlsError::NoClock);
        }
        let deadline = sys::ticks_ms() + timeout_ms as u64;
        let name = ServerName::try_from(String::from(host)).map_err(|_| TlsError::Net(Error::Invalid))?;
        let conn = UnbufferedClientConnection::new(config.config.clone(), name)?;
        let t0 = sys::ticks_ms();
        let tcp = session.connect(host, port, timeout_ms).map_err(TlsError::Net)?;
        let t1 = sys::ticks_ms();
        let mut s = TlsStream {
            tcp,
            conn,
            incoming: vec![0; RECORD_MAX],
            in_len: 0,
            outgoing: vec![0; RECORD_MAX],
            out_len: 0,
            plain: Vec::new(),
            plain_at: 0,
            peer_closed: false,
            suite: None,
            tcp_ms: t1 - t0,
            handshake_ms: 0,
        };
        match s.handshake(deadline) {
            Ok(()) => {
                s.suite = s.conn.negotiated_cipher_suite().map(|c| c.suite());
                s.handshake_ms = sys::ticks_ms() - t1;
                Ok(s)
            }
            Err(e) => {
                s.send_pending_alert();
                Err(e)
            }
        }
    }

    /// Pass `r` on, first sending the alert a TLS failure leaves behind.
    fn alert_on<T>(&mut self, r: Result<T, TlsError>) -> Result<T, TlsError> {
        if let Err(TlsError::Tls(_)) = r {
            self.send_pending_alert();
        }
        r
    }

    fn left(deadline: u64) -> Result<u64, TlsError> {
        match deadline.saturating_sub(sys::ticks_ms()) {
            0 => Err(TlsError::TimedOut),
            n => Ok(n),
        }
    }

    fn net(e: Error) -> TlsError {
        match e {
            Error::TimedOut => TlsError::TimedOut,
            e => TlsError::Net(e),
        }
    }

    /// Drop the first `n` bytes of the receive buffer: rustls is done with them.
    fn discard(&mut self, n: usize) {
        self.incoming.copy_within(n..self.in_len, 0);
        self.in_len -= n;
    }

    /// Read more TLS bytes from the connection. An end of stream here is a
    /// truncation: a TLS peer that is done says so first.
    fn fill(&mut self, deadline: u64) -> Result<(), TlsError> {
        if self.in_len == self.incoming.len() {
            if self.incoming.len() >= 2 * RECORD_MAX {
                return Err(TlsError::Tls(rustls::Error::General(String::from(
                    "record larger than TLS allows",
                ))));
            }
            self.incoming.resize(self.incoming.len() * 2, 0);
        }
        let left = Self::left(deadline)?;
        match self.tcp.read(&mut self.incoming[self.in_len..], left) {
            Ok(0) => Err(TlsError::Truncated),
            Ok(n) => {
                self.in_len += n;
                Ok(())
            }
            Err(e) => Err(Self::net(e)),
        }
    }

    fn send_outgoing(&mut self, deadline: u64) -> Result<(), TlsError> {
        let left = Self::left(deadline)?;
        self.tcp.write_all(&self.outgoing[..self.out_len], left).map_err(Self::net)?;
        self.out_len = 0;
        Ok(())
    }

    fn handshake(&mut self, deadline: u64) -> Result<(), TlsError> {
        loop {
            let UnbufferedStatus { discard, state } =
                self.conn.process_tls_records(&mut self.incoming[..self.in_len]);
            let mut need_input = false;
            let mut done = false;
            let mut transmit = false;
            match state? {
                ConnectionState::EncodeTlsData(mut e) => loop {
                    match e.encode(&mut self.outgoing[self.out_len..]) {
                        Ok(n) => {
                            self.out_len += n;
                            break;
                        }
                        Err(EncodeError::InsufficientSize(sz)) => {
                            let need = self.out_len + sz.required_size;
                            self.outgoing.resize(need, 0);
                        }
                        Err(_) => break,
                    }
                },
                ConnectionState::TransmitTlsData(t) => {
                    transmit = true;
                    t.done();
                }
                ConnectionState::BlockedHandshake => need_input = true,
                ConnectionState::WriteTraffic(_) => done = true,
                ConnectionState::PeerClosed | ConnectionState::Closed => {
                    return Err(TlsError::Net(Error::PeerClosed));
                }
                _ => need_input = true,
            }
            self.discard(discard);
            if transmit {
                self.send_outgoing(deadline)?;
            }
            if done {
                return Ok(());
            }
            if need_input {
                self.fill(deadline)?;
            }
        }
    }

    /// After a failure rustls may still hold the alert that tells the peer why (a
    /// certificate it refused, a record that did not authenticate): send it, so
    /// the peer hears the reason instead of a bare disconnect. RFC 8446 requires it
    /// for a record that does not authenticate (`bad_record_mac`).
    fn send_pending_alert(&mut self) {
        let deadline = sys::ticks_ms() + ALERT_MS;
        for _ in 0..4 {
            let UnbufferedStatus { discard, state } =
                self.conn.process_tls_records(&mut self.incoming[..self.in_len]);
            let mut transmit = false;
            match state {
                Ok(ConnectionState::EncodeTlsData(mut e)) => {
                    if let Ok(n) = e.encode(&mut self.outgoing[self.out_len..]) {
                        self.out_len += n;
                    }
                }
                Ok(ConnectionState::TransmitTlsData(t)) => {
                    transmit = true;
                    t.done();
                }
                _ => {
                    self.discard(discard);
                    break;
                }
            }
            self.discard(discard);
            if transmit {
                let _ = self.send_outgoing(deadline);
            }
        }
        if self.out_len > 0 {
            let _ = self.send_outgoing(deadline);
        }
    }

    /// Take whatever application data rustls decrypted; returns the extra bytes the
    /// records say to discard.
    fn collect(
        plain: &mut Vec<u8>,
        r: &mut rustls::unbuffered::ReadTraffic<'_, '_, rustls::client::ClientConnectionData>,
    ) -> Result<usize, TlsError> {
        let mut extra = 0;
        while let Some(rec) = r.next_record() {
            let rec = rec?;
            plain.extend_from_slice(rec.payload);
            extra += rec.discard;
        }
        Ok(extra)
    }

    /// Encrypt and send all of `data`, for at most `timeout_ms`.
    pub fn write_all(&mut self, data: &[u8], timeout_ms: u64) -> Result<(), TlsError> {
        let r = self.write_until(data, sys::ticks_ms() + timeout_ms);
        self.alert_on(r)
    }

    fn write_until(&mut self, data: &[u8], deadline: u64) -> Result<(), TlsError> {
        for chunk in data.chunks(WRITE_CHUNK) {
            loop {
                let UnbufferedStatus { discard, state } =
                    self.conn.process_tls_records(&mut self.incoming[..self.in_len]);
                let mut extra = 0;
                let mut written = 0;
                let mut transmit = false;
                let mut need_input = false;
                match state? {
                    ConnectionState::WriteTraffic(mut w) => loop {
                        match w.encrypt(chunk, &mut self.outgoing) {
                            Ok(n) => {
                                written = n;
                                break;
                            }
                            Err(EncryptError::InsufficientSize(sz)) => {
                                self.outgoing.resize(sz.required_size, 0)
                            }
                            Err(_) => return Err(TlsError::Tls(rustls::Error::EncryptError)),
                        }
                    },
                    ConnectionState::ReadTraffic(mut r) => extra = Self::collect(&mut self.plain, &mut r)?,
                    ConnectionState::EncodeTlsData(mut e) => {
                        if let Ok(n) = e.encode(&mut self.outgoing[self.out_len..]) {
                            self.out_len += n;
                        }
                    }
                    ConnectionState::TransmitTlsData(t) => {
                        transmit = true;
                        t.done();
                    }
                    ConnectionState::PeerClosed => self.peer_closed = true,
                    ConnectionState::Closed => return Err(TlsError::Net(Error::PeerClosed)),
                    _ => need_input = true,
                }
                self.discard(discard + extra);
                if transmit {
                    self.send_outgoing(deadline)?;
                }
                if written > 0 {
                    self.out_len = written;
                    self.send_outgoing(deadline)?;
                    break;
                }
                if need_input {
                    self.fill(deadline)?;
                }
            }
        }
        Ok(())
    }

    /// Read decrypted data, waiting up to `timeout_ms` for some. `Ok(0)` is the
    /// peer's close_notify: everything it sent has arrived.
    pub fn read(&mut self, out: &mut [u8], timeout_ms: u64) -> Result<usize, TlsError> {
        let r = self.read_until(out, sys::ticks_ms() + timeout_ms);
        self.alert_on(r)
    }

    fn read_until(&mut self, out: &mut [u8], deadline: u64) -> Result<usize, TlsError> {
        loop {
            if self.plain_at < self.plain.len() {
                let n = out.len().min(self.plain.len() - self.plain_at);
                out[..n].copy_from_slice(&self.plain[self.plain_at..self.plain_at + n]);
                self.plain_at += n;
                if self.plain_at == self.plain.len() {
                    self.plain.clear();
                    self.plain_at = 0;
                }
                return Ok(n);
            }
            if self.peer_closed || out.is_empty() {
                return Ok(0);
            }
            let UnbufferedStatus { discard, state } =
                self.conn.process_tls_records(&mut self.incoming[..self.in_len]);
            let mut extra = 0;
            let mut transmit = false;
            let mut need_input = false;
            match state? {
                ConnectionState::ReadTraffic(mut r) => extra = Self::collect(&mut self.plain, &mut r)?,
                ConnectionState::PeerClosed | ConnectionState::Closed => self.peer_closed = true,
                ConnectionState::EncodeTlsData(mut e) => {
                    if let Ok(n) = e.encode(&mut self.outgoing[self.out_len..]) {
                        self.out_len += n;
                    }
                }
                ConnectionState::TransmitTlsData(t) => {
                    transmit = true;
                    t.done();
                }
                // Nothing to read yet: get more from the wire.
                _ => need_input = true,
            }
            self.discard(discard + extra);
            if transmit {
                self.send_outgoing(deadline)?;
            }
            if need_input && self.plain.is_empty() {
                self.fill(deadline)?;
            }
        }
    }

    /// Say goodbye properly: send close_notify, then end the TCP stream.
    pub fn close(mut self, timeout_ms: u64) -> Result<(), TlsError> {
        let deadline = sys::ticks_ms() + timeout_ms;
        for _ in 0..8 {
            let UnbufferedStatus { discard, state } =
                self.conn.process_tls_records(&mut self.incoming[..self.in_len]);
            let mut extra = 0;
            let mut sent = false;
            match state? {
                ConnectionState::WriteTraffic(mut w) => {
                    let n = w
                        .queue_close_notify(&mut self.outgoing)
                        .map_err(|_| TlsError::Tls(rustls::Error::EncryptError))?;
                    self.out_len = n;
                    sent = true;
                }
                ConnectionState::ReadTraffic(mut r) => extra = Self::collect(&mut self.plain, &mut r)?,
                ConnectionState::Closed => sent = true,
                _ => {}
            }
            self.discard(discard + extra);
            if sent {
                if self.out_len > 0 {
                    self.send_outgoing(deadline)?;
                }
                self.tcp.shutdown(Self::left(deadline)?).map_err(Self::net)?;
                // Wait for the peer to finish too (its close_notify, then its FIN), so
                // the connection closes cleanly instead of being reset with its last
                // bytes unread.
                let mut scratch = [0u8; 256];
                while let Ok(left) = Self::left(deadline) {
                    match self.tcp.read(&mut scratch, left) {
                        Ok(0) | Err(_) => break,
                        Ok(_) => {}
                    }
                }
                return Ok(());
            }
        }
        Err(TlsError::Tls(rustls::Error::General(String::from("could not queue close_notify"))))
    }
}
