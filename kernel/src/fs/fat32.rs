//! Read-only FAT32 reader over the block device.
//!
//! Enough to resolve a path and stream a file: boot sector, FAT chain walking with
//! a one-sector cache, and 8.3 directory entries (long-name entries are skipped, so
//! files the guest must read are named in 8.3 form). Every field that drives a read
//! is validated, so a corrupt or hostile volume yields an error, never a panic.

use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

use spaceabi::error::Error;

use crate::dev::virtio_blk::{self, SECTOR_SIZE};

const ATTR_DIRECTORY: u8 = 0x10;
const ATTR_VOLUME_ID: u8 = 0x08;
const ATTR_LONG_NAME: u8 = 0x0F;
const EOC: u32 = 0x0FFF_FFF8;

#[derive(Clone, Copy, Debug)]
pub struct FileNode {
    pub first_cluster: u32,
    pub size: u64,
}

pub struct Fat32 {
    bytes_per_sector: u32,
    sectors_per_cluster: u32,
    fat_start: u64,
    fat_sectors: u32,
    data_start: u64,
    total_clusters: u32,
    root_cluster: u32,
    fat_cache_sector: u64,
    fat_cache: Vec<u8>,
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
            data_start,
            total_clusters,
            root_cluster,
            fat_cache_sector: u64::MAX,
            fat_cache: vec![0u8; SECTOR_SIZE as usize],
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
    fn lookup(&mut self, dir_cluster: u32, name: &str) -> Result<(u32, u64, bool), Error> {
        let cluster_len = self.cluster_bytes() as usize;
        let mut buf = vec![0u8; cluster_len];
        let mut cluster = dir_cluster;
        let mut visited = 0u32;
        loop {
            self.read_cluster(cluster, &mut buf)?;
            for entry in buf.chunks_exact(32) {
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
                return Ok((first, size, attr & ATTR_DIRECTORY != 0));
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

    /// Resolve an absolute path such as `/spaceos/model.slm` (case insensitive).
    pub fn open(&mut self, path: &str) -> Result<FileNode, Error> {
        let mut cluster = self.root_cluster;
        let mut node = FileNode { first_cluster: cluster, size: 0 };
        let mut components = path.split('/').filter(|c| !c.is_empty()).peekable();
        if components.peek().is_none() {
            return Err(Error::Invalid);
        }
        for component in components {
            if component.len() > 12 {
                return Err(Error::Invalid);
            }
            let upper: String = component.chars().map(|c| c.to_ascii_uppercase()).collect();
            let (first, size, is_dir) = self.lookup(cluster, &upper)?;
            node = FileNode { first_cluster: first, size };
            if is_dir {
                cluster = if first == 0 { self.root_cluster } else { first };
            } else {
                cluster = first;
            }
        }
        Ok(node)
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
}
