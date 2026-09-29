//! `spacenet` – the network service.
//!
//! The kernel hands out one lease on the network device and moves Ethernet frames
//! through it. Everything above the frames lives here, in one process: DHCP, ARP,
//! IPv4 and TCP (smoltcp, a ported library -- ADR-0016) and a DNS resolver. Other
//! programs never touch the device. They get a *session* from whoever started the
//! service, and a session carries an allowlist of destinations: a name or address
//! that is not on it is refused before a single packet leaves, and the refusal is
//! counted (PRD §5: default deny).
//!
//! Every TCP connection is a channel of its own ([`spaceabi::net::sock`]). Flow
//! control is the two queues: the service takes a client's data only when the
//! connection has room for it, and hands data over only as fast as the client's
//! queue accepts it. A client that stops reading stalls its own connection, never
//! the service.
#![no_std]
#![no_main]

extern crate alloc;

mod device;
mod resolver;

use alloc::format;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

use libspace::spaceabi::dns;
use libspace::spaceabi::error::Error;
use libspace::spaceabi::handle::{kind, rights};
use libspace::spaceabi::net::{
    self as abi, ABI_VERSION, ALLOW_MAX, ANY_HOST, ANY_PORT, DATA_MAX, NetReply, NetRequest, SESSIONS_MAX,
    SOCKETS_MAX, SOCKETS_PER_SESSION, req, sock,
};
use libspace::spaceabi::syscall::{MSG_MAX, WAIT_FOREVER, WAIT_MAX};
use libspace::{Handle, handle, println, sys};
use smoltcp::iface::{Config, Interface, PollResult, SocketHandle, SocketSet};
use smoltcp::socket::{Socket, dhcpv4, tcp};
use smoltcp::time::Instant;
use smoltcp::wire::{EthernetAddress, HardwareAddress, IpAddress, IpCidr, Ipv4Address};

use device::Lease;
use resolver::{Query, Waiter};

/// Heap for socket buffers: eight open connections and up to [`GRAVEYARD_MAX`]
/// closing ones of 16 KiB each, lookups, the stack itself.
const HEAP_PAGES: usize = 160;
const TCP_RX: usize = 8192;
const TCP_TX: usize = 8192;
/// How long a connection that is being let go of may take to close cleanly before
/// it is reset.
const LINGER_MS: u64 = 5000;
/// How long a connection stays in TIME-WAIT: long enough for the last ACK (which
/// smoltcp may hold back for its 10 ms ACK delay) to leave, and to answer a FIN the
/// peer sends again -- far short of the full 2 MSL, which would tie up a socket for
/// seconds per connection.
const TIME_WAIT_MS: u64 = 250;
/// Retry interval while a client's queue is full: doubling from the first to the
/// last while the client keeps not reading, so a stalled client costs the service
/// a wake-up every 64 ms, not every 2.
const PUSH_RETRY_FIRST_MS: u64 = 2;
const PUSH_RETRY_LAST_MS: u64 = 64;
/// Sockets let go of but still closing. Past this, the oldest is reset: a client
/// opening and dropping connections faster than they close must not be able to
/// fill the heap with lingering ones.
const GRAVEYARD_MAX: usize = 12;
/// Deadlines used when a request leaves `timeout_ms` at 0.
const ATTACH_DEFAULT_MS: u64 = 5000;
const RESOLVE_DEFAULT_MS: u64 = 3000;
const CONNECT_DEFAULT_MS: u64 = 5000;
const DNS_PORT: u16 = 53;
const EPHEMERAL_FIRST: u16 = 49152;
/// Passes in a row without sleeping before the loop sleeps anyway for a tick.
const SPIN_MAX: u32 = 256;

fn now_ms() -> u64 {
    sys::ticks_ms()
}

fn instant(ms: u64) -> Instant {
    Instant::from_millis(ms as i64)
}

fn close(h: Option<Handle>) {
    if let Some(h) = h {
        sys::handle_close(h).ok();
    }
}

/// An IPv4 address, for the log.
struct Ip([u8; 4]);

impl core::fmt::Display for Ip {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let a = self.0;
        write!(f, "{}.{}.{}.{}", a[0], a[1], a[2], a[3])
    }
}

fn port_text(port: u16) -> String {
    if port == ANY_PORT { String::from("any port") } else { format!("port {port}") }
}

fn send_reply(chan: Handle, r: &NetReply) {
    // A client asks one question at a time, so a full queue means one that does
    // not read its answers; it loses this one. A closed channel is noticed on the
    // next read.
    let _ = sys::send(chan, r.as_bytes(), None);
}

#[derive(Clone, Copy, Default)]
struct Counters {
    opened: u64,
    refused: u64,
    resolved: u64,
}

impl Counters {
    fn reply(&self) -> NetReply {
        NetReply { value: self.opened, value2: self.refused, value3: self.resolved, ..Default::default() }
    }
}

struct Session {
    id: u32,
    chan: Handle,
    allow: Vec<(String, u16)>,
    counters: Counters,
    /// A lookup is under way for this session; nothing more is read from it until
    /// the answer has gone out, so answers come back in the order asked.
    busy: bool,
}

impl Session {
    /// May this session learn where `host` is? Any entry naming it, on any port.
    fn may_resolve(&self, host: &str) -> bool {
        self.allow.iter().any(|(h, _)| h == ANY_HOST || h.eq_ignore_ascii_case(host))
    }

