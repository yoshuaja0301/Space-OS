//! FAT32 over the block device: read, and write to the data volume.
//!
//! Enough to resolve a path and stream a file: boot sector, FAT chain walking with
//! a one-sector cache, and 8.3 directory entries (long-name entries are skipped, so
//! files the guest must read are named in 8.3 form). Every field that drives a read
//! is validated, so a corrupt or hostile volume yields an error, never a panic.
//!
//! Writing follows the same rule and adds three of its own:
//!
//! * **Only the data volume is reachable.** The only block device the kernel binds
//!   is the virtio disk; the ESP the firmware booted from is not one, so no write
//!   here can reach the bootloader or the kernel image.
//! * **Every FAT copy is updated.** A volume whose FATs disagree is a volume another
//!   implementation may read differently from this one.
//! * **A newly allocated cluster is zeroed before it joins a file.** It still holds
//!   whatever its previous owner left there, and a reader of a gap would see it.

use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

use spaceabi::error::Error;
use spaceabi::syscall::DirEntry;

use crate::dev::virtio_blk::{self, SECTOR_SIZE};

const ATTR_DIRECTORY: u8 = 0x10;
const ATTR_VOLUME_ID: u8 = 0x08;
const ATTR_LONG_NAME: u8 = 0x0F;
/// FAT entries at or above this mark end a chain.
const EOC: u32 = 0x0FFF_FFF8;
/// What this driver writes to end a chain.
const EOC_MARK: u32 = 0x0FFF_FFFF;
/// A plain file, no other attribute bits.
const ATTR_ARCHIVE: u8 = 0x20;
/// Bytes in one directory entry.
const DIR_ENTRY: usize = 32;

#[derive(Clone, Copy, Debug)]
pub struct FileNode {
    pub first_cluster: u32,
    pub size: u64,
    /// Sector holding this file's directory entry, and the entry's offset inside it.
    /// Zero means "no entry of its own" -- the volume root -- and such a node can be
    /// read but never written, because there would be nowhere to record its size.
    pub entry_lba: u64,
    pub entry_off: u32,
}

/// What a directory entry says, plus where it lives.
#[derive(Clone, Copy)]
struct Entry {
    first: u32,
    size: u64,
    is_dir: bool,
    lba: u64,
    off: u32,
}

pub struct Fat32 {
    bytes_per_sector: u32,
    sectors_per_cluster: u32,
    fat_start: u64,
    fat_sectors: u32,
    /// Copies of the FAT. Every one of them is updated on every change.
    num_fats: u32,
    data_start: u64,
    total_clusters: u32,
    root_cluster: u32,
    fat_cache_sector: u64,
    fat_cache: Vec<u8>,
    /// Where to start looking for a free cluster. Only a hint: allocation still
    /// scans the whole FAT before giving up.
    next_free_hint: u32,
}

fn rd16(b: &[u8], o: usize) -> u16 {
    u16::from_le_bytes([b[o], b[o + 1]])
}

fn rd32(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]])
}

impl Fat32 {
    /// Parse the boot sector of the volume starting at `start_lba`.
    pub fn mount() -> Result<Fat32, Error> {
        let mut boot = vec![0u8; SECTOR_SIZE as usize];
        virtio_blk::read_sectors(0, &mut boot)?;
        if rd16(&boot, 510) != 0xAA55 {
            return Err(Error::NoExec);
        }
        let bytes_per_sector = rd16(&boot, 11) as u32;
        let sectors_per_cluster = boot[13] as u32;
        let reserved = rd16(&boot, 14) as u32;
        let num_fats = boot[16] as u32;
        let fat_size_16 = rd16(&boot, 22) as u32;
        let total_sectors_16 = rd16(&boot, 19) as u32;
        let fat_size_32 = rd32(&boot, 36);
        let total_sectors_32 = rd32(&boot, 32);
        let root_cluster = rd32(&boot, 44);

        if bytes_per_sector as u64 != SECTOR_SIZE
            || !sectors_per_cluster.is_power_of_two()
            || sectors_per_cluster == 0
            || sectors_per_cluster > 128
            || num_fats == 0
            || num_fats > 4
            || reserved == 0
            || fat_size_16 != 0
            || fat_size_32 == 0
            || root_cluster < 2
        {
            return Err(Error::NoExec);
        }
        let total_sectors = if total_sectors_16 != 0 { total_sectors_16 } else { total_sectors_32 };
        let fat_start = reserved as u64;
        let data_start = fat_start + (num_fats as u64) * (fat_size_32 as u64);
        if (total_sectors as u64) <= data_start {
            return Err(Error::NoExec);
        }
        let total_clusters = ((total_sectors as u64 - data_start) / sectors_per_cluster as u64) as u32;
        Ok(Fat32 {
            bytes_per_sector,
            sectors_per_cluster,
            fat_start,
            fat_sectors: fat_size_32,
            num_fats,
            data_start,
            total_clusters,
            root_cluster,
            fat_cache_sector: u64::MAX,
            fat_cache: vec![0u8; SECTOR_SIZE as usize],
            next_free_hint: 2,
        })
    }

