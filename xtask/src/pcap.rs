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
}

pub struct TcpReport {
    pub connections: usize,
    pub closed_cleanly: usize,
    pub reset: usize,
    pub peer_retransmits: u32,
    /// Connections whose FIN from the peer the guest never acknowledged.
    pub unacked_fins: Vec<String>,
}

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
    let le = match &raw[..4] {
        [0xd4, 0xc3, 0xb2, 0xa1] | [0x4d, 0x3c, 0xb2, 0xa1] => true,
        [0xa1, 0xb2, 0xc3, 0xd4] | [0xa1, 0xb2, 0x3c, 0x4d] => false,
        _ => return Err(format!("{}: not a pcap file", path.display())),
    };
    let u32at = |b: &[u8], at: usize| {
        let w = [b[at], b[at + 1], b[at + 2], b[at + 3]];
        if le { u32::from_le_bytes(w) } else { u32::from_be_bytes(w) }
    };
    let be16 = |b: &[u8], at: usize| u16::from_be_bytes([b[at], b[at + 1]]);
    let be32 = |b: &[u8], at: usize| u32::from_be_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]]);
    let mut flows: BTreeMap<(u16, [u8; 4], u16), Flow> = BTreeMap::new();
    let mut at = 24;
    while at + 16 <= raw.len() {
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
        let flow = flows.entry(key).or_default();
        if rst {
            flow.reset = true;
        }
        if from_guest {
            if has_ack && flow.guest_ack.is_none_or(|a| seq_after(ack, a)) {
                flow.guest_ack = Some(ack);
            }
            flow.guest_fin |= fin;
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
            }
        }
    }
    let mut report = TcpReport {
        connections: 0,
        closed_cleanly: 0,
        reset: 0,
        peer_retransmits: 0,
        unacked_fins: Vec::new(),
    };
    for ((gport, peer, pport), f) in &flows {
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
                report.unacked_fins.push(format!("{}:{gport} <- {}:{pport}", ip(&guest), ip(peer)));
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
}
