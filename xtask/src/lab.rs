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
use std::time::Duration;

/// Address, port and service name of every lab service.
pub const SERVICES: &[(&str, u16, &str)] = &[
    // DNS over TCP for the lab zone below.
    ("10.0.2.53", 53, "dns"),
    // Echoes every byte back until the guest finishes sending, then finishes too.
    ("10.0.2.101", 7, "echo"),
    // Leaves what it is sent unread and goes away: the guest sees a reset.
    ("10.0.2.102", 9, "reset"),
];

/// The zone the lab DNS server answers for. Nothing answers at 10.0.2.77 (not even
/// ARP): a connection there is never answered, which is how a timeout is tested.
const ZONE: &[(&str, [u8; 4])] = &[
    ("echo.lab.test", [10, 0, 2, 101]),
    // The echo service's address, used on a port nothing is forwarded to: QEMU
    // answers the SYN with a reset.
    ("closed.lab.test", [10, 0, 2, 101]),
    ("reset.lab.test", [10, 0, 2, 102]),
    ("blackhole.lab.test", [10, 0, 2, 77]),
    ("api.cloud.test", [10, 0, 2, 100]),
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

fn log(line: &str) {
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

fn echo() -> io::Result<()> {
    let mut input = io::stdin().lock();
    let mut output = io::stdout().lock();
    let mut buf = [0u8; 4096];
    let mut total = 0u64;
    loop {
        let n = input.read(&mut buf)?;
        if n == 0 {
            log(&format!("echo: {total} bytes echoed; the guest finished sending"));
            return Ok(());
        }
        output.write_all(&buf[..n])?;
        output.flush()?;
        total += n as u64;
    }
}

fn reset() -> io::Result<()> {
    // Whatever the guest sends in this time stays unread; leaving with unread data
    // makes the socket report a reset, which QEMU passes on to the guest as RST.
    std::thread::sleep(Duration::from_millis(300));
    log("reset: leaving with the guest's data unread");
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
    fn netdev_escapes_commas() {
        assert_eq!(shell_quote("/a/b-c/x.y"), "/a/b-c/x.y");
        assert_eq!(shell_quote("/a b/it's"), "'/a b/it'\\''s'");
    }
}