    pub fn cluster_bytes(&self) -> u64 {
        self.bytes_per_sector as u64 * self.sectors_per_cluster as u64
    }

    fn cluster_lba(&self, cluster: u32) -> Result<u64, Error> {
        if cluster < 2 || cluster >= self.total_clusters + 2 {
            return Err(Error::Invalid);
        }
        Ok(self.data_start + (cluster as u64 - 2) * self.sectors_per_cluster as u64)
    }

    /// Next cluster in the chain, or `None` at the end.
    fn next_cluster(&mut self, cluster: u32) -> Result<Option<u32>, Error> {
        let byte = cluster as u64 * 4;
        let sector = self.fat_start + byte / SECTOR_SIZE;
        if sector >= self.fat_start + self.fat_sectors as u64 {
            return Err(Error::Invalid);
        }
        if self.fat_cache_sector != sector {
            let mut buf = core::mem::take(&mut self.fat_cache);
            let r = virtio_blk::read_sectors(sector, &mut buf);
            self.fat_cache = buf;
            r?;
            self.fat_cache_sector = sector;
        }
        let off = (byte % SECTOR_SIZE) as usize;
        let entry = rd32(&self.fat_cache, off) & 0x0FFF_FFFF;
        if entry >= EOC || entry == 0 {
            Ok(None)
        } else if entry < 2 || entry >= self.total_clusters + 2 {
            Err(Error::Invalid)
        } else {
            Ok(Some(entry))
        }
    }

    /// Read one cluster into `buf` (which must be exactly one cluster long).
    fn read_cluster(&mut self, cluster: u32, buf: &mut [u8]) -> Result<(), Error> {
        let lba = self.cluster_lba(cluster)?;
        virtio_blk::read_sectors(lba, buf)
    }

    /// 8.3 name of a directory entry, e.g. `MODEL.SLM`.
    fn short_name(entry: &[u8]) -> String {
        let mut name = String::new();
        for &b in &entry[0..8] {
            if b == b' ' {
                break;
            }
            name.push(b as char);
        }
        let mut ext = String::new();
        for &b in &entry[8..11] {
            if b == b' ' {
                break;
            }
            ext.push(b as char);
        }
        if !ext.is_empty() {
            name.push('.');
            name.push_str(&ext);
        }
        name
    }

    /// Find `name` (8.3, upper case) inside the directory starting at `cluster`.
    fn lookup(&mut self, dir_cluster: u32, name: &str) -> Result<Entry, Error> {
        let cluster_len = self.cluster_bytes() as usize;
        let mut buf = vec![0u8; cluster_len];
        let mut cluster = dir_cluster;
        let mut visited = 0u32;
        loop {
            let base = self.cluster_lba(cluster)?;
            self.read_cluster(cluster, &mut buf)?;
            for (i, entry) in buf.chunks_exact(DIR_ENTRY).enumerate() {
                match entry[0] {
                    0x00 => return Err(Error::NotFound), // end of directory
                    0xE5 => continue,                    // deleted
                    _ => {}
                }
                let attr = entry[11];
                if attr == ATTR_LONG_NAME || attr & ATTR_VOLUME_ID != 0 {
                    continue;
                }
                if Self::short_name(entry) != name {
                    continue;
                }
                let first = ((rd16(entry, 20) as u32) << 16) | rd16(entry, 26) as u32;
                let size = rd32(entry, 28) as u64;
                let byte = (i * DIR_ENTRY) as u64;
                return Ok(Entry {
                    first,
                    size,
                    is_dir: attr & ATTR_DIRECTORY != 0,
                    lba: base + byte / SECTOR_SIZE,
                    off: (byte % SECTOR_SIZE) as u32,
                });
            }
            visited += 1;
            if visited > self.total_clusters {
                return Err(Error::Invalid);
            }
            match self.next_cluster(cluster)? {
                Some(next) => cluster = next,
                None => return Err(Error::NotFound),
            }
        }
    }

