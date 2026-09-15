//! Read-only ustar/POSIX tar archive walker used for the initrd.
//!
//! Regular files only; other entry kinds are skipped. No allocation.

pub struct TarEntry<'a> {
    pub name: &'a str,
    pub data: &'a [u8],
}

pub struct Tar<'a> {
    data: &'a [u8],
}

impl<'a> Tar<'a> {
    pub const fn new(data: &'a [u8]) -> Self {
        Tar { data }
    }

    pub fn entries(&self) -> TarIter<'a> {
        TarIter { data: self.data, pos: 0 }
    }

    pub fn find(&self, name: &str) -> Option<&'a [u8]> {
        self.entries().find(|e| e.name == name).map(|e| e.data)
    }
}

pub struct TarIter<'a> {
    data: &'a [u8],
    pos: usize,
}

fn parse_octal(field: &[u8]) -> Option<u64> {
    let mut v: u64 = 0;
    let mut seen = false;
    for &b in field {
        match b {
            b'0'..=b'7' => {
                v = v.checked_mul(8)?.checked_add((b - b'0') as u64)?;
                seen = true;
            }
            0 | b' ' => {
                if seen {
                    break;
                }
            }
            _ => return None,
        }
    }
    Some(v)
}

impl<'a> Iterator for TarIter<'a> {
    type Item = TarEntry<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            let hdr = self.data.get(self.pos..self.pos + 512)?;
            if hdr.iter().all(|&b| b == 0) {
                return None;
            }
            let name_raw = &hdr[0..100];
            let name_len = name_raw.iter().position(|&b| b == 0).unwrap_or(100);
            let name = core::str::from_utf8(&name_raw[..name_len]).ok()?;
            let size = parse_octal(&hdr[124..136])? as usize;
            let typeflag = hdr[156];
            let data_start = self.pos + 512;
            let data = self.data.get(data_start..data_start.checked_add(size)?)?;
            self.pos = data_start + size.div_ceil(512) * 512;
            if typeflag == b'0' || typeflag == 0 {
                // ustar prefix field (155 bytes at 345) for long names.
                let prefix_raw = &hdr[345..500];
                let plen = prefix_raw.iter().position(|&b| b == 0).unwrap_or(155);
                if plen == 0 {
                    return Some(TarEntry { name, data });
                }
                // Long names with a prefix are not used by Space OS images; skip them.
                continue;
            }
        }
    }
}
