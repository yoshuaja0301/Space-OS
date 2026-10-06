//! Virtual file system.
//!
//! One FAT32 volume, the data volume the block layer chose, exposed to user space through
//! `SYS_FS_OPEN` / `SYS_FS_READ` / `SYS_FS_STAT` behind the root `FS` capability
//! (requirement D01), and `SYS_FS_CREATE` / `SYS_FS_WRITE` behind a second root
//! right, `FS_WRITE`. The two rights are separate so that a process may be given
//! the ability to read the volume without the ability to change it.
//!
//! The MVP mounts exactly one volume: the data disk, on virtio-blk, SATA (AHCI) or
//! NVMe, whichever carries the `SPACEDATA` label (ADR-0025). The ESP the firmware
//! booted from never carries it, so nothing here can reach the bootloader or the
//! kernel image. A real mount table is still Developer Preview work.

pub mod fat32;

use alloc::vec::Vec;

use spaceabi::error::Error;
use spaceabi::syscall::FsCheck;

use self::fat32::{Fat32, FileNode};
use crate::dev::block;
use crate::sync::SpinLock;

static VOLUME: SpinLock<Option<Fat32>> = SpinLock::new(None);

/// Files somebody holds open, by where their directory entry is, and how many open
/// handles' worth. Such a file is not emptied, removed or replaced (`Busy`): its
/// holders read the clusters it was opened on, and those must stay theirs rather
/// than go back to the free pool and come back as somebody else's data.
static OPEN: SpinLock<Vec<(u64, u32, u32)>> = SpinLock::new(Vec::new());

fn held(node: &FileNode) -> bool {
    OPEN.lock().iter().any(|&(lba, off, _)| (lba, off) == (node.entry_lba, node.entry_off))
}

/// Count `node` as held open: its handle exists from now on (see
/// [`crate::proc::handles::OpenFile`], whose drop is [`closed`]).
fn hold(node: &FileNode) -> Result<(), Error> {
    let mut open = OPEN.lock();
    if let Some(e) = open.iter_mut().find(|e| (e.0, e.1) == (node.entry_lba, node.entry_off)) {
        e.2 += 1;
        return Ok(());
    }
    open.try_reserve(1).map_err(|_| Error::NoMemory)?;
    open.push((node.entry_lba, node.entry_off, 1));
    Ok(())
}

/// A handle on `node` is gone.
pub fn closed(node: &FileNode) {
    let mut open = OPEN.lock();
    if let Some(i) = open.iter().position(|e| (e.0, e.1) == (node.entry_lba, node.entry_off)) {
        if open[i].2 > 1 {
            open[i].2 -= 1;
        } else {
            open.swap_remove(i);
        }
    }
}

pub fn init() {
    if !block::present() {
        println!("[kernel] vfs: no data volume; file system unavailable");
        return;
    }
    match Fat32::mount() {
        Ok(fs) => {
            println!(
                "[kernel] vfs: FAT32 mounted from {} ({} byte clusters, {} MiB volume)",
                block::data_description(),
                fs.cluster_bytes(),
                block::capacity_sectors() * block::SECTOR_SIZE / (1024 * 1024)
            );
            *VOLUME.lock() = Some(fs);
            // Say it once, at mount: a volume the device refuses to write is a
            // different machine to reason about than one that accepts changes.
            println!("[kernel] vfs: volume is {}", if writable() { "writable" } else { "read-only" });
        }
        Err(e) => println!("[kernel] vfs: no usable FAT32 volume ({e})"),
    }
}

/// Sectors of the mounted volume, 0 when nothing is mounted.
pub fn volume_sectors() -> u64 {
    if VOLUME.lock().is_some() { block::capacity_sectors() } else { 0 }
}

/// Open `path`, counted as held until the handle made from it is dropped.
pub fn open(path: &str) -> Result<FileNode, Error> {
    let mut g = VOLUME.lock();
    let fs = g.as_mut().ok_or(Error::NotFound)?;
    let node = fs.open(path)?;
    hold(&node)?;
    Ok(node)
}

/// Entries of the directory at `path` (`/` for the volume root), at most `max`.
pub fn list(path: &str, max: usize) -> Result<alloc::vec::Vec<spaceabi::syscall::DirEntry>, Error> {
    let mut g = VOLUME.lock();
    let fs = g.as_mut().ok_or(Error::NotFound)?;
    fs.list(path, max)
}

pub fn read(node: &FileNode, offset: u64, buf: &mut [u8]) -> Result<usize, Error> {
    let mut g = VOLUME.lock();
    let fs = g.as_mut().ok_or(Error::NotFound)?;
    fs.read(node, offset, buf)
}

/// True when the volume is mounted and the device will accept writes.
pub fn writable() -> bool {
    VOLUME.lock().is_some() && !block::read_only()
}

/// Create `path` empty, or empty it if it is already there and nobody holds it
/// open; counted as held until the handle made from it is dropped.
pub fn create(path: &str) -> Result<FileNode, Error> {
    let mut g = VOLUME.lock();
    let fs = g.as_mut().ok_or(Error::NotFound)?;
    if let Ok(existing) = fs.open(path)
        && held(&existing)
    {
        return Err(Error::Busy);
    }
    let node = fs.create(path)?;
    hold(&node)?;
    Ok(node)
}

/// Remove the file at `path`, unless somebody holds it open.
pub fn remove(path: &str) -> Result<(), Error> {
    let mut g = VOLUME.lock();
    let fs = g.as_mut().ok_or(Error::NotFound)?;
    if held(&fs.open(path)?) {
        return Err(Error::Busy);
    }
    fs.remove(path)
}

/// Put `staging` in the place of `target` (see [`Fat32::replace`]), unless somebody
/// holds either open.
pub fn replace(staging: &str, target: &str) -> Result<(), Error> {
    let mut g = VOLUME.lock();
    let fs = g.as_mut().ok_or(Error::NotFound)?;
    if held(&fs.open(staging)?) {
        return Err(Error::Busy);
    }
    if let Ok(t) = fs.open(target)
        && held(&t)
    {
        return Err(Error::Busy);
    }
    fs.replace(staging, target)
}

/// Walk the volume (see [`Fat32::check`]).
pub fn check(repair: bool) -> Result<FsCheck, Error> {
    let mut g = VOLUME.lock();
    let fs = g.as_mut().ok_or(Error::NotFound)?;
    fs.check(repair)
}

/// Claim at most `clusters` more clusters (`None`: no limit).
pub fn set_space_limit(clusters: Option<u32>) -> Result<(), Error> {
    let mut g = VOLUME.lock();
    let fs = g.as_mut().ok_or(Error::NotFound)?;
    fs.set_space_limit(clusters);
    println!(
        "[kernel] vfs: injected limit: {}",
        match clusters {
            Some(n) => alloc::format!("{n} more cluster(s) may be claimed"),
            None => alloc::string::String::from("lifted"),
        }
    );
    Ok(())
}

/// Write `buf` at `offset`, growing the file if needed. `node` is updated in place.
pub fn write(node: &mut FileNode, offset: u64, buf: &[u8]) -> Result<usize, Error> {
    let mut g = VOLUME.lock();
    let fs = g.as_mut().ok_or(Error::NotFound)?;
    fs.write(node, offset, buf)
}