    /// Resolve an absolute path such as `/spaceos/model.slm` (case insensitive) to
    /// `(first cluster, size, is_dir)`. `/` is the volume root.
    ///
    /// Only a directory can be walked through: treating a file's data as directory
    /// entries would hand the caller whatever those bytes happen to decode to.
    fn resolve(&mut self, path: &str) -> Result<Entry, Error> {
        let mut cluster = self.root_cluster;
        // The volume root has no directory entry of its own, so it carries none.
        let mut found = Entry { first: self.root_cluster, size: 0, is_dir: true, lba: 0, off: 0 };
        let mut components = path.split('/').filter(|c| !c.is_empty()).peekable();
        while let Some(component) = components.next() {
            if component.len() > 12 {
                return Err(Error::Invalid);
            }
            let upper: String = component.chars().map(|c| c.to_ascii_uppercase()).collect();
            let e = self.lookup(cluster, &upper)?;
            if !e.is_dir {
                if components.peek().is_some() {
                    return Err(Error::NotFound);
                }
                return Ok(e);
            }
            cluster = if e.first == 0 { self.root_cluster } else { e.first };
            found = Entry { first: cluster, ..e };
        }
        Ok(found)
    }

    /// Open a regular file. A directory is not a file and is refused.
    pub fn open(&mut self, path: &str) -> Result<FileNode, Error> {
        let e = self.resolve(path)?;
        if e.is_dir {
            return Err(Error::Invalid);
        }
        Ok(FileNode { first_cluster: e.first, size: e.size, entry_lba: e.lba, entry_off: e.off })
    }

    /// Entries of a directory, at most `max`.
    pub fn list(&mut self, path: &str, max: usize) -> Result<Vec<DirEntry>, Error> {
        let dir = self.resolve(path)?;
        if !dir.is_dir {
            return Err(Error::Invalid);
        }
        let cluster = dir.first;
        let cluster_len = self.cluster_bytes() as usize;
        let mut buf = vec![0u8; cluster_len];
        let mut out = Vec::new();
        out.try_reserve(max).map_err(|_| Error::NoMemory)?;
        let mut cluster = cluster;
        let mut visited = 0u32;
        loop {
            self.read_cluster(cluster, &mut buf)?;
            for entry in buf.chunks_exact(32) {
                match entry[0] {
                    0x00 => return Ok(out), // end of directory
                    0xE5 => continue,       // deleted
                    _ => {}
                }
                let attr = entry[11];
                if attr == ATTR_LONG_NAME || attr & ATTR_VOLUME_ID != 0 {
                    continue;
                }
                let name = Self::short_name(entry);
                let mut e = DirEntry { is_dir: u8::from(attr & ATTR_DIRECTORY != 0), ..Default::default() };
                let bytes = name.as_bytes();
                let n = bytes.len().min(e.name.len());
                e.name[..n].copy_from_slice(&bytes[..n]);
                e.size = rd32(entry, 28) as u64;
                out.push(e);
                if out.len() == max {
                    return Ok(out);
                }
            }
            visited += 1;
            if visited > self.total_clusters {
                return Err(Error::Invalid);
            }
            match self.next_cluster(cluster)? {
                Some(next) => cluster = next,
                None => return Ok(out),
            }
        }
    }

    /// Read up to `buf.len()` bytes of `node` starting at `offset`.
    pub fn read(&mut self, node: &FileNode, offset: u64, buf: &mut [u8]) -> Result<usize, Error> {
        if offset >= node.size || buf.is_empty() {
            return Ok(0);
        }
        let want = buf.len().min((node.size - offset) as usize);
        let cluster_len = self.cluster_bytes();
        let mut cluster = node.first_cluster;
        let mut skip = offset / cluster_len;
        let mut hops = 0u32;
        while skip > 0 {
            cluster = self.next_cluster(cluster)?.ok_or(Error::Invalid)?;
            skip -= 1;
            hops += 1;
            if hops > self.total_clusters {
                return Err(Error::Invalid);
            }
        }
        let mut chunk = vec![0u8; cluster_len as usize];
        let mut written = 0usize;
        let mut in_cluster = offset % cluster_len;
        while written < want {
            self.read_cluster(cluster, &mut chunk)?;
            let n = ((cluster_len - in_cluster) as usize).min(want - written);
            buf[written..written + n].copy_from_slice(&chunk[in_cluster as usize..in_cluster as usize + n]);
            written += n;
            in_cluster = 0;
            if written < want {
                cluster = self.next_cluster(cluster)?.ok_or(Error::Invalid)?;
            }
        }
        Ok(written)
    }

