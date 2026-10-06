//! Driver-level network checks: ARP and ICMP frames built by hand and sent straight
//! through the device lease, with no network stack above the driver. What these
//! prove is the kernel's part -- frames leave, frames arrive, intact and in order --
//! against the QEMU user-mode network, whose gateway (10.0.2.2) answers ARP and ICMP
//! echo itself even when the network is restricted.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use libspace::spaceabi::error::Error;
use libspace::spaceabi::syscall::FRAME_MAX;
use libspace::{Handle, sys};

/// The address QEMU's user-mode DHCP server hands the first guest.
pub const GUEST_IP: [u8; 4] = [10, 0, 2, 15];
/// QEMU user-mode gateway, which also stands for the host.
pub const GATEWAY_IP: [u8; 4] = [10, 0, 2, 2];
const BROADCAST: [u8; 6] = [0xFF; 6];
const ETH_ARP: u16 = 0x0806;
const ETH_IPV4: u16 = 0x0800;
const ARP_REQUEST: u16 = 1;
const ARP_REPLY: u16 = 2;
const IP_ICMP: u8 = 1;
const ICMP_ECHO_REPLY: u8 = 0;
const ICMP_ECHO_REQUEST: u8 = 8;

fn be16(b: &[u8], at: usize) -> u16 {
    u16::from_be_bytes([b[at], b[at + 1]])
}

/// The Internet checksum (RFC 1071) over `data`.
pub fn checksum(data: &[u8]) -> u16 {
    let mut sum = 0u32;
    for chunk in data.chunks(2) {
        let word =
            if chunk.len() == 2 { u16::from_be_bytes([chunk[0], chunk[1]]) } else { (chunk[0] as u16) << 8 };
        sum += word as u32;
    }
    while sum >> 16 != 0 {
        sum = (sum & 0xFFFF) + (sum >> 16);
    }
    !(sum as u16)
}

fn eth_header(out: &mut Vec<u8>, dst: [u8; 6], src: [u8; 6], ethertype: u16) {
    out.extend_from_slice(&dst);
    out.extend_from_slice(&src);
    out.extend_from_slice(&ethertype.to_be_bytes());
}

fn arp(
    op: u16,
    dst_mac: [u8; 6],
    src_mac: [u8; 6],
    src_ip: [u8; 4],
    target_mac: [u8; 6],
    target_ip: [u8; 4],
) -> Vec<u8> {
    let mut f = Vec::with_capacity(42);
    eth_header(&mut f, dst_mac, src_mac, ETH_ARP);
    f.extend_from_slice(&1u16.to_be_bytes()); // hardware: Ethernet
    f.extend_from_slice(&ETH_IPV4.to_be_bytes());
    f.push(6);
    f.push(4);
    f.extend_from_slice(&op.to_be_bytes());
    f.extend_from_slice(&src_mac);
    f.extend_from_slice(&src_ip);
    f.extend_from_slice(&target_mac);
    f.extend_from_slice(&target_ip);
    f
}

/// Broadcast "who has `target_ip`, tell `src_ip`".
pub fn arp_request(src_mac: [u8; 6], src_ip: [u8; 4], target_ip: [u8; 4]) -> Vec<u8> {
    arp(ARP_REQUEST, BROADCAST, src_mac, src_ip, [0; 6], target_ip)
}

/// The sender MAC if `frame` is an ARP reply from `from_ip` addressed to `me`.
pub fn parse_arp_reply(frame: &[u8], me: [u8; 6], from_ip: [u8; 4]) -> Option<[u8; 6]> {
    if frame.len() < 42 || be16(frame, 12) != ETH_ARP || be16(frame, 20) != ARP_REPLY {
        return None;
    }
    if frame[0..6] != me || frame[28..32] != from_ip || frame[32..38] != me {
        return None;
    }
    let mut mac = [0u8; 6];
    mac.copy_from_slice(&frame[22..28]);
    Some(mac)
}

/// Answer an ARP request for `my_ip`, so the gateway can learn where we are. Returns
/// true when `frame` was such a request (and the answer was sent).
pub fn answer_arp(nic: Handle, frame: &[u8], my_mac: [u8; 6], my_ip: [u8; 4]) -> bool {
    if frame.len() < 42
        || be16(frame, 12) != ETH_ARP
        || be16(frame, 20) != ARP_REQUEST
        || frame[38..42] != my_ip
    {
        return false;
    }
    let mut asker_mac = [0u8; 6];
    asker_mac.copy_from_slice(&frame[22..28]);
    let mut asker_ip = [0u8; 4];
    asker_ip.copy_from_slice(&frame[28..32]);
    let reply = arp(ARP_REPLY, asker_mac, my_mac, my_ip, asker_mac, asker_ip);
    sys::net_send(nic, &reply).is_ok()
}

