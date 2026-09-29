//! Network service checks: `spacenet` started, handed the device, and used through
//! a session -- against the lab services the build tool provides (xtask/src/lab.rs):
//! DNS at 10.0.2.53, an echo service at 10.0.2.101:7, a service that resets its
//! connections at 10.0.2.102:9, and nothing at all at 10.0.2.77.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use libspace::net::{Session, TcpStream};
use libspace::spaceabi::error::Error;
use libspace::spaceabi::handle::rights;
use libspace::spaceabi::net::{ANY_PORT, NetReply, NetRequest, req};
use libspace::{Handle, kill_reason, println, sys};

/// Pages for the service: its heap is 128 of them (socket buffers).
pub const SERVICE_QUOTA: u64 = 256;
const LAB_DNS: [u8; 4] = [10, 0, 2, 53];
const ECHO_ADDR: [u8; 4] = [10, 0, 2, 101];

/// What the test session may reach. Deliberately absent: api.cloud.test, any other
/// port of the echo service, and every address written as an address.
const ALLOWLIST: &[(&str, u16)] = &[
    ("echo.lab.test", 7),
    ("alias.lab.test", 7),
    ("reset.lab.test", 9),
    ("blackhole.lab.test", 80),
    ("closed.lab.test", 8),
    ("nosuch.lab.test", ANY_PORT),
];

/// The running service and what the tests hold of it.
pub struct Lab {
    pub process: Handle,
    pub op: Handle,
    pub session: Session,
    /// A read-only duplicate of the lease: the device's own counters, which the
    /// service cannot fake.
    pub watch: Handle,
}

impl Lab {
    /// Frames the device has transmitted so far.
    pub fn frames_sent(&self) -> Result<u64, String> {
        sys::net_info(self.watch).map(|i| i.tx_frames).map_err(|e| format!("net_info: {e}"))
    }

    /// Let go of everything held; the service itself is left running.
    pub fn release(self) {
        for h in [self.session.handle(), self.op, self.watch, self.process] {
            sys::handle_close(h).ok();
        }
    }
}

fn op_call(op: Handle, r: &NetRequest, carry: Option<Handle>, timeout_ms: u64) -> Result<NetReply, String> {
    sys::send(op, r.as_bytes(), carry).map_err(|e| format!("send request {}: {e}", r.kind))?;
    sys::wait_any(&[op], timeout_ms).map_err(|e| format!("request {}: no answer: {e}", r.kind))?;
    let mut buf = [0u8; core::mem::size_of::<NetReply>()];
    let (n, _) = sys::recv(op, &mut buf, true).map_err(|e| format!("request {}: {e}", r.kind))?;
    let reply =
        NetReply::from_bytes(&buf[..n]).ok_or_else(|| format!("request {}: a {n}-byte answer", r.kind))?;
    reply.result().map_err(|e| format!("request {}: {e}", r.kind))?;
    Ok(reply)
}

fn ip(a: [u8; 4]) -> String {
    format!("{}.{}.{}.{}", a[0], a[1], a[2], a[3])
}

