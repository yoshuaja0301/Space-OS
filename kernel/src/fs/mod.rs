//! Virtual file system.
//!
//! One FAT32 volume on the VirtIO block device, exposed to user space through
//! `SYS_FS_OPEN` / `SYS_FS_READ` / `SYS_FS_STAT` behind the root `FS` capability
//! (requirement D01), and `SYS_FS_CREATE` / `SYS_FS_WRITE` behind a second root
//! right, `FS_WRITE`. The two rights are separate so that a process may be given
//! the ability to read the volume without the ability to change it.
//!
//! The MVP mounts exactly one volume: the data disk. The ESP the firmware booted
//! from is not a virtio device, so nothing here can reach the bootloader or the
//! kernel image. A real mount table is still Developer Preview work.

pub mod fat32;

use spaceabi::error::Error;

use self::fat32::{Fat32, FileNode};
use crate::dev::virtio_blk;
use crate::sync::SpinLock;

static VOLUME: SpinLock<Option<Fat32>> = SpinLock::new(None);

pub fn init() {
    if !virtio_blk::present() {
        println!("[kernel] vfs: no block device; file system unavailable");
        return;
    }
    match Fat32::mount() {
        Ok(fs) => {
            println!(
                "[kernel] vfs: FAT32 mounted from virtio-blk ({} byte clusters, {} MiB volume)",
                fs.cluster_bytes(),
                virtio_blk::capacity_sectors() * virtio_blk::SECTOR_SIZE / (1024 * 1024)
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
    if VOLUME.lock().is_some() { virtio_blk::capacity_sectors() } else { 0 }
}

pub fn open(path: &str) -> Result<FileNode, Error> {
    let mut g = VOLUME.lock();
    let fs = g.as_mut().ok_or(Error::NotFound)?;
    fs.open(path)
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
    VOLUME.lock().is_some() && !virtio_blk::read_only()
}

/// Create `path` empty, or empty it if it already exists.
pub fn create(path: &str) -> Result<FileNode, Error> {
    let mut g = VOLUME.lock();
    let fs = g.as_mut().ok_or(Error::NotFound)?;
    fs.create(path)
}

/// Write `buf` at `offset`, growing the file if needed. `node` is updated in place.
pub fn write(node: &mut FileNode, offset: u64, buf: &[u8]) -> Result<usize, Error> {
    let mut g = VOLUME.lock();
    let fs = g.as_mut().ok_or(Error::NotFound)?;
    fs.write(node, offset, buf)
}