    // ---- writing -------------------------------------------------------------
    //
    // Everything below changes the volume. Each step validates first and writes
    // second, so a refusal leaves the disk exactly as it was.

    /// The raw FAT entry for `cluster`, without deciding what it means.
    fn fat_raw(&mut self, cluster: u32) -> Result<u32, Error> {
        let byte = cluster as u64 * 4;
        let sector = self.fat_start + byte / SECTOR_SIZE;
        if sector >= self.fat_start + self.fat_sectors as u64 {
            return Err(Error::Invalid);
        }
        if self.fat_cache_sector != sector {
            let mut buf = core::mem::take(&mut self.fat_cache);
            let r = virtio_blk::read_sectors(sector, &mut buf);
            self.fat_cache = buf;
            r?;
            self.fat_cache_sector = sector;
        }
        Ok(rd32(&self.fat_cache, (byte % SECTOR_SIZE) as usize) & 0x0FFF_FFFF)
    }

    /// Point `cluster` at `value`, in every copy of the FAT.
    fn fat_set(&mut self, cluster: u32, value: u32) -> Result<(), Error> {
        if cluster < 2 || cluster >= self.total_clusters + 2 {
            return Err(Error::Invalid);
        }
        let byte = cluster as u64 * 4;
        let sector_in_fat = byte / SECTOR_SIZE;
        if sector_in_fat >= self.fat_sectors as u64 {
            return Err(Error::Invalid);
        }
        let off = (byte % SECTOR_SIZE) as usize;
        let mut buf = vec![0u8; SECTOR_SIZE as usize];
        for copy in 0..self.num_fats as u64 {
            let sector = self.fat_start + copy * self.fat_sectors as u64 + sector_in_fat;
            virtio_blk::read_sectors(sector, &mut buf)?;
            // The top four bits are reserved; the spec says to leave them alone.
            let old = rd32(&buf, off);
            let new = (old & 0xF000_0000) | (value & 0x0FFF_FFFF);
            buf[off..off + 4].copy_from_slice(&new.to_le_bytes());
            virtio_blk::write_sectors(sector, &buf)?;
        }
        // The cached sector may be one of the ones just rewritten.
        self.fat_cache_sector = u64::MAX;
        Ok(())
    }

    /// Overwrite one whole cluster.
    fn write_cluster(&mut self, cluster: u32, buf: &[u8]) -> Result<(), Error> {
        if buf.len() as u64 != self.cluster_bytes() {
            return Err(Error::Invalid);
        }
        let lba = self.cluster_lba(cluster)?;
        virtio_blk::write_sectors(lba, buf)
    }

    /// Claim a free cluster as the end of a chain, zeroed and ready to use.
    fn alloc_cluster(&mut self) -> Result<u32, Error> {
        let last = self.total_clusters + 2;
        let start = self.next_free_hint.clamp(2, last);
        for cluster in (start..last).chain(2..start) {
            if self.fat_raw(cluster)? != 0 {
                continue;
            }
            self.fat_set(cluster, EOC_MARK)?;
            self.next_free_hint = cluster + 1;
            // Zero it before it belongs to anybody: whatever the previous owner left
            // here must not become the contents of a hole in a growing file.
            let zeros = vec![0u8; self.cluster_bytes() as usize];
            self.write_cluster(cluster, &zeros)?;
            return Ok(cluster);
        }
        Err(Error::NoMemory)
    }

    /// Extend a chain by one cluster and return it.
    fn append_cluster(&mut self, last: u32) -> Result<u32, Error> {
        let new = self.alloc_cluster()?;
        self.fat_set(last, new)?;
        Ok(new)
    }

