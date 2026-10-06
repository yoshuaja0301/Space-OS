//! What the guest's TCP looked like on the wire.
//!
//! The in-guest tests see the service's side: bytes arrive, streams end. A peer
//! can still be left hanging -- a FIN never acknowledged, data sent again and
//! again -- and nothing inside the guest would notice. This reads the capture
//! QEMU made of the lab network (`filter-dump`) and holds every connection to the
//! manners of TCP.

use std::collections::BTreeMap;
use std::path::Path;

/// One TCP connection, from the guest's point of view.
#[derive(Default)]
struct Flow {
    /// Sequence number just past the peer's FIN, when it sent one.
    peer_fin_end: Option<u32>,
    /// Highest acknowledgement the guest sent.
    guest_ack: Option<u32>,
    guest_fin: bool,
    reset: bool,
    /// Highest sequence number the peer had sent up to (end of data).
    peer_sent_end: Option<u32>,
    peer_retransmits: u32,
    handshake: bool,
    /// The guest's initial sequence number: a SYN with another one on the same
    /// ports is a new connection. A long run goes through every local port more
    /// than once.
    guest_isn: Option<u32>,
    /// When the guest last sent anything on this connection, and when the peer's
    /// first FIN came (microseconds, capture time).
    guest_last_us: Option<u64>,
    peer_fin_first_us: Option<u64>,
}

/// A connection's ports and the peer's address: the guest's port, the peer's
/// address, the peer's port.
type FlowKey = (u16, [u8; 4], u16);

pub struct TcpReport {
    pub connections: usize,
    pub closed_cleanly: usize,
    pub reset: usize,
    pub peer_retransmits: u32,
    /// Connections whose FIN from the peer the guest never acknowledged.
    pub unacked_fins: Vec<String>,
    /// Connections left behind by a network service that was killed: the guest had
    /// said nothing on them for [`ABANDONED_AFTER_US`] when the peer's FIN came, had
    /// not closed them, and never answered. Nothing is left in the guest to answer
    /// until another service starts, and a later one answers with a reset -- if the
    /// run lasts that long. Reported, not held against the guest.
    pub abandoned: Vec<String>,
}

/// How long the guest must have been silent on a connection it never closed before
/// an unanswered FIN is put down to a killed service rather than to its TCP. No
/// connection in these tests stays quiet this long while its service lives; the
/// lab's echo service waits 30 s for a guest that has gone quiet before it closes
/// (`lab::ECHO_IDLE`), so a connection whose service was killed mid-transfer ends
/// well past it.
const ABANDONED_AFTER_US: u64 = 20_000_000;

/// `a` is after `b` in sequence space.
fn seq_after(a: u32, b: u32) -> bool {
    (a.wrapping_sub(b) as i32) > 0
}

fn ip(b: &[u8]) -> String {
    format!("{}.{}.{}.{}", b[0], b[1], b[2], b[3])
}

