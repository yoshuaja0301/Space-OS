//! Virtual file system.
//!
//! One read-only FAT32 volume on the VirtIO block device, exposed to user space
//! through `SYS_FS_OPEN` / `SYS_FS_READ` / `SYS_FS_STAT` behind the root `FS`
//! capability (requirement D01). The MVP mounts exactly one volume; a real mount
//! table and writable filesystems are Developer Preview work.

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
        }
        Err(e) => println!("[kernel] vfs: no usable FAT32 volume ({e})"),
    }
}

pub fn open(path: &str) -> Result<FileNode, Error> {
    let mut g = VOLUME.lock();
    let fs = g.as_mut().ok_or(Error::NotFound)?;
    fs.open(path)
}

pub fn read(node: &FileNode, offset: u64, buf: &mut [u8]) -> Result<usize, Error> {
    let mut g = VOLUME.lock();
    let fs = g.as_mut().ok_or(Error::NotFound)?;
    fs.read(node, offset, buf)
}