/// An ICMP echo request in an IPv4 packet in an Ethernet frame.
#[allow(clippy::too_many_arguments)]
pub fn icmp_echo(
    src_mac: [u8; 6],
    dst_mac: [u8; 6],
    src_ip: [u8; 4],
    dst_ip: [u8; 4],
    ident: u16,
    seq: u16,
    payload: &[u8],
) -> Vec<u8> {
    let icmp_len = 8 + payload.len();
    let total = 20 + icmp_len;
    let mut f = Vec::with_capacity(14 + total);
    eth_header(&mut f, dst_mac, src_mac, ETH_IPV4);
    let ip_start = f.len();
    f.push(0x45);
    f.push(0);
    f.extend_from_slice(&(total as u16).to_be_bytes());
    f.extend_from_slice(&ident.to_be_bytes()); // IP identification
    f.extend_from_slice(&0x4000u16.to_be_bytes()); // don't fragment
    f.push(64);
    f.push(IP_ICMP);
    f.extend_from_slice(&[0, 0]);
    f.extend_from_slice(&src_ip);
    f.extend_from_slice(&dst_ip);
    let ip_sum = checksum(&f[ip_start..ip_start + 20]);
    f[ip_start + 10..ip_start + 12].copy_from_slice(&ip_sum.to_be_bytes());
    let icmp_start = f.len();
    f.push(ICMP_ECHO_REQUEST);
    f.push(0);
    f.extend_from_slice(&[0, 0]);
    f.extend_from_slice(&ident.to_be_bytes());
    f.extend_from_slice(&seq.to_be_bytes());
    f.extend_from_slice(payload);
    let icmp_sum = checksum(&f[icmp_start..]);
    f[icmp_start + 2..icmp_start + 4].copy_from_slice(&icmp_sum.to_be_bytes());
    f
}

/// The echoed payload if `frame` is the echo reply from `from_ip` to (`ident`, `seq`),
/// with both checksums verified; `Err` names the first thing that is wrong with a
/// frame that is an echo reply but a broken one.
pub fn parse_icmp_reply(
    frame: &[u8],
    from_ip: [u8; 4],
    ident: u16,
    seq: u16,
) -> Option<Result<Vec<u8>, String>> {
    if frame.len() < 14 + 20 + 8 || be16(frame, 12) != ETH_IPV4 {
        return None;
    }
    let ip = &frame[14..];
    let ihl = (ip[0] & 0x0F) as usize * 4;
    if ip[0] >> 4 != 4 || ihl < 20 || ip.len() < ihl + 8 || ip[9] != IP_ICMP || ip[12..16] != from_ip {
        return None;
    }
    let total = be16(ip, 2) as usize;
    let end = total.min(ip.len());
    if end < ihl + 8 {
        return None;
    }
    let icmp = &ip[ihl..end];
    if icmp[0] != ICMP_ECHO_REPLY || be16(icmp, 4) != ident || be16(icmp, 6) != seq {
        return None;
    }
    if checksum(&ip[..ihl]) != 0 {
        return Some(Err(String::from("IPv4 header checksum does not verify")));
    }
    if total > ip.len() {
        return Some(Err(format!("IPv4 total length {total} exceeds the {} bytes received", ip.len())));
    }
    if checksum(icmp) != 0 {
        return Some(Err(String::from("ICMP checksum does not verify")));
    }
    Some(Ok(icmp[8..].to_vec()))
}

/// Wait up to `timeout_ms` for a frame `pick` accepts, handing every frame to it in
/// arrival order. Frames it declines are dropped. The waiting is `wait_any` on the
/// lease: no polling loop, the kernel wakes us when a frame arrives.
pub fn await_frame<T>(
    nic: Handle,
    timeout_ms: u64,
    mut pick: impl FnMut(&[u8]) -> Option<T>,
) -> Result<T, String> {
    let deadline = sys::ticks_ms() + timeout_ms;
    let mut buf = [0u8; FRAME_MAX];
    loop {
        loop {
            match sys::net_recv(nic, &mut buf) {
                Ok(n) => {
                    if let Some(v) = pick(&buf[..n]) {
                        return Ok(v);
                    }
                }
                Err(Error::WouldBlock) => break,
                Err(e) => return Err(format!("net_recv: {e}")),
            }
        }
        let now = sys::ticks_ms();
        if now >= deadline {
            return Err(format!("nothing suitable arrived within {timeout_ms} ms"));
        }
        match sys::wait_any(&[nic], deadline - now) {
            Ok(0) => {}
            Ok(i) => return Err(format!("wait_any reported index {i} for a one-handle set")),
            Err(Error::TimedOut) => return Err(format!("nothing suitable arrived within {timeout_ms} ms")),
            Err(e) => return Err(format!("wait_any: {e}")),
        }
    }
}
