//! The data volume, whatever disk and controller it is on.
//!
//! A machine can have disks on virtio-blk, on SATA controllers (AHCI) and on NVMe.
//! Every disk found is probed, and exactly one volume becomes *the* data volume: a
//! FAT32 file system labelled `SPACEDATA`, either filling a whole disk (what `xtask`
//! makes) or in one partition of a disk, found through its GPT or MBR (what a disk
//! shared with an EFI system partition has). Everything else is listed and left
//! alone.
//!
//! The file system talks to this module only -- 512-byte sectors, read, write,
//! flush -- in sectors counted from the start of the volume, and every request is
//! checked against the volume's end before a driver sees it. So nothing the file
//! system does can reach another partition, or another disk: the EFI system
//! partition with the bootloader and the kernel image above all (ADR-0025).

use alloc::format;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

use spaceabi::error::Error;

use super::{ahci, nvme, virtio_blk};
use crate::sync::SpinLock;

pub const SECTOR_SIZE: u64 = 512;

/// The volume label the data volume carries in its FAT32 boot sector.
pub const DATA_LABEL: &[u8; 11] = b"SPACEDATA  ";

/// A disk, by controller.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Disk {
    Virtio,
    /// Handle from [`ahci::disks`].
    Ahci(usize),
    /// Controller handle and namespace, from [`nvme::namespaces`].
    Nvme(usize, u32),
}

/// Where the data volume is: `sectors` sectors of `disk` from `start`.
#[derive(Clone, Copy)]
struct Volume {
    disk: Disk,
    start: u64,
    sectors: u64,
}

static DATA: SpinLock<Option<Volume>> = SpinLock::new(None);

fn read_on(disk: Disk, lba: u64, buf: &mut [u8]) -> Result<(), Error> {
    match disk {
        Disk::Virtio => virtio_blk::read_sectors(lba, buf),
        Disk::Ahci(d) => ahci::read_sectors(d, lba, buf),
        Disk::Nvme(c, ns) => nvme::read_sectors(c, ns, lba, buf),
    }
}

fn sectors_on(disk: Disk) -> u64 {
    match disk {
        Disk::Virtio => virtio_blk::capacity_sectors(),
        Disk::Ahci(d) => ahci::capacity_sectors(d),
        Disk::Nvme(c, ns) => nvme::capacity_sectors(c, ns),
    }
}

fn name(disk: Disk) -> String {
    match disk {
        Disk::Virtio => String::from("virtio-blk"),
        Disk::Ahci(d) => ahci::name(d),
        Disk::Nvme(c, ns) => nvme::name(c, ns),
    }
}

/// What a disk holds, as far as choosing the data volume goes.
enum Probe {
    /// The data volume: `sectors` sectors from `start`, in `place` (for the log).
    Data { start: u64, sectors: u64, place: String },
    /// Something else, in a few words.
    Other(String),
}

fn signed(sector: &[u8]) -> bool {
    sector[510] == 0x55 && sector[511] == 0xAA
}

fn fat32(sector: &[u8]) -> bool {
    signed(sector) && &sector[82..90] == b"FAT32   "
}

fn data_volume(sector: &[u8]) -> bool {
    fat32(sector) && &sector[71..82] == DATA_LABEL
}

fn label(sector: &[u8]) -> String {
    let text: String =
        sector[71..82].iter().filter(|b| b.is_ascii_graphic() || **b == b' ').map(|&b| b as char).collect();
    String::from(text.trim_end())
}

fn le32(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}

fn le64(b: &[u8], at: usize) -> u64 {
    u64::from(le32(b, at)) | (u64::from(le32(b, at + 4)) << 32)
}