pub fn analyze(path: &Path, guest: [u8; 4]) -> Result<TcpReport, String> {
    let raw = std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
    if raw.len() < 24 {
        return Err(format!("{}: not a capture", path.display()));
    }
    // Byte order, and whether the second half of a timestamp counts nanoseconds.
    let (le, nanos) = match &raw[..4] {
        [0xd4, 0xc3, 0xb2, 0xa1] => (true, false),
        [0x4d, 0x3c, 0xb2, 0xa1] => (true, true),
        [0xa1, 0xb2, 0xc3, 0xd4] => (false, false),
        [0xa1, 0xb2, 0x3c, 0x4d] => (false, true),
        _ => return Err(format!("{}: not a pcap file", path.display())),
    };
    let u32at = |b: &[u8], at: usize| {
        let w = [b[at], b[at + 1], b[at + 2], b[at + 3]];
        if le { u32::from_le_bytes(w) } else { u32::from_be_bytes(w) }
    };
    let be16 = |b: &[u8], at: usize| u16::from_be_bytes([b[at], b[at + 1]]);
    let be32 = |b: &[u8], at: usize| u32::from_be_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]]);
    let mut flows: BTreeMap<FlowKey, Flow> = BTreeMap::new();
    // Connections whose ports a later one took over.
    let mut finished: Vec<(FlowKey, Flow)> = Vec::new();
    let mut at = 24;
    while at + 16 <= raw.len() {
        let sub = u64::from(u32at(&raw, at + 4));
        let now_us = u64::from(u32at(&raw, at)) * 1_000_000 + if nanos { sub / 1000 } else { sub };
        let incl = u32at(&raw, at + 8) as usize;
        let start = at + 16;
        at = start + incl;
        if at > raw.len() {
            break; // a capture cut short by the guest's exit
        }
        let f = &raw[start..at];
        if f.len() < 14 + 20 || be16(f, 12) != 0x0800 {
            continue;
        }
        let ipk = &f[14..];
        let ihl = (ipk[0] & 0x0F) as usize * 4;
        let total = (be16(ipk, 2) as usize).min(ipk.len());
        if ipk[9] != 6 || total < ihl + 20 {
            continue;
        }
        let tcp = &ipk[ihl..total];
        let (src, dst) = (&ipk[12..16], &ipk[16..20]);
        let (sport, dport) = (be16(tcp, 0), be16(tcp, 2));
        let (seq, ack) = (be32(tcp, 4), be32(tcp, 8));
        let off = (tcp[12] >> 4) as usize * 4;
        let flags = tcp[13];
        let len = tcp.len().saturating_sub(off) as u32;
        let (fin, syn, rst, has_ack) = (flags & 1 != 0, flags & 2 != 0, flags & 4 != 0, flags & 16 != 0);
        let from_guest = src == guest;
        let key = if from_guest {
            (sport, [dst[0], dst[1], dst[2], dst[3]], dport)
        } else if dst == guest {
            (dport, [src[0], src[1], src[2], src[3]], sport)
        } else {
            continue;
        };
        if from_guest && syn && !has_ack {
            if let Some(old) = flows.get(&key)
                && old.guest_isn.is_some_and(|isn| isn != seq)
            {
                let old = flows.remove(&key).unwrap_or_default();
                finished.push((key, old));
            }
            flows.entry(key).or_default().guest_isn = Some(seq);
        }
        let flow = flows.entry(key).or_default();
        if rst {
            flow.reset = true;
        }
        if from_guest {
            if has_ack && flow.guest_ack.is_none_or(|a| seq_after(ack, a)) {
                flow.guest_ack = Some(ack);
            }
            flow.guest_fin |= fin;
            flow.guest_last_us = Some(now_us);
        } else {
            flow.handshake |= syn && has_ack;
            let data_start = seq.wrapping_add(syn as u32);
            let end = data_start.wrapping_add(len);
            if len > 0 || fin {
                // Data (or a FIN) at or before what was already sent: sent again.
                if let Some(prev) = flow.peer_sent_end
                    && !seq_after(end.wrapping_add(fin as u32), prev)
                {
                    flow.peer_retransmits += 1;
                }
                let new_end = end.wrapping_add(fin as u32);
                if flow.peer_sent_end.is_none_or(|p| seq_after(new_end, p)) {
                    flow.peer_sent_end = Some(new_end);
                }
            }
            if fin {
                flow.peer_fin_end = Some(end.wrapping_add(1));
                flow.peer_fin_first_us.get_or_insert(now_us);
            }
        }
    }
    let mut report = TcpReport {
        connections: 0,
        closed_cleanly: 0,
        reset: 0,
        peer_retransmits: 0,
        unacked_fins: Vec::new(),
        abandoned: Vec::new(),
    };
    for ((gport, peer, pport), f) in finished.iter().map(|(k, f)| (k, f)).chain(flows.iter()) {
        if !f.handshake {
            continue; // never answered: nothing to be polite to
        }
        report.connections += 1;
        report.peer_retransmits += f.peer_retransmits;
        if f.reset {
            report.reset += 1;
            continue;
        }
        if let Some(fin_end) = f.peer_fin_end {
            if f.guest_ack.is_some_and(|a| !seq_after(fin_end, a)) {
                if f.guest_fin {
                    report.closed_cleanly += 1;
                }
            } else {
                let name = format!("{}:{gport} <- {}:{pport}", ip(&guest), ip(peer));
                let quiet = match (f.guest_last_us, f.peer_fin_first_us) {
                    (Some(last), Some(fin)) => fin.saturating_sub(last),
                    _ => 0,
                };
                if !f.guest_fin && quiet >= ABANDONED_AFTER_US {
                    report.abandoned.push(name);
                } else {
                    report.unacked_fins.push(name);
                }
            }
        }
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sequence_comparison_wraps() {
        assert!(seq_after(1, 0));
        assert!(seq_after(5, u32::MAX - 5));
        assert!(!seq_after(u32::MAX - 5, 5));
        assert!(!seq_after(7, 7));
    }

    const GUEST: [u8; 4] = [10, 0, 2, 15];
    const PEER: [u8; 4] = [10, 0, 2, 101];
    const SYN: u8 = 2;
    const ACK: u8 = 16;
    const FIN: u8 = 1;

    /// One Ethernet/IPv4/TCP frame as a pcap record, at the start of the capture.
    fn record(from_guest: bool, seq: u32, ack: u32, flags: u8) -> Vec<u8> {
        record_at(0, from_guest, seq, ack, flags)
    }

    /// One Ethernet/IPv4/TCP frame as a pcap record, `us` microseconds in.
    fn record_at(us: u64, from_guest: bool, seq: u32, ack: u32, flags: u8) -> Vec<u8> {
        let (src, dst, sport, dport) =
            if from_guest { (GUEST, PEER, 50000u16, 7u16) } else { (PEER, GUEST, 7, 50000) };
        let mut f = vec![0u8; 14];
        f[12..14].copy_from_slice(&0x0800u16.to_be_bytes());
        let mut ip = vec![0x45, 0, 0, 40, 0, 0, 0, 0, 64, 6, 0, 0];
        ip.extend_from_slice(&src);
        ip.extend_from_slice(&dst);
        let mut tcp = Vec::new();
        tcp.extend_from_slice(&sport.to_be_bytes());
        tcp.extend_from_slice(&dport.to_be_bytes());
        tcp.extend_from_slice(&seq.to_be_bytes());
        tcp.extend_from_slice(&ack.to_be_bytes());
        tcp.extend_from_slice(&[0x50, flags, 0xff, 0xff, 0, 0, 0, 0]);
        f.extend(ip);
        f.extend(tcp);
        let mut r = Vec::new();
        r.extend_from_slice(&((us / 1_000_000) as u32).to_le_bytes());
        r.extend_from_slice(&((us % 1_000_000) as u32).to_le_bytes());
        r.extend_from_slice(&(f.len() as u32).to_le_bytes());
        r.extend_from_slice(&(f.len() as u32).to_le_bytes());
        r.extend(f);
        r
    }

    /// A handshake, then the peer's FIN, acknowledged by the guest or not.
    fn connection(isn: u32, acked: bool) -> Vec<Vec<u8>> {
        let peer_isn = isn.wrapping_mul(7);
        let mut c = vec![
            record(true, isn, 0, SYN),
            record(false, peer_isn, isn + 1, SYN | ACK),
            record(true, isn + 1, peer_isn + 1, ACK),
            record(false, peer_isn + 1, isn + 1, FIN | ACK),
        ];
        if acked {
            c.push(record(true, isn + 1, peer_isn + 2, FIN | ACK));
        }
        c
    }

    fn capture(records: Vec<Vec<u8>>) -> Result<TcpReport, String> {
        let mut raw = vec![0xd4, 0xc3, 0xb2, 0xa1, 2, 0, 4, 0];
        raw.extend_from_slice(&[0; 8]);
        raw.extend_from_slice(&65535u32.to_le_bytes());
        raw.extend_from_slice(&1u32.to_le_bytes());
        for r in records {
            raw.extend(r);
        }
        // Tests run side by side in one process: each capture gets its own file.
        static NEXT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!("spaceos-pcap-test-{}-{n}.pcap", std::process::id()));
        std::fs::write(&path, raw).map_err(|e| e.to_string())?;
        let r = analyze(&path, GUEST);
        std::fs::remove_file(&path).ok();
        r
    }

    /// A long run goes through every local port more than once: two connections on
    /// the same ports are two connections, and the second one's manners are its
    /// own -- a FIN it ignored is not excused by the first one's acknowledgement.
    #[test]
    fn a_reused_port_is_a_new_connection() {
        let polite = capture([connection(1000, true), connection(900_000, true)].concat()).unwrap();
        assert_eq!((polite.connections, polite.closed_cleanly), (2, 2));
        assert!(polite.unacked_fins.is_empty());

        let rude = capture([connection(1000, true), connection(900_000, false)].concat()).unwrap();
        assert_eq!((rude.connections, rude.closed_cleanly, rude.unacked_fins.len()), (2, 1, 1));

        // A SYN sent again with the same number is the same connection.
        let mut again = connection(5000, true);
        again.insert(1, record(true, 5000, 0, SYN));
        let r = capture(again).unwrap();
        assert_eq!((r.connections, r.closed_cleanly), (1, 1));
    }

    /// A network service killed mid-transfer leaves its connection open at the
    /// peer, which gives up after a while and sends a FIN nobody in the guest is
    /// left to answer. That is not the guest's TCP being rude -- but a FIN left
    /// unanswered on a connection the guest was still using, or had closed its own
    /// side of, is.
    #[test]
    fn a_killed_service_leaves_its_connections_behind() {
        let (isn, peer_isn) = (4000u32, 70_000u32);
        let opened = |fin_after_s: u64| {
            vec![
                record_at(0, true, isn, 0, SYN),
                record_at(1_000, false, peer_isn, isn + 1, SYN | ACK),
                record_at(2_000, true, isn + 1, peer_isn + 1, ACK),
                record_at(2_000 + fin_after_s * 1_000_000, false, peer_isn + 1, isn + 1, FIN | ACK),
            ]
        };
        let left = capture(opened(30)).unwrap();
        assert_eq!((left.connections, left.unacked_fins.len(), left.abandoned.len()), (1, 0, 1));

        let live = capture(opened(5)).unwrap();
        assert_eq!((live.unacked_fins.len(), live.abandoned.len()), (1, 0));

        let mut closed = opened(30);
        closed.insert(3, record_at(3_000, true, isn + 1, peer_isn + 1, FIN | ACK));
        let closed = capture(closed).unwrap();
        assert_eq!((closed.unacked_fins.len(), closed.abandoned.len()), (1, 0));

        // The next service answers a FIN it knows nothing about with a reset.
        let mut reset = opened(30);
        reset.push(record_at(40_000_000, true, isn + 1, 0, 4));
        let reset = capture(reset).unwrap();
        assert_eq!((reset.reset, reset.unacked_fins.len(), reset.abandoned.len()), (1, 0, 0));
    }
}
