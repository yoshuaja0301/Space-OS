//! Minimal ELF64 (little-endian, x86-64) reader: enough to load static executables.
//!
//! Only `PT_LOAD` program headers are interpreted. Everything is bounds-checked so a
//! malformed image yields an error instead of an out-of-bounds read.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ElfError {
    TooShort,
    BadMagic,
    NotElf64,
    NotLittleEndian,
    NotExecutable,
    WrongMachine,
    BadProgramHeaders,
    SegmentOutOfBounds,
}

pub const PT_LOAD: u32 = 1;
pub const PF_X: u32 = 1;
pub const PF_W: u32 = 2;
pub const PF_R: u32 = 4;

/// One `PT_LOAD` segment.
#[derive(Clone, Copy, Debug)]
pub struct Segment {
    pub vaddr: u64,
    pub memsz: u64,
    pub filesz: u64,
    pub offset: u64,
    pub flags: u32,
}

impl Segment {
    pub const fn writable(&self) -> bool {
        self.flags & PF_W != 0
    }
    pub const fn executable(&self) -> bool {
        self.flags & PF_X != 0
    }
}

pub struct Elf<'a> {
    data: &'a [u8],
    pub entry: u64,
    phoff: usize,
    phentsize: usize,
    phnum: usize,
}

fn rd16(d: &[u8], o: usize) -> u16 {
    u16::from_le_bytes([d[o], d[o + 1]])
}
fn rd32(d: &[u8], o: usize) -> u32 {
    u32::from_le_bytes([d[o], d[o + 1], d[o + 2], d[o + 3]])
}
fn rd64(d: &[u8], o: usize) -> u64 {
    let mut b = [0u8; 8];
    b.copy_from_slice(&d[o..o + 8]);
    u64::from_le_bytes(b)
}

impl<'a> Elf<'a> {
    pub fn parse(data: &'a [u8]) -> Result<Self, ElfError> {
        if data.len() < 64 {
            return Err(ElfError::TooShort);
        }
        if &data[0..4] != b"\x7fELF" {
            return Err(ElfError::BadMagic);
        }
        if data[4] != 2 {
            return Err(ElfError::NotElf64);
        }
        if data[5] != 1 {
            return Err(ElfError::NotLittleEndian);
        }
        if rd16(data, 16) != 2 {
            return Err(ElfError::NotExecutable);
        }
        if rd16(data, 18) != 0x3E {
            return Err(ElfError::WrongMachine);
        }
        let entry = rd64(data, 24);
        let phoff = rd64(data, 32) as usize;
        let phentsize = rd16(data, 54) as usize;
        let phnum = rd16(data, 56) as usize;
        if phentsize < 56 || phnum > 64 {
            return Err(ElfError::BadProgramHeaders);
        }
        let table_end = phoff
            .checked_add(phentsize.checked_mul(phnum).ok_or(ElfError::BadProgramHeaders)?)
            .ok_or(ElfError::BadProgramHeaders)?;
        if table_end > data.len() {
            return Err(ElfError::BadProgramHeaders);
        }
        Ok(Elf { data, entry, phoff, phentsize, phnum })
    }

    /// Iterate over `PT_LOAD` segments, validating that file ranges are in bounds.
    pub fn load_segments(&self) -> impl Iterator<Item = Result<Segment, ElfError>> + '_ {
        (0..self.phnum).filter_map(move |i| {
            let o = self.phoff + i * self.phentsize;
            let d = self.data;
            let p_type = rd32(d, o);
            if p_type != PT_LOAD {
                return None;
            }
            let seg = Segment {
                flags: rd32(d, o + 4),
                offset: rd64(d, o + 8),
                vaddr: rd64(d, o + 16),
                filesz: rd64(d, o + 32),
                memsz: rd64(d, o + 40),
            };
            if seg.filesz > seg.memsz {
                return Some(Err(ElfError::SegmentOutOfBounds));
            }
            let end = seg.offset.checked_add(seg.filesz);
            match end {
                Some(e) if e as usize <= d.len() && seg.vaddr.checked_add(seg.memsz).is_some() => {
                    Some(Ok(seg))
                }
                _ => Some(Err(ElfError::SegmentOutOfBounds)),
            }
        })
    }

    /// File bytes backing a segment.
    pub fn segment_data(&self, seg: &Segment) -> &'a [u8] {
        &self.data[seg.offset as usize..(seg.offset + seg.filesz) as usize]
    }
}