/// Whether the partition `start..=last` of a disk of `disk_sectors` holds the data
/// volume. A partition that does not fit on the disk is not looked into.
fn partition_is_data(disk: Disk, disk_sectors: u64, start: u64, last: u64) -> bool {
    if start == 0 || last < start || last >= disk_sectors {
        return false;
    }
    let mut first = [0u8; SECTOR_SIZE as usize];
    read_on(disk, start, &mut first).is_ok() && data_volume(&first)
}

fn probe(disk: Disk, disk_sectors: u64) -> Result<Probe, Error> {
    let mut first = [0u8; SECTOR_SIZE as usize];
    read_on(disk, 0, &mut first)?;
    if !signed(&first) {
        return Ok(Probe::Other(String::from("no boot signature")));
    }
    if fat32(&first) {
        if data_volume(&first) {
            return Ok(Probe::Data {
                start: 0,
                sectors: disk_sectors,
                place: String::from("the whole disk"),
            });
        }
        return Ok(Probe::Other(format!("a FAT32 volume labelled \"{}\"", label(&first))));
    }
    // A partition table. A GPT disk's MBR has a partition of type 0xEE covering it.
    let mbr: Vec<(u8, u64, u64)> = (0..4)
        .map(|i| {
            let e = 446 + i * 16;
            (first[e + 4], u64::from(le32(&first, e + 8)), u64::from(le32(&first, e + 12)))
        })
        .collect();
    if mbr.iter().any(|&(kind, _, _)| kind == 0xEE) {
        return probe_gpt(disk, disk_sectors);
    }
    let mut used = 0;
    for (i, &(kind, start, count)) in mbr.iter().enumerate() {
        // Empty, or an extended partition (a chain of further tables, not a volume).
        if kind == 0 || count == 0 || matches!(kind, 0x05 | 0x0F | 0x85) {
            continue;
        }
        used += 1;
        if partition_is_data(disk, disk_sectors, start, start + count - 1) {
            return Ok(Probe::Data { start, sectors: count, place: format!("MBR partition {}", i + 1) });
        }
    }
    Ok(Probe::Other(format!("MBR with {used} partition(s), none labelled SPACEDATA")))
}

fn probe_gpt(disk: Disk, disk_sectors: u64) -> Result<Probe, Error> {
    let mut header = [0u8; SECTOR_SIZE as usize];
    read_on(disk, 1, &mut header)?;
    if &header[0..8] != b"EFI PART" {
        return Ok(Probe::Other(String::from("a protective MBR without a GPT header")));
    }
    let table = le64(&header, 72);
    let count = le32(&header, 80) as usize;
    let entry = le32(&header, 84) as usize;
    // Entries are 128 << n bytes; tables of more than 128 of them are not made by
    // anything this has to read. Bounds make a corrupt header harmless.
    if !(128..=512).contains(&entry) || !entry.is_power_of_two() || count == 0 || count > 128 {
        return Ok(Probe::Other(String::from("a GPT header with an unreadable partition table")));
    }
    let bytes = (count * entry).next_multiple_of(SECTOR_SIZE as usize);
    if table < 2 || table.saturating_add((bytes as u64) / SECTOR_SIZE) > disk_sectors {
        return Ok(Probe::Other(String::from("a GPT header with its table off the disk")));
    }
    let mut entries = vec![0u8; bytes];
    read_on(disk, table, &mut entries)?;
    let mut used = 0;
    for i in 0..count {
        let e = &entries[i * entry..(i + 1) * entry];
        if e[0..16].iter().all(|&b| b == 0) {
            continue; // unused entry
        }
        used += 1;
        let (start, last) = (le64(e, 32), le64(e, 40));
        if partition_is_data(disk, disk_sectors, start, last) {
            return Ok(Probe::Data {
                start,
                sectors: last - start + 1,
                place: format!("GPT partition {}", i + 1),
            });
        }
    }
    Ok(Probe::Other(format!("GPT with {used} partition(s), none labelled SPACEDATA")))
}