/// Start the service, hand it the device lease, point it at the lab DNS server and
/// open one session with [`ALLOWLIST`].
pub fn start(root: Handle) -> Result<Lab, String> {
    let (op, theirs) = sys::channel_create().map_err(|e| format!("channel: {e}"))?;
    let process = sys::spawn(root, "bin/spacenet", SERVICE_QUOTA, Some(theirs)).map_err(|e| {
        sys::handle_close(op).ok();
        format!("spawn: {e}")
    })?;
    let lab = (|| {
        let nic = sys::net_open(root).map_err(|e| format!("lease: {e}"))?;
        let watch = sys::handle_dup(nic, rights::READ).map_err(|e| format!("dup lease: {e}"))?;
        let mut attach = NetRequest::new(req::ATTACH);
        attach.timeout_ms = 5000;
        let t0 = sys::ticks_ms();
        let cfg = match op_call(op, &attach, Some(nic), 8000) {
            Ok(c) => c,
            Err(e) => {
                sys::handle_close(watch).ok();
                return Err(e);
            }
        };
        println!(
            "[init] network service: {}/{} via {}, DNS {}, configured in {} ms",
            ip(cfg.addr),
            cfg.prefix,
            ip(cfg.gateway),
            ip(cfg.dns),
            sys::ticks_ms() - t0
        );
        let fail = |e: String| {
            sys::handle_close(watch).ok();
            e
        };
        // QEMU's restricted network leases the address alone: no router and no DNS
        // server, since neither would lead anywhere.
        if (cfg.addr, cfg.prefix, cfg.gateway, cfg.dns) != ([10, 0, 2, 15], 24, [0; 4], [0; 4]) {
            return Err(fail(String::from(
                "DHCP gave something other than 10.0.2.15/24 with no router and no DNS server",
            )));
        }
        op_call(op, &NetRequest::with_addr(req::SET_DNS, LAB_DNS, 53), None, 2000).map_err(fail)?;
        let (mine, theirs) = sys::channel_create().map_err(|e| fail(format!("channel: {e}")))?;
        op_call(op, &NetRequest::new(req::SESSION), Some(theirs), 2000).map_err(|e| {
            sys::handle_close(mine).ok();
            fail(e)
        })?;
        for (host, port) in ALLOWLIST {
            op_call(op, &NetRequest::with_host(req::ALLOW, host, *port), None, 2000).map_err(|e| {
                sys::handle_close(mine).ok();
                fail(e)
            })?;
        }
        Ok(Lab { process, op, session: Session::new(mine), watch })
    })();
    if lab.is_err() {
        sys::kill(process).ok();
        sys::wait(process).ok();
        sys::handle_close(process).ok();
        sys::handle_close(op).ok();
    }
    lab
}

pub fn hello(lab: &Lab) -> Result<(), String> {
    let h = lab.session.hello().map_err(|e| format!("HELLO: {e}"))?;
    let mac = sys::net_info(lab.watch).map_err(|e| format!("net_info: {e}"))?.mac;
    if (h.addr, h.prefix, h.gateway, h.dns) != ([10, 0, 2, 15], 24, [0; 4], LAB_DNS) {
        return Err(format!(
            "HELLO says {}/{} via {}, DNS {}",
            ip(h.addr),
            h.prefix,
            ip(h.gateway),
            ip(h.dns)
        ));
    }
    if h.mac != mac {
        return Err(String::from("HELLO names a MAC address that is not the device's"));
    }
    Ok(())
}

pub fn resolve(lab: &Lab) -> Result<(), String> {
    let s = &lab.session;
    for (name, want) in
        [("echo.lab.test", ECHO_ADDR), ("alias.lab.test", ECHO_ADDR), ("ECHO.Lab.Test", ECHO_ADDR)]
    {
        let t0 = sys::ticks_ms();
        let got = s.resolve(name, 3000).map_err(|e| format!("{name}: {e}"))?;
        println!("[init] network: {name} is {} ({} ms)", ip(got), sys::ticks_ms() - t0);
        if got != want {
            return Err(format!("{name} resolved to {}, expected {}", ip(got), ip(want)));
        }
    }
    match s.resolve("nosuch.lab.test", 3000) {
        Err(Error::NotFound) => {}
        other => return Err(format!("nosuch.lab.test gave {other:?}, expected NotFound")),
    }
    Ok(())
}

/// Byte `i` of the test stream: no short period, so a dropped, doubled or
/// reordered piece shows. Computed, never stored: init's heap is 128 KiB.
fn pattern_byte(i: usize) -> u8 {
    (i as u32).wrapping_mul(2_654_435_761).rotate_left(7) as u8 ^ (i >> 8) as u8
}

/// Check `data` as the continuation of the stream after `*got` bytes.
fn check_echo(got: &mut usize, data: &[u8]) -> Result<(), String> {
    if let Some(k) = data.iter().enumerate().position(|(k, b)| *b != pattern_byte(*got + k)) {
        return Err(format!("the echo differs from byte {} on", *got + k));
    }
    *got += data.len();
    Ok(())
}

