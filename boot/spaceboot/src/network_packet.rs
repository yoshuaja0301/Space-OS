#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DhcpLease {
    pub address: [u8; 4],
    pub subnet: [u8; 4],
    pub router: Option<[u8; 4]>,
    pub server: [u8; 4],
    pub seconds: u32,
}

fn word(bytes: &[u8], offset: usize) -> Option<u16> {
    Some(u16::from_be_bytes(bytes.get(offset..offset + 2)?.try_into().ok()?))
}

fn checksum(bytes: &[u8]) -> u16 {
    let mut sum = 0_u32;
    for pair in bytes.chunks(2) {
        sum += u32::from(u16::from_be_bytes([pair[0], *pair.get(1).unwrap_or(&0)]));
    }
    while sum >> 16 != 0 {
        sum = (sum & 0xffff) + (sum >> 16);
    }
    !u16::try_from(sum).unwrap_or(0)
}

pub fn request(mac: [u8; 6], xid: u32, offer: Option<DhcpLease>) -> [u8; 342] {
    let mut frame = [0; 342];
    frame[..6].fill(255);
    frame[6..12].copy_from_slice(&mac);
    frame[12..14].copy_from_slice(&[8, 0]);
    let ip = &mut frame[14..34];
    ip[0] = 0x45;
    ip[2..4].copy_from_slice(&328_u16.to_be_bytes());
    ip[8] = 64;
    ip[9] = 17;
    ip[16..20].fill(255);
    let sum = checksum(ip);
    ip[10..12].copy_from_slice(&sum.to_be_bytes());
    frame[34..36].copy_from_slice(&68_u16.to_be_bytes());
    frame[36..38].copy_from_slice(&67_u16.to_be_bytes());
    frame[38..40].copy_from_slice(&308_u16.to_be_bytes());
    let dhcp = &mut frame[42..];
    dhcp[..3].copy_from_slice(&[1, 1, 6]);
    dhcp[4..8].copy_from_slice(&xid.to_be_bytes());
    dhcp[10] = 128;
    dhcp[28..34].copy_from_slice(&mac);
    dhcp[236..240].copy_from_slice(&[99, 130, 83, 99]);
    dhcp[240..243].copy_from_slice(&[53, 1, if offer.is_some() { 3 } else { 1 }]);
    let mut end = 243;
    if let Some(lease) = offer {
        dhcp[end..end + 2].copy_from_slice(&[50, 4]);
        dhcp[end + 2..end + 6].copy_from_slice(&lease.address);
        end += 6;
        dhcp[end..end + 2].copy_from_slice(&[54, 4]);
        dhcp[end + 2..end + 6].copy_from_slice(&lease.server);
        end += 6;
    }
    dhcp[end..end + 6].copy_from_slice(&[55, 4, 1, 3, 51, 54]);
    dhcp[end + 6] = 255;
    frame
}

pub fn parse_reply(frame: &[u8], xid: u32, mac: [u8; 6], kind: u8) -> Option<DhcpLease> {
    if frame.get(12..14)? != [8, 0] {
        return None;
    }
    let ip = frame.get(14..)?;
    let header = usize::from(*ip.first()? & 15) * 4;
    if ip[0] >> 4 != 4 || header < 20 || ip.get(9) != Some(&17) {
        return None;
    }
    if word(ip, 6)? & 0x3fff != 0 || checksum(ip.get(..header)?) != 0 {
        return None;
    }
    let total = usize::from(word(ip, 2)?);
    let udp = ip.get(header..total)?;
    if word(udp, 0)? != 67 || word(udp, 2)? != 68 {
        return None;
    }
    let udp_len = usize::from(word(udp, 4)?);
    if udp_len != udp.len() {
        return None;
    }
    if word(udp, 6)? != 0 {
        let mut pseudo = [0_u8; 12];
        pseudo[..8].copy_from_slice(ip.get(12..20)?);
        pseudo[9] = 17;
        pseudo[10..12].copy_from_slice(&word(udp, 4)?.to_be_bytes());
        let sum = u32::from(!checksum(&pseudo)) + u32::from(!checksum(udp));
        if (sum & 0xffff) + (sum >> 16) != 0xffff {
            return None;
        }
    }
    let dhcp = udp.get(8..)?;
    if dhcp.get(..3)? != [2, 1, 6]
        || dhcp.get(4..8)? != xid.to_be_bytes()
        || dhcp.get(28..34)? != mac
        || dhcp.get(236..240)? != [99, 130, 83, 99]
    {
        return None;
    }
    let address: [u8; 4] = dhcp.get(16..20)?.try_into().ok()?;
    let mut subnet = None;
    let mut router = None;
    let mut server = None;
    let mut seconds = None;
    let mut message = None;
    let mut options = dhcp.get(240..)?;
    let mut ended = false;
    while let Some((&tag, rest)) = options.split_first() {
        options = rest;
        if tag == 0 {
            continue;
        }
        if tag == 255 {
            ended = true;
            break;
        }
        let (&length, rest) = options.split_first()?;
        let value = rest.get(..usize::from(length))?;
        options = rest.get(usize::from(length)..)?;
        match tag {
            1 if subnet.is_none() && length == 4 => subnet = Some(value.try_into().ok()?),
            3 if router.is_none() && length >= 4 && length % 4 == 0 => {
                router = Some(value[..4].try_into().ok()?)
            }
            51 if seconds.is_none() && length == 4 => {
                seconds = Some(u32::from_be_bytes(value.try_into().ok()?))
            }
            53 if message.is_none() && length == 1 => message = Some(value[0]),
            54 if server.is_none() && length == 4 => server = Some(value.try_into().ok()?),
            1 | 3 | 51 | 53 | 54 => return None,
            _ => {}
        }
    }
    let lease = DhcpLease { address, subnet: subnet?, router, server: server?, seconds: seconds? };
    let mask = u32::from_be_bytes(lease.subnet);
    if !ended
        || message != Some(kind)
        || lease.seconds == 0
        || !unicast(address)
        || !unicast(lease.server)
        || mask == 0
        || (!mask).wrapping_add(1) & !mask != 0
        || lease.router.is_some_and(|ip| !unicast(ip))
    {
        return None;
    }
    Some(lease)
}