/// Probe every disk the drivers found and choose the data volume.
pub fn init() {
    let mut disks = Vec::new();
    if virtio_blk::present() {
        disks.push(Disk::Virtio);
    }
    disks.extend(ahci::disks().into_iter().map(Disk::Ahci));
    disks.extend(nvme::namespaces().into_iter().map(|(c, ns)| Disk::Nvme(c, ns)));
    let mut chosen: Option<Volume> = None;
    for &disk in &disks {
        let disk_sectors = sectors_on(disk);
        let mib = disk_sectors * SECTOR_SIZE / (1024 * 1024);
        match probe(disk, disk_sectors) {
            Ok(Probe::Data { start, sectors, place }) if chosen.is_none() => {
                println!("[kernel] block: {} ({mib} MiB) holds the data volume ({place})", name(disk));
                chosen = Some(Volume { disk, start, sectors });
            }
            Ok(Probe::Data { place, .. }) => {
                println!(
                    "[kernel] block: {} ({mib} MiB): another data volume ({place}); only the first is used",
                    name(disk)
                )
            }
            Ok(Probe::Other(what)) => {
                println!("[kernel] block: {} ({mib} MiB): {what}; not used", name(disk))
            }
            Err(e) => println!("[kernel] block: {} ({mib} MiB): cannot be read ({e}); not used", name(disk)),
        }
    }
    if chosen.is_none() && !disks.is_empty() {
        println!("[kernel] block: no disk holds a volume labelled SPACEDATA");
    }
    *DATA.lock() = chosen;
}

fn volume() -> Result<Volume, Error> {
    (*DATA.lock()).ok_or(Error::NotFound)
}

/// Where on its disk the data volume's `lba` is, once `len` bytes from there are
/// known to stay inside the volume.
fn locate(v: &Volume, lba: u64, len: usize) -> Result<u64, Error> {
    if !(len as u64).is_multiple_of(SECTOR_SIZE) {
        return Err(Error::Invalid);
    }
    match lba.checked_add(len as u64 / SECTOR_SIZE) {
        Some(end) if end <= v.sectors => Ok(v.start + lba),
        _ => Err(Error::Invalid),
    }
}

pub fn present() -> bool {
    DATA.lock().is_some()
}

/// Which disk the data volume is on, for the log.
pub fn data_description() -> String {
    match *DATA.lock() {
        Some(v) => name(v.disk),
        None => String::from("none"),
    }
}

/// Sectors in the data volume, 0 when there is none.
pub fn capacity_sectors() -> u64 {
    volume().map_or(0, |v| v.sectors)
}

/// Read whole sectors of the data volume, `lba` counted from its start.
pub fn read_sectors(lba: u64, buf: &mut [u8]) -> Result<(), Error> {
    let v = volume()?;
    read_on(v.disk, locate(&v, lba, buf.len())?, buf)
}

/// Write whole sectors of the data volume, `lba` counted from its start.
pub fn write_sectors(lba: u64, buf: &[u8]) -> Result<(), Error> {
    let v = volume()?;
    let at = locate(&v, lba, buf.len())?;
    match v.disk {
        Disk::Virtio => virtio_blk::write_sectors(at, buf),
        Disk::Ahci(d) => ahci::write_sectors(d, at, buf),
        Disk::Nvme(c, ns) => nvme::write_sectors(c, ns, at, buf),
    }
}

/// Make earlier writes to the data volume durable.
pub fn flush() -> Result<(), Error> {
    match volume()?.disk {
        Disk::Virtio => virtio_blk::flush(),
        Disk::Ahci(d) => ahci::flush(d),
        Disk::Nvme(c, ns) => nvme::flush(c, ns),
    }
}

/// True when writes to the data volume would be refused (no volume, or a device
/// that says it is read-only).
pub fn read_only() -> bool {
    match *DATA.lock() {
        Some(Volume { disk: Disk::Virtio, .. }) => virtio_blk::read_only(),
        Some(_) => false,
        None => true,
    }
}