pub fn echo_large(lab: &Lab) -> Result<(), String> {
    const LEN: usize = 64 * 1024;
    let t0 = sys::ticks_ms();
    let mut s = lab.session.connect("echo.lab.test", 7, 3000).map_err(|e| format!("connect: {e}"))?;
    if s.remote != (ECHO_ADDR, 7) {
        return Err(format!("connected to {}:{}", ip(s.remote.0), s.remote.1));
    }
    let (mut sent, mut got) = (0usize, 0usize);
    let mut out = [0u8; 1024];
    let mut buf = [0u8; 512];
    // Interleave writing and reading: the echo comes back while we send, and a
    // client that only wrote would fill every buffer on the way and stall.
    while sent < LEN {
        let n = (LEN - sent).min(out.len());
        for (k, b) in out[..n].iter_mut().enumerate() {
            *b = pattern_byte(sent + k);
        }
        s.write_all(&out[..n], 5000).map_err(|e| format!("write after {sent} bytes: {e}"))?;
        sent += n;
        while let Some(n) = s.try_read(&mut buf).map_err(|e| format!("read after {got} bytes: {e}"))? {
            if n == 0 {
                return Err(format!("end of stream after {got} of {LEN} bytes"));
            }
            check_echo(&mut got, &buf[..n])?;
        }
    }
    s.shutdown(5000).map_err(|e| format!("shutdown: {e}"))?;
    loop {
        match s.read(&mut buf, 10_000).map_err(|e| format!("read after {got} bytes: {e}"))? {
            0 => break,
            n => check_echo(&mut got, &buf[..n])?,
        }
    }
    let ms = sys::ticks_ms() - t0;
    println!("[init] network: {LEN} bytes to the echo service and back in {ms} ms");
    if got != LEN {
        return Err(format!("{got} bytes came back, {LEN} were sent"));
    }
    Ok(())
}

pub fn refused_before_sending(lab: &Lab) -> Result<(), String> {
    let s = &lab.session;
    // Let the previous tests' last segments go out before counting.
    sys::sleep_ms(20);
    let (_, refused_before, _) = s.stats().map_err(|e| format!("stats: {e}"))?;
    let sent_before = lab.frames_sent()?;
    let tries: [(&str, u16, &str); 3] = [
        ("api.cloud.test", 80, "a name that is not on the list"),
        ("echo.lab.test", 8, "an allowed name on another port"),
        ("10.0.2.101", 7, "the allowed name's address, written as an address"),
    ];
    for (host, port, what) in tries {
        match s.connect(host, port, 1000) {
            Err(Error::Denied) => {}
            Ok(_) => return Err(format!("{what} ({host}:{port}) was let through")),
            Err(e) => return Err(format!("{what} ({host}:{port}): {e}, expected Denied")),
        }
    }
    match s.resolve("api.cloud.test", 1000) {
        Err(Error::Denied) => {}
        other => return Err(format!("looking up a name not on the list gave {other:?}")),
    }
    let sent_after = lab.frames_sent()?;
    let (_, refused_after, _) = s.stats().map_err(|e| format!("stats: {e}"))?;
    println!(
        "[init] network: 4 refusals counted ({refused_before} -> {refused_after}); frames sent meanwhile: {}",
        sent_after - sent_before
    );
    if sent_after != sent_before {
        return Err(format!("{} frame(s) left the device while refusing", sent_after - sent_before));
    }
    if refused_after != refused_before + 4 {
        return Err(format!("refusals counted {refused_before} -> {refused_after}, expected +4"));
    }
    Ok(())
}

pub fn timeout(lab: &Lab) -> Result<(), String> {
    let t0 = sys::ticks_ms();
    let r = lab.session.connect("blackhole.lab.test", 80, 500);
    let ms = sys::ticks_ms() - t0;
    println!("[init] network: blackhole.lab.test:80 gave {:?} after {ms} ms", r.as_ref().map(|_| ()));
    match r {
        Err(Error::TimedOut) if (400..3000).contains(&ms) => Ok(()),
        Err(Error::TimedOut) => Err(format!("timed out after {ms} ms, for a 500 ms deadline")),
        Ok(_) => Err(String::from("a connection to an address nobody answers succeeded")),
        Err(e) => Err(format!("{e}, expected TimedOut")),
    }
}