    /// Give every cluster of a chain back to the free pool.
    fn free_chain(&mut self, first: u32) -> Result<(), Error> {
        let mut cluster = first;
        let mut hops = 0u32;
        while cluster >= 2 && cluster < self.total_clusters + 2 {
            let next = self.fat_raw(cluster)?;
            self.fat_set(cluster, 0)?;
            self.next_free_hint = self.next_free_hint.min(cluster);
            hops += 1;
            if hops > self.total_clusters {
                return Err(Error::Invalid);
            }
            if !(2..EOC).contains(&next) {
                break;
            }
            cluster = next;
        }
        Ok(())
    }

    /// Write a file's size and first cluster back into its directory entry.
    fn update_entry(&mut self, node: &FileNode) -> Result<(), Error> {
        if node.entry_lba == 0 {
            return Err(Error::Invalid);
        }
        let off = node.entry_off as usize;
        if off + DIR_ENTRY > SECTOR_SIZE as usize {
            return Err(Error::Invalid);
        }
        let mut sector = vec![0u8; SECTOR_SIZE as usize];
        virtio_blk::read_sectors(node.entry_lba, &mut sector)?;
        sector[off + 20..off + 22].copy_from_slice(&((node.first_cluster >> 16) as u16).to_le_bytes());
        sector[off + 26..off + 28].copy_from_slice(&(node.first_cluster as u16).to_le_bytes());
        sector[off + 28..off + 32].copy_from_slice(&(node.size as u32).to_le_bytes());
        virtio_blk::write_sectors(node.entry_lba, &sector)
    }

    /// Write `data` at `offset`, growing the file if it runs past the end.
    ///
    /// Returns when the bytes, the FAT and the directory entry are all on the device
    /// and a flush has been asked for -- a partial write that reported success would
    /// be worse than an error.
    pub fn write(&mut self, node: &mut FileNode, offset: u64, data: &[u8]) -> Result<usize, Error> {
        if data.is_empty() {
            return Ok(0);
        }
        if node.entry_lba == 0 {
            return Err(Error::Invalid);
        }
        let end = offset.checked_add(data.len() as u64).ok_or(Error::Invalid)?;
        // FAT32 records a size in 32 bits; a file cannot be told to grow past that.
        if end > u32::MAX as u64 {
            return Err(Error::Invalid);
        }
        let cluster_len = self.cluster_bytes();
        if node.first_cluster < 2 {
            node.first_cluster = self.alloc_cluster()?;
        }
        let mut cluster = node.first_cluster;
        let mut skip = offset / cluster_len;
        let mut hops = 0u32;
        while skip > 0 {
            cluster = match self.next_cluster(cluster)? {
                Some(next) => next,
                None => self.append_cluster(cluster)?,
            };
            skip -= 1;
            hops += 1;
            if hops > self.total_clusters {
                return Err(Error::Invalid);
            }
        }
        let mut chunk = vec![0u8; cluster_len as usize];
        let mut written = 0usize;
        let mut in_cluster = offset % cluster_len;
        while written < data.len() {
            let n = ((cluster_len - in_cluster) as usize).min(data.len() - written);
            // A partial cluster keeps the bytes around what is being replaced.
            if n as u64 != cluster_len {
                self.read_cluster(cluster, &mut chunk)?;
            }
            let at = in_cluster as usize;
            chunk[at..at + n].copy_from_slice(&data[written..written + n]);
            self.write_cluster(cluster, &chunk)?;
            written += n;
            in_cluster = 0;
            if written < data.len() {
                cluster = match self.next_cluster(cluster)? {
                    Some(next) => next,
                    None => self.append_cluster(cluster)?,
                };
            }
        }
        node.size = node.size.max(end);
        self.update_entry(node)?;
        virtio_blk::flush()?;
        Ok(written)
    }