    fn may_connect(&self, host: &str, port: u16) -> bool {
        self.allow.iter().any(|(h, p)| abi::allow_matches(h, *p, host, port))
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase {
    /// Waiting for the resolver.
    Resolving,
    /// The handshake is under way (or waiting for ARP), until `deadline`.
    Connecting,
    /// Data flows.
    Open,
}

struct Conn {
    id: u32,
    session: u32,
    chan: Handle,
    /// `host:port` as the client named it, for the log.
    label: String,
    port: u16,
    remote: [u8; 4],
    phase: Phase,
    tcp: Option<SocketHandle>,
    started: u64,
    deadline: u64,
    bytes_in: u64,
    bytes_out: u64,
    eof_sent: bool,
    /// The client sent SHUTDOWN: our FIN is queued behind its data.
    shut: bool,
    /// A control message the client's queue had no room for yet; nothing may
    /// overtake it.
    outbox: Option<([u8; 8], usize)>,
    /// Received data is waiting for room in the client's queue; retry at this
    /// interval.
    push_blocked: bool,
    push_retry_ms: u64,
    /// The client closed its end of the channel.
    client_gone: bool,
    linger_until: u64,
    /// Nothing more will happen: once `outbox` is out -- or `linger_until` has
    /// passed without the client making room for it -- the channel closes and the
    /// connection is forgotten.
    over: bool,
}

impl Conn {
    fn finish(&mut self, now: u64) {
        if !self.over {
            self.over = true;
            if !self.client_gone {
                self.linger_until = now + LINGER_MS;
            }
        }
    }
}

/// A socket let go of that is still closing.
struct Grave {
    sock: SocketHandle,
    until: u64,
    /// Pass in which it was reset: removed only after that pass's transmit, or the
    /// reset would never leave.
    aborted_pass: Option<u64>,
    /// When it was first seen in TIME-WAIT.
    time_wait_since: Option<u64>,
}

#[derive(Clone, Copy)]
struct IfConfig {
    addr: [u8; 4],
    prefix: u8,
    gateway: [u8; 4],
    dns: [u8; 4],
}

struct Service {
    op: Handle,
    dev: Lease,
    iface: Interface,
    sockets: SocketSet<'static>,
    dhcp: SocketHandle,
    mac: [u8; 6],
    config: Option<IfConfig>,
    dns_override: Option<([u8; 4], u16)>,
    /// ATTACH is answered once DHCP has configured the interface, or at this time.
    attach_deadline: Option<u64>,
    sessions: Vec<Session>,
    last_session: Option<u32>,
    conns: Vec<Conn>,
    queries: Vec<Query>,
    graveyard: Vec<Grave>,
    totals: Counters,
    next_id: u32,
    next_port: u16,
    next_query: u16,
    pass: u64,
    quit: bool,
}

#[unsafe(no_mangle)]
pub extern "C" fn space_main() -> i32 {
    libspace::heap::set_pages(HEAP_PAGES);
    println!("[net] Space OS network service, ABI v{ABI_VERSION}");
    let op = handle::BOOTSTRAP;
    let Some((nic, attach_ms)) = wait_attach(op) else { return 0 };
    let mut svc = match Service::new(op, nic, attach_ms) {
        Ok(s) => s,
        Err(e) => {
            println!("[net] cannot use the device lease: {e}");
            sys::handle_close(nic).ok();
            send_reply(op, &NetReply::error(e));
            return 1;
        }
    };
    svc.run();
    0
}

/// Serve the operator channel until it hands over the device lease.
fn wait_attach(op: Handle) -> Option<(Handle, u64)> {
    let mut buf = [0u8; MSG_MAX];
    loop {
        let (n, carried) = match sys::recv(op, &mut buf, false) {
            Ok(x) => x,
            Err(_) => {
                println!("[net] the operator left before attaching; exiting");
                return None;
            }
        };
        match (NetRequest::from_bytes(&buf[..n]), carried) {
            (Some(r), Some(nic)) if r.kind == req::ATTACH && r.abi_version == ABI_VERSION => {
                let ms = if r.timeout_ms == 0 { ATTACH_DEFAULT_MS } else { r.timeout_ms as u64 };
                return Some((nic, ms));
            }
            (Some(r), carried) if r.kind == req::QUIT => {
                close(carried);
                send_reply(op, &NetReply::default());
                return None;
            }
            (r, carried) => {
                close(carried);
                // Anything but ATTACH first: the interface is not there yet.
                let e = if r.is_some() { Error::Unreachable } else { Error::Invalid };
                send_reply(op, &NetReply::error(e));
            }
        }
    }
}

impl Service {
    fn new(op: Handle, nic: Handle, attach_ms: u64) -> Result<Service, Error> {
        let info = sys::handle_info(nic)?;
        if info.kind != kind::NIC
            || info.rights & (rights::READ | rights::WRITE) != rights::READ | rights::WRITE
        {
            return Err(Error::Denied);
        }
        let dev_info = sys::net_info(nic)?;
        let mac = dev_info.mac;
        // smoltcp refuses (panics on) a group address; so do we, politely.
        if mac == [0; 6] || mac[0] & 1 != 0 {
            return Err(Error::Invalid);
        }
        let mut dev = Lease::new(nic);
        let mut mac8 = [0u8; 8];
        mac8[..6].copy_from_slice(&mac);
        let seed = sys::clock_realtime_ms().unwrap_or(0) ^ (now_ms() << 32) ^ u64::from_le_bytes(mac8);
        let mut cfg = Config::new(HardwareAddress::Ethernet(EthernetAddress(mac)));
        // TCP sequence numbers and the first local port come from this seed, so two
        // boots do not reuse each other's.
        cfg.random_seed = seed;
        let iface = Interface::new(cfg, &mut dev, instant(now_ms()));
        let mut sockets = SocketSet::new(Vec::new());
        let dhcp = sockets.add(dhcpv4::Socket::new());
        println!(
            "[net] attached: mac {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}, link {}; asking DHCP for an address",
            mac[0],
            mac[1],
            mac[2],
            mac[3],
            mac[4],
            mac[5],
            if dev_info.link_up != 0 { "up" } else { "down" }
        );
        Ok(Service {
            op,
            dev,
            iface,
            sockets,
            dhcp,
            mac,
            config: None,
            dns_override: None,
            attach_deadline: Some(now_ms() + attach_ms),
            sessions: Vec::new(),
            last_session: None,
            conns: Vec::new(),
            queries: Vec::new(),
            graveyard: Vec::new(),
            totals: Counters::default(),
            next_id: 0,
            next_port: EPHEMERAL_FIRST + (seed % 16384) as u16,
            next_query: seed as u16,
            pass: 0,
            quit: false,
        })
    }

    fn run(&mut self) {
        let mut spins = 0;
        loop {
            self.pass += 1;
            self.iface.poll(instant(now_ms()), &mut self.dev, &mut self.sockets);
            let mut busy = self.dhcp_events();
            busy |= self.serve_operator();
            if self.quit {
                break;
            }
            busy |= self.serve_sessions();
            busy |= self.advance_queries();
            busy |= self.advance_conns();
            self.tend_graveyard();
            // Transmit what the steps above queued, and take in anything that came.
            let changed = self.iface.poll(instant(now_ms()), &mut self.dev, &mut self.sockets)
                == PollResult::SocketStateChanged;
            if (busy || changed) && spins < SPIN_MAX {
                spins += 1;
                continue;
            }
            let (set, n, timeout) = self.wait_set();
            let timeout = if spins >= SPIN_MAX { timeout.min(1) } else { timeout };
            spins = 0;
            let _ = sys::wait_any(&set[..n], timeout);
        }
        self.shutdown();
    }

    fn fresh_id(&mut self) -> u32 {
        self.next_id += 1;
        self.next_id
    }

    /// A local port none of our sockets uses.
    fn fresh_port(&mut self) -> u16 {
        loop {
            let p = self.next_port;
            self.next_port = if p == u16::MAX { EPHEMERAL_FIRST } else { p + 1 };
            let used = self.sockets.iter().any(|(_, s)| match s {
                Socket::Tcp(t) => t.local_endpoint().is_some_and(|e| e.port == p),
                _ => false,
            });
            if !used {
                return p;
            }
        }
    }

    fn dns_server(&self) -> Option<([u8; 4], u16)> {
        self.dns_override.or_else(|| self.config.filter(|c| c.dns != [0; 4]).map(|c| (c.dns, DNS_PORT)))
    }

    fn hello_reply(&self) -> NetReply {
        let Some(c) = self.config else { return NetReply::error(Error::Unreachable) };
        NetReply {
            prefix: c.prefix,
            addr: c.addr,
            gateway: c.gateway,
            dns: self.dns_server().map_or([0; 4], |(a, _)| a),
            mac: self.mac,
            ..Default::default()
        }
    }

    fn dhcp_events(&mut self) -> bool {
        let event = match self.sockets.get_mut::<dhcpv4::Socket>(self.dhcp).poll() {
            None => None,
            Some(dhcpv4::Event::Configured(cfg)) => {
                Some(Some((cfg.address, cfg.router, cfg.dns_servers.first().copied())))
            }
            Some(dhcpv4::Event::Deconfigured) => Some(None),
        };
        match event {
            None => false,
            Some(Some((cidr, router, dns))) => {
                self.iface.update_ip_addrs(|addrs| {
                    addrs.clear();
                    let _ = addrs.push(IpCidr::Ipv4(cidr));
                });
                match router {
                    Some(r) => {
                        let _ = self.iface.routes_mut().add_default_ipv4_route(r);
                    }
                    None => {
                        self.iface.routes_mut().remove_default_ipv4_route();
                    }
                }
                let c = IfConfig {
                    addr: cidr.address().octets(),
                    prefix: cidr.prefix_len(),
                    gateway: router.map_or([0; 4], |r| r.octets()),
                    dns: dns.map_or([0; 4], |d| d.octets()),
                };
                let or_none =
                    |a: [u8; 4]| if a == [0; 4] { String::from("none") } else { format!("{}", Ip(a)) };
                println!(
                    "[net] DHCP: address {}/{}, router {}, DNS server {}",
                    Ip(c.addr),
                    c.prefix,
                    or_none(c.gateway),
                    or_none(c.dns)
                );
                self.config = Some(c);
                if self.attach_deadline.take().is_some() {
                    send_reply(self.op, &self.hello_reply());
                }
                true
            }
            // Also reported once at the start, before there was anything to lose.
            Some(None) => {
                self.iface.update_ip_addrs(|addrs| addrs.clear());
                self.iface.routes_mut().remove_default_ipv4_route();
                if self.config.take().is_some() {
                    println!("[net] DHCP: the lease is gone; no address until a new one");
                }
                true
            }
        }
    }

    // ---- operator ----------------------------------------------------------------

    fn serve_operator(&mut self) -> bool {
        if let Some(d) = self.attach_deadline {
            if now_ms() < d {
                return false;
            }
            self.attach_deadline = None;
            println!("[net] DHCP: no address within the time ATTACH allowed; still asking");
            send_reply(self.op, &NetReply::error(Error::TimedOut));
        }
        let mut did = false;
        for _ in 0..8 {
            let mut buf = [0u8; MSG_MAX];
            let (n, carried) = match sys::recv(self.op, &mut buf, true) {
                Ok(x) => x,
                Err(Error::WouldBlock) => break,
                Err(_) => {
                    println!("[net] the operator closed its channel; shutting down");
                    self.quit = true;
                    return true;
                }
            };
            did = true;
            let reply = match NetRequest::from_bytes(&buf[..n]) {
                Some(r) if r.abi_version == ABI_VERSION => self.operator_request(&r, carried),
                _ => {
                    close(carried);
                    NetReply::error(Error::Invalid)
                }
            };
            send_reply(self.op, &reply);
            if self.quit {
                break;
            }
        }
        did
    }

    fn operator_request(&mut self, r: &NetRequest, carried: Option<Handle>) -> NetReply {
        if r.kind != req::SESSION {
            close(carried);
        }
        match r.kind {
            req::SESSION => {
                let Some(chan) = carried else { return NetReply::error(Error::Invalid) };
                let usable = sys::handle_info(chan).is_ok_and(|i| {
                    i.kind == kind::CHANNEL
                        && i.rights & (rights::SEND | rights::RECV) == rights::SEND | rights::RECV
                });
                if !usable {
                    close(Some(chan));
                    return NetReply::error(Error::Invalid);
                }
                if self.sessions.len() >= SESSIONS_MAX {
                    close(Some(chan));
                    return NetReply::error(Error::Busy);
                }
                let id = self.fresh_id();
                self.sessions.push(Session {
                    id,
                    chan,
                    allow: Vec::new(),
                    counters: Counters::default(),
                    busy: false,
                });
                self.last_session = Some(id);
                println!("[net] session {id} opened; nothing is allowed yet");
                NetReply { value: id as u64, ..Default::default() }
            }
            req::ALLOW => {
                let Some(host) = r.host() else { return NetReply::error(Error::Invalid) };
                if !(host == ANY_HOST || dns::valid_name(host) || abi::parse_ipv4(host).is_some()) {
                    return NetReply::error(Error::Invalid);
                }
                let last = self.last_session;
                let Some(s) = self.sessions.iter_mut().find(|s| Some(s.id) == last) else {
                    return NetReply::error(Error::NotFound);
                };
                if s.allow.len() >= ALLOW_MAX {
                    return NetReply::error(Error::Busy);
                }
                s.allow.push((String::from(host), r.port));
                println!("[net] session {}: allow {host}, {}", s.id, port_text(r.port));
                NetReply::default()
            }
            req::SET_DNS => {
                if r.addr == [0; 4] {
                    return NetReply::error(Error::Invalid);
                }
                let port = if r.port == 0 { DNS_PORT } else { r.port };
                self.dns_override = Some((r.addr, port));
                println!("[net] DNS: using {}:{port} (over TCP)", Ip(r.addr));
                NetReply::default()
            }
            req::STATS => self.totals.reply(),
            req::QUIT => {
                self.quit = true;
                NetReply::default()
            }
            // Already attached: there is one device and it is ours.
            req::ATTACH => NetReply::error(Error::Busy),
            _ => NetReply::error(Error::Invalid),
        }
    }

    // ---- sessions ----------------------------------------------------------------

    fn serve_sessions(&mut self) -> bool {
        let mut did = false;
        let mut i = 0;
        while i < self.sessions.len() {
            if self.sessions[i].busy {
                i += 1;
                continue;
            }
            let chan = self.sessions[i].chan;
            let mut buf = [0u8; MSG_MAX];
            match sys::recv(chan, &mut buf, true) {
                Ok((n, carried)) => {
                    did = true;
                    let reply = match NetRequest::from_bytes(&buf[..n]) {
                        Some(r) if r.abi_version == ABI_VERSION => self.session_request(i, &r, carried),
                        _ => {
                            close(carried);
                            Some(NetReply::error(Error::Invalid))
                        }
                    };
                    if let Some(rep) = reply {
                        send_reply(chan, &rep);
                    }
                    i += 1;
                }
                Err(Error::WouldBlock) => i += 1,
                Err(_) => {
                    // The client let go of its session. Its connections are channels
                    // of their own and carry on.
                    let s = self.sessions.remove(i);
                    sys::handle_close(s.chan).ok();
                    println!(
                        "[net] session {} closed: {} opened, {} refused, {} resolved",
                        s.id, s.counters.opened, s.counters.refused, s.counters.resolved
                    );
                    did = true;
                }
            }
        }
        did
    }

    fn refuse(&mut self, i: usize, what: &str, host: &str, port: Option<u16>) {
        let s = &mut self.sessions[i];
        s.counters.refused += 1;
        self.totals.refused += 1;
        match port {
            Some(p) => {
                println!("[net] session {}: refused to {what} {host} port {p}: not on its allowlist", s.id)
            }
            None => println!("[net] session {}: refused to {what} {host}: not on its allowlist", s.id),
        }
    }

    /// Handle one request; `None` when the answer comes later.
    fn session_request(&mut self, i: usize, r: &NetRequest, carried: Option<Handle>) -> Option<NetReply> {
        if r.kind != req::CONNECT {
            close(carried);
        }
        let sid = self.sessions[i].id;
        match r.kind {
            req::HELLO => Some(self.hello_reply()),
            req::STATS => Some(self.sessions[i].counters.reply()),
            req::RESOLVE => {
                let Some(host) = r.host() else { return Some(NetReply::error(Error::Invalid)) };
                if !self.sessions[i].may_resolve(host) {
                    self.refuse(i, "look up", host, None);
                    return Some(NetReply::error(Error::Denied));
                }
                if let Some(a) = abi::parse_ipv4(host) {
                    return Some(NetReply { addr: a, ..Default::default() });
                }
                if !dns::valid_name(host) {
                    return Some(NetReply::error(Error::Invalid));
                }
                let ms = if r.timeout_ms == 0 { RESOLVE_DEFAULT_MS } else { r.timeout_ms as u64 };
                match self.start_query(host, Waiter::Session(sid), now_ms() + ms) {
                    Ok(()) => {
                        self.sessions[i].busy = true;
                        None
                    }
                    Err(e) => Some(NetReply::error(e)),
                }
            }
            req::CONNECT => {
                let Some(chan) = carried else { return Some(NetReply::error(Error::Invalid)) };
                match self.open_conn(i, r, chan) {
                    Ok(id) => Some(NetReply { value: id as u64, ..Default::default() }),
                    Err(e) => {
                        sys::handle_close(chan).ok();
                        Some(NetReply::error(e))
                    }
                }
            }
            _ => Some(NetReply::error(Error::Invalid)),
        }
    }

    /// Can a TCP connection go to `a` at all? Not to "this host" or loopback, not
    /// to a multicast or reserved address, not to a broadcast address, not to
    /// ourselves -- whether the client named the address or a DNS answer did.
    fn tcp_destination(&self, a: [u8; 4]) -> bool {
        let special = a[0] == 0 || a[0] == 127 || a[0] >= 224;
        let ours = self.config.is_some_and(|c| {
            let mask = if c.prefix == 0 { 0 } else { u32::MAX << (32 - c.prefix as u32) };
            let addr = u32::from_be_bytes(a);
            a == c.addr
                || (c.prefix < 31
                    && addr & mask == u32::from_be_bytes(c.addr) & mask
                    && addr | mask == u32::MAX)
        });
        !special && !ours
    }

    /// Check a CONNECT and start it. The outcome of the attempt itself arrives on
    /// the socket channel.
    fn open_conn(&mut self, i: usize, r: &NetRequest, chan: Handle) -> Result<u32, Error> {
        let usable = sys::handle_info(chan).is_ok_and(|info| {
            info.kind == kind::CHANNEL
                && info.rights & (rights::SEND | rights::RECV) == rights::SEND | rights::RECV
        });
        if !usable || r.port == 0 {
            return Err(Error::Invalid);
        }
        // The destination as the client named it: that text is what the allowlist
        // is checked against, before anything is looked up or sent.
        let (host, literal) = if r.host_len == 0 {
            let mut b = [0u8; 15];
            (String::from(abi::fmt_ipv4(r.addr, &mut b)), Some(r.addr))
        } else {
            let h = r.host().ok_or(Error::Invalid)?;
            (String::from(h), abi::parse_ipv4(h))
        };
        match literal {
            Some(a) if !self.tcp_destination(a) => return Err(Error::Invalid),
            None if !dns::valid_name(&host) => return Err(Error::Invalid),
            _ => {}
        }
        if !self.sessions[i].may_connect(&host, r.port) {
            self.refuse(i, "connect to", &host, Some(r.port));
            return Err(Error::Denied);
        }
        let sid = self.sessions[i].id;
        if self.conns.len() >= SOCKETS_MAX
            || self.conns.iter().filter(|c| c.session == sid).count() >= SOCKETS_PER_SESSION
        {
            return Err(Error::Busy);
        }
        if self.config.is_none() {
            return Err(Error::Unreachable);
        }
        let id = self.fresh_id();
        let now = now_ms();
        let ms = if r.timeout_ms == 0 { CONNECT_DEFAULT_MS } else { r.timeout_ms as u64 };
        self.conns.push(Conn {
            id,
            session: self.sessions[i].id,
            chan,
            label: format!("{host}:{}", r.port),
            port: r.port,
            remote: [0; 4],
            phase: Phase::Resolving,
            tcp: None,
            started: now,
            deadline: now + ms,
            bytes_in: 0,
            bytes_out: 0,
            eof_sent: false,
            shut: false,
            outbox: None,
            push_blocked: false,
            push_retry_ms: PUSH_RETRY_FIRST_MS,
            client_gone: false,
            linger_until: 0,
            over: false,
        });
        let ci = self.conns.len() - 1;
        match literal {
            Some(a) => self.start_tcp(ci, a),
            None => {
                if let Err(e) = self.start_query(&host, Waiter::Conn(id), now + ms) {
                    self.fail(ci, e);
                }
            }
        }
        Ok(id)
    }

    // ---- lookups -----------------------------------------------------------------

    fn start_query(&mut self, name: &str, waiter: Waiter, deadline: u64) -> Result<(), Error> {
        if self.config.is_none() {
            return Err(Error::Unreachable);
        }
        let (server, port) = self.dns_server().ok_or(Error::Unreachable)?;
        let local = self.fresh_port();
        self.next_query = self.next_query.wrapping_add(1);
        let q = Query::start(
            &mut self.sockets,
            &mut self.iface,
            (Ipv4Address::from(server), port),
            local,
            self.next_query,
            name,
            waiter,
            deadline,
        )?;
        self.queries.push(q);
        Ok(())
    }

    fn advance_queries(&mut self) -> bool {
        let now = now_ms();
        let mut did = false;
        let mut i = 0;
        while i < self.queries.len() {
            let Some(result) = self.queries[i].advance(&mut self.sockets, now) else {
                i += 1;
                continue;
            };
            let q = self.queries.remove(i);
            self.release(q.sock);
            did = true;
            match &result {
                Ok(a) => println!("[net] DNS: {} is {} ({} ms)", q.name, Ip(*a), now - q.started),
                Err(e) => println!("[net] DNS: {}: {e}", q.name),
            }
            match q.waiter {
                Waiter::Session(sid) => {
                    let Some(si) = self.sessions.iter().position(|s| s.id == sid) else { continue };
                    self.sessions[si].busy = false;
                    let rep = match result {
                        Ok(a) => {
                            self.sessions[si].counters.resolved += 1;
                            self.totals.resolved += 1;
                            NetReply { addr: a, ..Default::default() }
                        }
                        Err(e) => NetReply::error(e),
                    };
                    send_reply(self.sessions[si].chan, &rep);
                }
                Waiter::Conn(cid) => {
                    let Some(ci) = self.conns.iter().position(|c| c.id == cid) else { continue };
                    match result {
                        Ok(a) => {
                            let sid = self.conns[ci].session;
                            if let Some(s) = self.sessions.iter_mut().find(|s| s.id == sid) {
                                s.counters.resolved += 1;
                            }
                            self.totals.resolved += 1;
                            if self.tcp_destination(a) {
                                self.start_tcp(ci, a);
                            } else {
                                println!("[net] DNS: {} names {}, where no connection may go", q.name, Ip(a));
                                self.fail(ci, Error::Invalid);
                            }
                        }
                        Err(e) => self.fail(ci, e),
                    }
                }
            }
        }
        did
    }

    // ---- connections -------------------------------------------------------------

    fn start_tcp(&mut self, ci: usize, addr: [u8; 4]) {
        let local = self.fresh_port();
        let mut s = tcp::Socket::new(
            tcp::SocketBuffer::new(vec![0u8; TCP_RX]),
            tcp::SocketBuffer::new(vec![0u8; TCP_TX]),
        );
        // The client's messages are coalesced in the send buffer before each
        // transmit, so there is nothing for Nagle's algorithm to add but delay. And
        // acknowledge at once: a delayed ACK can be lost for good when the socket is
        // let go of first (and the peer then resends into the void).
        s.set_nagle_enabled(false);
        s.set_ack_delay(None);
        let port = self.conns[ci].port;
        match s.connect(self.iface.context(), (IpAddress::Ipv4(Ipv4Address::from(addr)), port), local) {
            Ok(()) => {
                let h = self.sockets.add(s);
                let c = &mut self.conns[ci];
                c.tcp = Some(h);
                c.remote = addr;
                c.phase = Phase::Connecting;
            }
            Err(_) => self.fail(ci, Error::Unreachable),
        }
    }

    /// Queue a control message for the client, behind nothing and ahead of
    /// everything that follows.
    fn control(&mut self, ci: usize, msg: &[u8]) {
        let c = &mut self.conns[ci];
        if c.client_gone {
            return;
        }
        match sys::send(c.chan, msg, None) {
            Ok(()) => {}
            Err(Error::WouldBlock) => {
                let mut m = [0u8; 8];
                m[..msg.len()].copy_from_slice(msg);
                c.outbox = Some((m, msg.len()));
            }
            Err(_) => {
                c.client_gone = true;
                c.linger_until = now_ms() + LINGER_MS;
            }
        }
    }

    /// The connection failed or broke: say why, reset it, and finish.
    fn fail(&mut self, ci: usize, e: Error) {
        let now = now_ms();
        let c = &mut self.conns[ci];
        println!("[net] session {}: {}: {e} after {} ms", c.session, c.label, now - c.started);
        if let Some(h) = c.tcp.take() {
            self.abort(h);
        }
        let mut m = [0u8; 5];
        m[0] = sock::ERROR;
        m[1..5].copy_from_slice(&(e as u32).to_le_bytes());
        self.control(ci, &m);
        self.conns[ci].finish(now);
    }

    /// The client closed its end of the socket channel. Like closing a socket:
    /// with unread data the connection is reset, otherwise what is queued still goes
    /// out, then FIN.
    fn client_left(&mut self, ci: usize, now: u64) {
        let c = &mut self.conns[ci];
        if c.client_gone {
            return;
        }
        c.client_gone = true;
        c.outbox = None;
        c.linger_until = now + LINGER_MS;
        let Some(h) = c.tcp else {
            c.finish(now);
            return;
        };
        let s = self.sockets.get_mut::<tcp::Socket>(h);
        if c.phase != Phase::Open || s.recv_queue() > 0 {
            c.tcp = None;
            c.finish(now);
            self.abort(h);
        } else {
            s.close();
        }
    }

    fn advance_conns(&mut self) -> bool {
        let now = now_ms();
        let mut did = false;
        let mut i = 0;
        while i < self.conns.len() {
            did |= self.advance_conn(i, now);
            let c = &self.conns[i];
            // Finished, and the last word delivered -- or the client did not make
            // room for it in time.
            if c.over && (c.outbox.is_none() || c.client_gone || now >= c.linger_until) {
                let c = self.conns.remove(i);
                if let Some(h) = c.tcp {
                    self.release(h);
                }
                sys::handle_close(c.chan).ok();
                did = true;
            } else {
                i += 1;
            }
        }
        did
    }

    fn advance_conn(&mut self, ci: usize, now: u64) -> bool {
        let mut did = false;
        if let Some((m, n)) = self.conns[ci].outbox {
            match sys::send(self.conns[ci].chan, &m[..n], None) {
                Ok(()) => {
                    self.conns[ci].outbox = None;
                    did = true;
                }
                Err(Error::WouldBlock) => {}
                Err(_) => {
                    self.client_left(ci, now);
                    return true;
                }
            }
        }
        if self.conns[ci].over {
            return did;
        }
        match self.conns[ci].phase {
            // The lookup finishes it, one way or the other, by the deadline.
            Phase::Resolving => did,
            Phase::Connecting => self.check_connect(ci, now) || did,
            Phase::Open => self.pump(ci, now) || did,
        }
    }

    fn check_connect(&mut self, ci: usize, now: u64) -> bool {
        let Some(h) = self.conns[ci].tcp else { return false };
        match self.sockets.get::<tcp::Socket>(h).state() {
            tcp::State::SynSent | tcp::State::SynReceived => {
                if now >= self.conns[ci].deadline {
                    self.fail(ci, Error::TimedOut);
                    return true;
                }
                false
            }
            // Straight from SYN-SENT to CLOSED: the answer to our SYN was a reset.
            tcp::State::Closed => {
                self.fail(ci, Error::Refused);
                true
            }
            _ => {
                let c = &mut self.conns[ci];
                c.phase = Phase::Open;
                self.totals.opened += 1;
                let sid = c.session;
                if let Some(s) = self.sessions.iter_mut().find(|s| s.id == sid) {
                    s.counters.opened += 1;
                }
                println!(
                    "[net] session {}: connected to {} ({}) in {} ms",
                    c.session,
                    c.label,
                    Ip(c.remote),
                    now - c.started
                );
                let mut m = [0u8; 7];
                m[0] = sock::CONNECTED;
                m[1..5].copy_from_slice(&c.remote);
                m[5..7].copy_from_slice(&c.port.to_le_bytes());
                self.control(ci, &m);
                true
            }
        }
    }

    /// Move data both ways on an open connection and notice its end.
    fn pump(&mut self, ci: usize, now: u64) -> bool {
        let Some(h) = self.conns[ci].tcp else { return false };
        let mut did = false;
        let chan = self.conns[ci].chan;

        // Client -> network, only while the connection has room for a whole message.
        if !self.conns[ci].shut && !self.conns[ci].client_gone {
            for _ in 0..64 {
                let s = self.sockets.get_mut::<tcp::Socket>(h);
                if !s.may_send() || s.send_capacity() - s.send_queue() < DATA_MAX {
                    break;
                }
                let mut m = [0u8; MSG_MAX];
                match sys::recv(chan, &mut m, true) {
                    Ok((n, carried)) => {
                        close(carried);
                        did = true;
                        match (n, m[0]) {
                            (2.., sock::DATA) => {
                                let _ = s.send_slice(&m[1..n]);
                                self.conns[ci].bytes_out += (n - 1) as u64;
                            }
                            (1, sock::SHUTDOWN) => {
                                s.close();
                                self.conns[ci].shut = true;
                                break;
                            }
                            _ => {
                                println!(
                                    "[net] session {}: {}: malformed socket message (tag {:#x}, {n} bytes)",
                                    self.conns[ci].session, self.conns[ci].label, m[0]
                                );
                                self.fail(ci, Error::Invalid);
                                return true;
                            }
                        }
                    }
                    Err(Error::WouldBlock) => break,
                    Err(_) => {
                        self.client_left(ci, now);
                        return true;
                    }
                }
            }
        }

        // Network -> client, as fast as the client's queue takes it.
        self.conns[ci].push_blocked = false;
        if self.conns[ci].outbox.is_none() && !self.conns[ci].client_gone {
            let s = self.sockets.get_mut::<tcp::Socket>(h);
            let mut gone = false;
            let mut blocked = false;
            let mut moved = 0u64;
            while s.can_recv() {
                let r = s.recv(|d| {
                    let n = d.len().min(DATA_MAX);
                    let mut m = [0u8; MSG_MAX];
                    m[0] = sock::RECEIVED;
                    m[1..1 + n].copy_from_slice(&d[..n]);
                    match sys::send(chan, &m[..1 + n], None) {
                        Ok(()) => (n, Ok(n)),
                        Err(e) => (0, Err(e)),
                    }
                });
                match r {
                    Ok(Ok(n)) => moved += n as u64,
                    Ok(Err(Error::WouldBlock)) => {
                        blocked = true;
                        break;
                    }
                    Ok(Err(_)) => {
                        gone = true;
                        break;
                    }
                    Err(_) => break,
                }
            }
            let c = &mut self.conns[ci];
            c.bytes_in += moved;
            if blocked {
                // Blocked again without having moved anything: back off.
                if c.push_blocked && moved == 0 {
                    c.push_retry_ms = (c.push_retry_ms * 2).min(PUSH_RETRY_LAST_MS);
                }
            } else {
                c.push_retry_ms = PUSH_RETRY_FIRST_MS;
            }
            c.push_blocked = blocked;
            did |= moved > 0;
            if gone {
                self.client_left(ci, now);
                return true;
            }
        }

        // The end of the peer's stream, or of the connection.
        let c = &mut self.conns[ci];
        let s = self.sockets.get_mut::<tcp::Socket>(h);
        if c.outbox.is_none() && !c.client_gone && !c.eof_sent && !s.can_recv() {
            match s.recv(|_| (0, ())) {
                Err(tcp::RecvError::Finished) => {
                    c.eof_sent = true;
                    self.control(ci, &[sock::EOF]);
                    did = true;
                }
                // Closed without the peer having finished: it reset the connection.
                Err(tcp::RecvError::InvalidState) if s.state() == tcp::State::Closed => {
                    self.fail(ci, Error::Reset);
                    return true;
                }
                _ => {}
            }
        }
        let c = &mut self.conns[ci];
        let state = self.sockets.get::<tcp::Socket>(h).state();
        let closed = matches!(state, tcp::State::Closed | tcp::State::TimeWait);
        if c.client_gone {
            if closed || now >= c.linger_until {
                c.finish(now);
            }
        } else if c.eof_sent && closed {
            println!(
                "[net] session {}: {} closed: {} bytes out, {} in",
                c.session, c.label, c.bytes_out, c.bytes_in
            );
            c.finish(now);
            did = true;
        }
        did
    }

    // ---- sockets let go of -------------------------------------------------------

    /// Hand a socket over to be closed cleanly (or forgotten, if it already is).
    fn release(&mut self, h: SocketHandle) {
        let s = self.sockets.get_mut::<tcp::Socket>(h);
        match s.state() {
            tcp::State::Closed => {
                self.sockets.remove(h);
            }
            // Not yet: the ACK of the peer's FIN may still be waiting to go out.
            tcp::State::TimeWait => self.graveyard.push(Grave {
                sock: h,
                until: 0,
                aborted_pass: None,
                time_wait_since: Some(now_ms()),
            }),
            _ => {
                s.close();
                self.graveyard.push(Grave {
                    sock: h,
                    until: now_ms() + LINGER_MS,
                    aborted_pass: None,
                    time_wait_since: None,
                });
            }
        }
        self.cap_graveyard();
    }

    /// Reset a socket; it is removed once the reset has been transmitted.
    fn abort(&mut self, h: SocketHandle) {
        self.sockets.get_mut::<tcp::Socket>(h).abort();
        self.graveyard.push(Grave {
            sock: h,
            until: 0,
            aborted_pass: Some(self.pass),
            time_wait_since: None,
        });
    }

    /// Keep the sockets let go of within [`GRAVEYARD_MAX`]. The oldest one in
    /// TIME-WAIT goes first, quietly: its last ACK left the moment the peer's FIN
    /// came in (ACKs are not delayed), so nothing is lost but the courtesy of
    /// answering a repeated FIN. Only when none is in TIME-WAIT is a connection
    /// still closing reset.
    fn cap_graveyard(&mut self) {
        let lingering = self.graveyard.iter().filter(|g| g.aborted_pass.is_none()).count();
        if lingering <= GRAVEYARD_MAX {
            return;
        }
        let sockets = &mut self.sockets;
        let in_time_wait =
            self.graveyard.iter().position(|g| g.aborted_pass.is_none() && g.time_wait_since.is_some());
        if let Some(i) = in_time_wait {
            let g = self.graveyard.remove(i);
            sockets.remove(g.sock);
            return;
        }
        let pass = self.pass;
        if let Some(g) = self.graveyard.iter_mut().find(|g| g.aborted_pass.is_none()) {
            sockets.get_mut::<tcp::Socket>(g.sock).abort();
            g.aborted_pass = Some(pass);
        }
    }

    fn tend_graveyard(&mut self) {
        let now = now_ms();
        let pass = self.pass;
        let sockets = &mut self.sockets;
        self.graveyard.retain_mut(|g| match g.aborted_pass {
            Some(p) if p >= pass => true,
            Some(_) => {
                sockets.remove(g.sock);
                false
            }
            None => {
                let s = sockets.get_mut::<tcp::Socket>(g.sock);
                match s.state() {
                    tcp::State::Closed => {
                        sockets.remove(g.sock);
                        false
                    }
                    tcp::State::TimeWait => {
                        let since = *g.time_wait_since.get_or_insert(now);
                        if now >= since + TIME_WAIT_MS {
                            sockets.remove(g.sock);
                            false
                        } else {
                            true
                        }
                    }
                    _ => {
                        if now >= g.until {
                            s.abort();
                            g.aborted_pass = Some(pass);
                        }
                        true
                    }
                }
            }
        });
    }

    // ---- sleeping ----------------------------------------------------------------

    /// What to wait for, and for how long at most.
    fn wait_set(&mut self) -> ([Handle; WAIT_MAX], usize, u64) {
        let mut set = [0 as Handle; WAIT_MAX];
        let mut n = 0;
        let mut add = |h: Handle| {
            if n < WAIT_MAX {
                set[n] = h;
                n += 1;
            }
        };
        add(self.dev.nic);
        if self.attach_deadline.is_none() {
            add(self.op);
        }
        for s in self.sessions.iter().filter(|s| !s.busy) {
            add(s.chan);
        }
        let now = now_ms();
        let mut until = self.iface.poll_delay(instant(now), &self.sockets).map(|d| now + d.total_millis());
        let mut at = |t: u64| until = Some(until.map_or(t, |u| u.min(t)));
        if let Some(d) = self.attach_deadline {
            at(d);
        }
        for q in &self.queries {
            at(q.deadline);
        }
        for g in &self.graveyard {
            at(match (g.aborted_pass, g.time_wait_since) {
                (Some(_), _) => now,
                (None, Some(since)) => since + TIME_WAIT_MS,
                (None, None) => g.until,
            });
        }
        for c in &self.conns {
            if c.outbox.is_some() || c.push_blocked {
                at(now + c.push_retry_ms);
            }
            if c.over {
                at(c.linger_until);
            }
            if c.client_gone {
                at(c.linger_until);
            }
            match c.phase {
                Phase::Resolving | Phase::Connecting => at(c.deadline),
                Phase::Open => {
                    let room = c.tcp.is_some_and(|h| {
                        let s = self.sockets.get::<tcp::Socket>(h);
                        s.may_send() && s.send_capacity() - s.send_queue() >= DATA_MAX
                    });
                    if room && !c.shut && !c.client_gone && !c.over {
                        add(c.chan);
                    }
                }
            }
        }
        let timeout = until.map_or(WAIT_FOREVER, |u| u.saturating_sub(now));
        (set, n, timeout)
    }

    fn shutdown(&mut self) {
        // Reset what is still open; a connection already in TIME-WAIT has said
        // goodbye properly and gets no reset on top.
        let reset = |sockets: &mut SocketSet<'static>, h: SocketHandle| {
            let s = sockets.get_mut::<tcp::Socket>(h);
            if !matches!(s.state(), tcp::State::Closed | tcp::State::TimeWait) {
                s.abort();
            }
        };
        for c in core::mem::take(&mut self.conns) {
            if let Some(h) = c.tcp {
                reset(&mut self.sockets, h);
            }
            sys::handle_close(c.chan).ok();
        }
        for q in core::mem::take(&mut self.queries) {
            reset(&mut self.sockets, q.sock);
        }
        for g in &self.graveyard {
            reset(&mut self.sockets, g.sock);
        }
        // Let the resets, and the last acknowledgements, go out.
        let until = now_ms() + 20;
        loop {
            self.iface.poll(instant(now_ms()), &mut self.dev, &mut self.sockets);
            if now_ms() >= until {
                break;
            }
            sys::sleep_ms(2);
        }
        for s in core::mem::take(&mut self.sessions) {
            sys::handle_close(s.chan).ok();
        }
        println!(
            "[net] closing: {} connections opened, {} refused, {} names resolved; {} frames in, {} out, {} dropped",
            self.totals.opened,
            self.totals.refused,
            self.totals.resolved,
            self.dev.rx_frames,
            self.dev.tx_frames,
            self.dev.tx_dropped
        );
        sys::handle_close(self.dev.nic).ok();
    }
}