pub fn refused(lab: &Lab) -> Result<(), String> {
    let t0 = sys::ticks_ms();
    let r = lab.session.connect("closed.lab.test", 8, 3000);
    let ms = sys::ticks_ms() - t0;
    println!("[init] network: closed.lab.test:8 gave {:?} after {ms} ms", r.as_ref().map(|_| ()));
    match r {
        Err(Error::Refused) => Ok(()),
        Ok(_) => Err(String::from("a port nothing listens on accepted the connection")),
        Err(e) => Err(format!("{e}, expected Refused")),
    }
}

pub fn reset(lab: &Lab) -> Result<(), String> {
    let mut s = lab.session.connect("reset.lab.test", 9, 3000).map_err(|e| format!("connect: {e}"))?;
    s.write_all(b"is anyone reading this?", 1000).map_err(|e| format!("write: {e}"))?;
    let mut buf = [0u8; 64];
    let t0 = sys::ticks_ms();
    let r = s.read(&mut buf, 5000);
    println!("[init] network: the resetting service answered {:?} after {} ms", r, sys::ticks_ms() - t0);
    match r {
        Err(Error::Reset) => Ok(()),
        other => Err(format!("read gave {other:?}, expected Reset")),
    }
}

/// One short connection: connect, send, finish, read the echo to its end.
pub fn ping(lab: &Lab) -> Result<(), String> {
    let mut s: TcpStream =
        lab.session.connect("echo.lab.test", 7, 3000).map_err(|e| format!("connect: {e}"))?;
    s.write_all(b"ping", 1000).map_err(|e| format!("write: {e}"))?;
    s.shutdown(5000).map_err(|e| format!("shutdown: {e}"))?;
    let mut got = Vec::new();
    s.read_to_end(&mut got, 5000).map_err(|e| format!("read: {e}"))?;
    if got != b"ping" {
        return Err(format!("{} bytes came back instead of \"ping\"", got.len()));
    }
    Ok(())
}

/// Kill the service with a connection open: the client hears about it, the device
/// lease comes back, and a new instance starts, serves and quits cleanly.
pub fn kill_and_restart(root: Handle, lab: Lab) -> Result<(), String> {
    let mut s = lab.session.connect("echo.lab.test", 7, 3000).map_err(|e| format!("connect: {e}"))?;
    sys::kill(lab.process).map_err(|e| format!("kill: {e}"))?;
    let st = sys::wait(lab.process).map_err(|e| format!("wait: {e}"))?;
    if !st.is_killed_by(kill_reason::SIGNAL) {
        return Err(format!("the service ended with {st:?}"));
    }
    let mut buf = [0u8; 16];
    match s.read(&mut buf, 2000) {
        Err(Error::PeerClosed) => {}
        other => return Err(format!("reading from a connection of the killed service gave {other:?}")),
    }
    drop(s);
    match lab.session.hello() {
        Err(Error::PeerClosed) => {}
        other => return Err(format!("the session of the killed service answered {other:?}")),
    }
    lab.release();
    // Every handle on the lease is gone with the process: it can be taken again.
    let again = sys::net_open(root).map_err(|e| format!("lease after the kill: {e}"))?;
    sys::handle_close(again).ok();

    let lab = start(root)?;
    let served = ping(&lab);
    let quit = op_call(lab.op, &NetRequest::new(req::QUIT), None, 2000);
    let st = sys::wait(lab.process);
    lab.release();
    let lease_back = sys::net_open(root);
    served.map_err(|e| format!("the restarted service: {e}"))?;
    quit?;
    match st {
        Ok(st) if st.is_exited_with(0) => {}
        other => return Err(format!("after QUIT the service ended with {other:?}")),
    }
    let h = lease_back.map_err(|e| format!("lease after QUIT: {e}"))?;
    sys::handle_close(h).ok();
    Ok(())
}