fn unicast(ip: [u8; 4]) -> bool {
    ip[0] != 0 && ip[0] != 127 && ip[0] < 224 && ip != [255; 4]
}

#[cfg(test)]
mod tests {
    use super::*;

    const MAC: [u8; 6] = [2, 3, 4, 5, 6, 7];
    const XID: u32 = 0x12345678;

    fn reply() -> [u8; 342] {
        let mut frame = request(MAC, XID, None);
        frame[34..36].copy_from_slice(&67_u16.to_be_bytes());
        frame[36..38].copy_from_slice(&68_u16.to_be_bytes());
        let dhcp = &mut frame[42..];
        dhcp[0] = 2;
        dhcp[16..20].copy_from_slice(&[10, 0, 2, 15]);
        dhcp[240..].fill(0);
        dhcp[240..268].copy_from_slice(&[
            53, 1, 5, 1, 4, 255, 255, 255, 0, 3, 4, 10, 0, 2, 2, 54, 4, 10, 0, 2, 2, 51, 4, 0, 0, 14, 16, 255,
        ]);
        frame
    }

    #[test]
    fn returns_observed_lease_when_ack_matches_transaction() {
        let frame = reply();
        let lease = parse_reply(&frame, XID, MAC, 5);
        assert_eq!(
            lease,
            Some(DhcpLease {
                address: [10, 0, 2, 15],
                subnet: [255, 255, 255, 0],
                router: Some([10, 0, 2, 2]),
                server: [10, 0, 2, 2],
                seconds: 3600
            })
        );
    }

    #[test]
    fn rejects_spoofed_transaction_and_client_identity() {
        let frame = reply();
        assert_eq!(parse_reply(&frame, XID + 1, MAC, 5), None);
        assert_eq!(parse_reply(&frame, XID, [9; 6], 5), None);
        assert_eq!(parse_reply(&frame, XID, MAC, 2), None);
    }

    #[test]
    fn rejects_packet_when_any_header_or_required_option_is_invalid() {
        let valid = reply();
        for (offset, byte) in [
            (12, 9),
            (14, 0x65),
            (20, 32),
            (23, 6),
            (24, 123),
            (35, 68),
            (39, 1),
            (42, 1),
            (278, 0),
            (282, 255),
            (285, 250),
            (309, 0),
        ] {
            let mut frame = valid;
            frame[offset] = byte;
            assert_eq!(parse_reply(&frame, XID, MAC, 5), None, "offset {offset}");
        }
    }

    #[test]
    fn rejects_truncated_ack_at_every_boundary() {
        let frame = reply();
        for length in 0..frame.len() {
            assert_eq!(parse_reply(&frame[..length], XID, MAC, 5), None);
        }
    }

    #[test]
    fn rejects_noncontiguous_subnet_and_missing_end_option() {
        let mut frame = reply();
        frame[289] = 253;
        assert_eq!(parse_reply(&frame, XID, MAC, 5), None);
        let mut frame = reply();
        frame[309] = 0;
        assert_eq!(parse_reply(&frame, XID, MAC, 5), None);
    }

    #[test]
    fn rejects_truncated_and_unrelated_frames() {
        let bytes = [0_u8; 400];
        assert_eq!(parse_reply(&bytes, 7, [1; 6], 5), None);
        for length in 0..bytes.len() {
            assert_eq!(parse_reply(&bytes[..length], 7, [1; 6], 5), None);
        }
    }
}