    /// Find a free directory slot, extending the directory if every one is taken.
    fn alloc_dir_entry(&mut self, dir_cluster: u32) -> Result<(u64, u32), Error> {
        let cluster_len = self.cluster_bytes() as usize;
        let mut buf = vec![0u8; cluster_len];
        let mut cluster = dir_cluster;
        let mut visited = 0u32;
        loop {
            let base = self.cluster_lba(cluster)?;
            self.read_cluster(cluster, &mut buf)?;
            for (i, entry) in buf.chunks_exact(DIR_ENTRY).enumerate() {
                // Never used, or freed by a delete: either is ours to take.
                if entry[0] == 0x00 || entry[0] == 0xE5 {
                    let byte = (i * DIR_ENTRY) as u64;
                    return Ok((base + byte / SECTOR_SIZE, (byte % SECTOR_SIZE) as u32));
                }
            }
            visited += 1;
            if visited > self.total_clusters {
                return Err(Error::Invalid);
            }
            cluster = match self.next_cluster(cluster)? {
                Some(next) => next,
                // A directory with no room left grows like any other chain; the new
                // cluster arrives zeroed, so every slot in it reads as unused.
                None => self.append_cluster(cluster)?,
            };
        }
    }

    /// Pack `name` into the 11 bytes of an 8.3 directory entry.
    ///
    /// Long names are refused rather than truncated or silently mangled: this driver
    /// does not write the long-name entries that would preserve them, and a file
    /// whose name is not the one asked for is worse than a rejected request.
    fn short_name_bytes(name: &str) -> Result<[u8; 11], Error> {
        if name.is_empty() || !name.is_ascii() {
            return Err(Error::Invalid);
        }
        let (base, ext) = match name.rsplit_once('.') {
            Some((b, e)) => (b, e),
            None => (name, ""),
        };
        if base.is_empty() || base.len() > 8 || ext.len() > 3 || base.contains('.') {
            return Err(Error::Invalid);
        }
        let ok = |c: char| c.is_ascii_alphanumeric() || "$%'-_@~`!(){}^#&".contains(c);
        if !base.chars().all(ok) || !ext.chars().all(ok) {
            return Err(Error::Invalid);
        }
        let mut out = [b' '; 11];
        for (i, c) in base.chars().enumerate() {
            out[i] = c.to_ascii_uppercase() as u8;
        }
        for (i, c) in ext.chars().enumerate() {
            out[8 + i] = c.to_ascii_uppercase() as u8;
        }
        Ok(out)
    }

    /// Create `path` empty, or empty it if it is already there.
    ///
    /// "Create or truncate" is the whole of it: this driver rewrites files, it does
    /// not edit them in place, and every caller above it writes a file whole.
    pub fn create(&mut self, path: &str) -> Result<FileNode, Error> {
        let trimmed = path.trim_end_matches('/');
        let (parent_path, name) = match trimmed.rsplit_once('/') {
            Some((p, n)) => (if p.is_empty() { "/" } else { p }, n),
            None => ("/", trimmed),
        };
        if name.is_empty() || name.len() > 12 {
            return Err(Error::Invalid);
        }
        let short = Self::short_name_bytes(name)?;
        let parent = self.resolve(parent_path)?;
        if !parent.is_dir {
            return Err(Error::Invalid);
        }
        let upper: String = name.chars().map(|c| c.to_ascii_uppercase()).collect();
        match self.lookup(parent.first, &upper) {
            // Already there: a directory is not something to overwrite, a file is.
            Ok(e) if e.is_dir => Err(Error::Invalid),
            Ok(e) => {
                let node = FileNode { first_cluster: 0, size: 0, entry_lba: e.lba, entry_off: e.off };
                if e.first >= 2 {
                    self.free_chain(e.first)?;
                }
                self.update_entry(&node)?;
                virtio_blk::flush()?;
                Ok(node)
            }
            Err(Error::NotFound) => {
                let (lba, off) = self.alloc_dir_entry(parent.first)?;
                let at = off as usize;
                if at + DIR_ENTRY > SECTOR_SIZE as usize {
                    return Err(Error::Invalid);
                }
                let mut sector = vec![0u8; SECTOR_SIZE as usize];
                virtio_blk::read_sectors(lba, &mut sector)?;
                // A fresh entry: name, "plain file", and nothing else. Timestamps are
                // left at zero because this kernel has no wall clock to put in them,
                // and inventing one would be a lie stored on disk.
                sector[at..at + DIR_ENTRY].fill(0);
                sector[at..at + 11].copy_from_slice(&short);
                sector[at + 11] = ATTR_ARCHIVE;
                virtio_blk::write_sectors(lba, &sector)?;
                virtio_blk::flush()?;
                Ok(FileNode { first_cluster: 0, size: 0, entry_lba: lba, entry_off: off })
            }
            Err(e) => Err(e),
        }
    }
}
