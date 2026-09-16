//! Polled VirtIO 1.0 block driver (virtio-blk over modern PCI).
//!
//! Requests are submitted on virtqueue 0 and completed by polling the used ring:
//! the MVP needs a few hundred kilobytes read at boot, not throughput, and polling
//! keeps the driver free of interrupt-ordering hazards. A bounded poll turns a
//! wedged device into an error instead of a hang.

use alloc::vec::Vec;
use core::sync::atomic::{Ordering, fence};

use spaceabi::error::Error;
use x86_64::structures::paging::PhysFrame;

use crate::dev::pci;
use crate::mm::{PAGE_SIZE, frame, mmio, phys_to_virt};
use crate::sync::SpinLock;

const VIRTIO_VENDOR: u16 = 0x1AF4;
/// Transitional (0x1001) and modern (0x1042) virtio-blk device ids.
const BLK_DEVICE_IDS: [u16; 2] = [0x1001, 0x1042];

const CAP_VENDOR: u8 = 0x09;
const CFG_COMMON: u8 = 1;
const CFG_NOTIFY: u8 = 2;
const CFG_DEVICE: u8 = 4;

// virtio_pci_common_cfg field offsets (virtio 1.0, 4.1.4.3).
const CC_DEVICE_FEATURE_SELECT: u64 = 0x00;
const CC_DEVICE_FEATURE: u64 = 0x04;
const CC_DRIVER_FEATURE_SELECT: u64 = 0x08;
const CC_DRIVER_FEATURE: u64 = 0x0C;
const CC_NUM_QUEUES: u64 = 0x12;
const CC_DEVICE_STATUS: u64 = 0x14;
const CC_QUEUE_SELECT: u64 = 0x16;
const CC_QUEUE_SIZE: u64 = 0x18;
const CC_QUEUE_ENABLE: u64 = 0x1C;
const CC_QUEUE_NOTIFY_OFF: u64 = 0x1E;
const CC_QUEUE_DESC: u64 = 0x20;
const CC_QUEUE_DRIVER: u64 = 0x28;
const CC_QUEUE_DEVICE: u64 = 0x30;

const STATUS_ACKNOWLEDGE: u8 = 1;
const STATUS_DRIVER: u8 = 2;
const STATUS_DRIVER_OK: u8 = 4;
const STATUS_FEATURES_OK: u8 = 8;

/// Feature bit 32: the device speaks the virtio 1.0 (non-legacy) protocol.
const F_VERSION_1: u32 = 1 << 0; // in feature word 1

const DESC_F_NEXT: u16 = 1;
const DESC_F_WRITE: u16 = 2;

/// Largest queue we ask for. The device may offer less; the negotiated size is what
/// every ring index is taken modulo, and it bounds the descriptors a request may use.
const QUEUE_SIZE: u16 = 16;
/// Data descriptors per request: 8 pages = 32 KiB, leaving room for header+status.
const MAX_DATA_PAGES: usize = 8;
/// A request always needs a header and a status descriptor besides its data.
const DESC_OVERHEAD: usize = 2;
pub const SECTOR_SIZE: u64 = 512;
const POLL_LIMIT: u64 = 200_000_000;

#[repr(C)]
#[derive(Clone, Copy)]
struct Desc {
    addr: u64,
    len: u32,
    flags: u16,
    next: u16,
}

#[repr(C)]
struct BlkReqHeader {
    kind: u32,
    reserved: u32,
    sector: u64,
}

const BLK_T_IN: u32 = 0;
const BLK_T_OUT: u32 = 1;
const BLK_T_FLUSH: u32 = 4;

/// Feature bit 5 (word 0): the device is read-only. Writes must be refused rather
/// than sent and silently failed.
const F_BLK_RO: u32 = 1 << 5;
/// Feature bit 9 (word 0): the device understands a flush request. Without it there
/// is no way to ask the device to make earlier writes durable.
const F_BLK_FLUSH: u32 = 1 << 9;

/// One 4 KiB page of DMA memory, addressable by both CPU and device.
struct DmaPage {
    phys: u64,
    virt: u64,
}

impl DmaPage {
    fn new() -> Result<Self, Error> {
        let f: PhysFrame = frame::alloc_zeroed().ok_or(Error::NoMemory)?;
        let phys = f.start_address().as_u64();
        Ok(DmaPage { phys, virt: phys_to_virt(phys).as_u64() })
    }
}

struct VirtioBlk {
    common: u64,
    notify: u64,
    notify_multiplier: u32,
    queue_notify_off: u16,
    queue: DmaPage,
    header: DmaPage,
    data: Vec<DmaPage>,
    desc_off: u64,
    avail_off: u64,
    used_off: u64,
    /// Queue size the device agreed to; every ring index is taken modulo this.
    size: u16,
    /// Data pages one request may use: `size - DESC_OVERHEAD`, capped at `MAX_DATA_PAGES`.
    max_data_pages: usize,
    last_used: u16,
    avail_idx: u16,
    /// The device declared itself read-only; every write is refused up front.
    read_only: bool,
    /// The device accepted `VIRTIO_BLK_F_FLUSH`, so durability can be requested.
    can_flush: bool,
    /// Set when a request timed out. The device has been reset and is never used
    /// again: a late completion would otherwise be mistaken for the next request's.
    failed: bool,
    pub capacity_sectors: u64,
}

// SAFETY: every field is either a plain integer or a physical/virtual address of
// memory owned exclusively by the driver, which is serialised by `BLK`'s lock.
unsafe impl Send for VirtioBlk {}

static BLK: SpinLock<Option<VirtioBlk>> = SpinLock::new(None);

fn mmio_read<T: Copy>(addr: u64) -> T {
    // SAFETY: `addr` is inside a device MMIO mapping created by `mmio::map`.
    unsafe { core::ptr::read_volatile(addr as *const T) }
}

fn mmio_write<T: Copy>(addr: u64, value: T) {
    // SAFETY: as above.
    unsafe { core::ptr::write_volatile(addr as *mut T, value) }
}

impl VirtioBlk {
    fn status(&self) -> u8 {
        mmio_read::<u8>(self.common + CC_DEVICE_STATUS)
    }
}

fn desc_ptr(q: &VirtioBlk, i: usize) -> *mut Desc {
    (q.queue.virt + q.desc_off + (i * core::mem::size_of::<Desc>()) as u64) as *mut Desc
}

fn avail_flags_ptr(q: &VirtioBlk) -> *mut u16 {
    (q.queue.virt + q.avail_off) as *mut u16
}

fn avail_idx_ptr(q: &VirtioBlk) -> *mut u16 {
    (q.queue.virt + q.avail_off + 2) as *mut u16
}

fn avail_ring_ptr(q: &VirtioBlk, i: usize) -> *mut u16 {
    (q.queue.virt + q.avail_off + 4 + (i * 2) as u64) as *mut u16
}

fn used_idx_ptr(q: &VirtioBlk) -> *const u16 {
    (q.queue.virt + q.used_off + 2) as *const u16
}

/// Probe the PCI bus and bring the first virtio-blk device up.
pub fn init() {
    match probe() {
        Ok(Some(cap)) => println!(
            "[kernel] virtio-blk: ready, {} MiB ({} sectors)",
            cap * SECTOR_SIZE / (1024 * 1024),
            cap
        ),
        Ok(None) => println!("[kernel] virtio-blk: no device present"),
        Err(e) => println!("[kernel] virtio-blk: initialisation failed: {e}"),
    }
}

fn probe() -> Result<Option<u64>, Error> {
    let Some(addr) = pci::find(VIRTIO_VENDOR, &BLK_DEVICE_IDS) else {
        return Ok(None);
    };
    addr.enable_memory_and_bus_master();

    let mut common = 0u64;
    let mut notify = 0u64;
    let mut notify_multiplier = 0u32;
    let mut device_cfg = 0u64;
    for (id, off) in addr.capabilities() {
        if id != CAP_VENDOR {
            continue;
        }
        let cfg_type = addr.read8(off + 3);
        let bar = addr.read8(off + 4);
        let cap_offset = addr.read32(off + 8) as u64;
        let cap_len = addr.read32(off + 12) as u64;
        let Some(bar_base) = addr.bar_address(bar) else { continue };
        match cfg_type {
            CFG_COMMON => common = mmio::map(bar_base + cap_offset, cap_len.max(0x38))?,
            CFG_NOTIFY => {
                notify_multiplier = addr.read32(off + 16);
                notify = mmio::map(bar_base + cap_offset, cap_len.max(4))?;
            }
            CFG_DEVICE => device_cfg = mmio::map(bar_base + cap_offset, cap_len.max(8))?,
            _ => {}
        }
    }
    if common == 0 || notify == 0 {
        println!("[kernel] virtio-blk: device has no modern virtio capabilities; ignored");
        return Ok(None);
    }

    // Reset, then announce the driver.
    mmio_write::<u8>(common + CC_DEVICE_STATUS, 0);
    while mmio_read::<u8>(common + CC_DEVICE_STATUS) != 0 {
        core::hint::spin_loop();
    }
    mmio_write::<u8>(common + CC_DEVICE_STATUS, STATUS_ACKNOWLEDGE);
    mmio_write::<u8>(common + CC_DEVICE_STATUS, STATUS_ACKNOWLEDGE | STATUS_DRIVER);

    // Negotiate: VIRTIO_F_VERSION_1 is required. From the block features we take
    // only FLUSH, and only if offered -- writing without a way to ask for durability
    // would make "it survived the reboot" a property of the host's cache, not of this
    // driver. RO is not a feature to accept but a fact to record: the device is
    // telling us every write will fail.
    mmio_write::<u32>(common + CC_DEVICE_FEATURE_SELECT, 1);
    let device_features_hi = mmio_read::<u32>(common + CC_DEVICE_FEATURE);
    if device_features_hi & F_VERSION_1 == 0 {
        println!("[kernel] virtio-blk: device does not offer VIRTIO_F_VERSION_1; ignored");
        return Ok(None);
    }
    mmio_write::<u32>(common + CC_DEVICE_FEATURE_SELECT, 0);
    let device_features_lo = mmio_read::<u32>(common + CC_DEVICE_FEATURE);
    let read_only = device_features_lo & F_BLK_RO != 0;
    let can_flush = device_features_lo & F_BLK_FLUSH != 0;
    mmio_write::<u32>(common + CC_DRIVER_FEATURE_SELECT, 0);
    mmio_write::<u32>(common + CC_DRIVER_FEATURE, device_features_lo & F_BLK_FLUSH);
    mmio_write::<u32>(common + CC_DRIVER_FEATURE_SELECT, 1);
    mmio_write::<u32>(common + CC_DRIVER_FEATURE, F_VERSION_1);
    mmio_write::<u8>(common + CC_DEVICE_STATUS, STATUS_ACKNOWLEDGE | STATUS_DRIVER | STATUS_FEATURES_OK);
    if mmio_read::<u8>(common + CC_DEVICE_STATUS) & STATUS_FEATURES_OK == 0 {
        println!("[kernel] virtio-blk: device rejected the feature set");
        return Ok(None);
    }
    if mmio_read::<u16>(common + CC_NUM_QUEUES) == 0 {
        println!("[kernel] virtio-blk: device exposes no queues");
        return Ok(None);
    }

    // Queue 0.
    mmio_write::<u16>(common + CC_QUEUE_SELECT, 0);
    let max_size = mmio_read::<u16>(common + CC_QUEUE_SIZE);
    if max_size == 0 {
        println!("[kernel] virtio-blk: queue 0 unavailable");
        return Ok(None);
    }
    let size = QUEUE_SIZE.min(max_size);
    if (size as usize) <= DESC_OVERHEAD {
        println!("[kernel] virtio-blk: queue size {size} is too small for a request; ignored");
        return Ok(None);
    }
    mmio_write::<u16>(common + CC_QUEUE_SIZE, size);
    // The device may clamp the size further; take what it reports back.
    let size = mmio_read::<u16>(common + CC_QUEUE_SIZE);
    if (size as usize) <= DESC_OVERHEAD || size > QUEUE_SIZE {
        println!("[kernel] virtio-blk: device settled on queue size {size}; ignored");
        return Ok(None);
    }
    let max_data_pages = MAX_DATA_PAGES.min(size as usize - DESC_OVERHEAD);
    let queue_notify_off = mmio_read::<u16>(common + CC_QUEUE_NOTIFY_OFF);

    let queue = DmaPage::new()?;
    let header = DmaPage::new()?;
    let mut data = Vec::new();
    for _ in 0..max_data_pages {
        data.push(DmaPage::new()?);
    }
    // One page holds the whole split queue for size <= 16:
    // desc 16*16 = 256 B, avail 4 + 2*16 + 2, used 4 + 8*16 + 2.
    let desc_off = 0u64;
    let avail_off = 16 * size as u64;
    let used_off = (avail_off + 6 + 2 * size as u64).div_ceil(16) * 16;
    if used_off + 6 + 8 * size as u64 > PAGE_SIZE {
        println!("[kernel] virtio-blk: queue size {size} does not fit in one page; ignored");
        return Ok(None);
    }

    mmio_write::<u64>(common + CC_QUEUE_DESC, queue.phys + desc_off);
    mmio_write::<u64>(common + CC_QUEUE_DRIVER, queue.phys + avail_off);
    mmio_write::<u64>(common + CC_QUEUE_DEVICE, queue.phys + used_off);
    mmio_write::<u16>(common + CC_QUEUE_ENABLE, 1);
    mmio_write::<u8>(
        common + CC_DEVICE_STATUS,
        STATUS_ACKNOWLEDGE | STATUS_DRIVER | STATUS_FEATURES_OK | STATUS_DRIVER_OK,
    );

    let capacity_sectors = if device_cfg != 0 { mmio_read::<u64>(device_cfg) } else { 0 };
    let dev = VirtioBlk {
        common,
        notify,
        notify_multiplier,
        queue_notify_off,
        queue,
        header,
        data,
        desc_off,
        avail_off,
        used_off,
        size,
        max_data_pages,
        last_used: 0,
        avail_idx: 0,
        read_only,
        can_flush,
        failed: false,
        capacity_sectors,
    };
    let status = dev.status();
    println!(
        "[kernel] virtio-blk: pci {:02x}:{:02x}.{} queue size {} (max {}), {} data pages/request, {}, status {:#x}",
        addr.bus,
        addr.device,
        addr.function,
        size,
        max_size,
        max_data_pages,
        match (read_only, can_flush) {
            (true, _) => "read-only",
            (false, true) => "writable with flush",
            (false, false) => "writable, no flush",
        },
        status
    );
    *BLK.lock() = Some(dev);
    Ok(Some(capacity_sectors))
}

/// True when a block device is present.
pub fn present() -> bool {
    BLK.lock().as_ref().is_some_and(|d| !d.failed)
}

/// Capacity in 512-byte sectors (0 when absent).
pub fn capacity_sectors() -> u64 {
    BLK.lock().as_ref().map(|d| d.capacity_sectors).unwrap_or(0)
}

/// Read `buf.len()` bytes starting at sector `lba`. `buf.len()` must be a multiple
/// of [`SECTOR_SIZE`].
pub fn read_sectors(lba: u64, buf: &mut [u8]) -> Result<(), Error> {
    if buf.is_empty() || !(buf.len() as u64).is_multiple_of(SECTOR_SIZE) {
        return Err(Error::Invalid);
    }
    let mut guard = BLK.lock();
    let dev = guard.as_mut().ok_or(Error::NotFound)?;
    if dev.failed {
        return Err(Error::Fault);
    }
    let total_sectors = buf.len() as u64 / SECTOR_SIZE;
    if lba
        .checked_add(total_sectors)
        .is_none_or(|end| dev.capacity_sectors != 0 && end > dev.capacity_sectors)
    {
        return Err(Error::Invalid);
    }
    let chunk_bytes = dev.max_data_pages as u64 * PAGE_SIZE;
    let mut done = 0u64;
    while done < buf.len() as u64 {
        let this = chunk_bytes.min(buf.len() as u64 - done);
        request(dev, BLK_T_IN, lba + done / SECTOR_SIZE, this)?;
        let pages = (this as usize).div_ceil(PAGE_SIZE as usize);
        for p in 0..pages {
            let off = p * PAGE_SIZE as usize;
            let n = (this as usize - off).min(PAGE_SIZE as usize);
            // SAFETY: DMA page the device just filled; `n` bytes inside one page.
            let src = unsafe { core::slice::from_raw_parts(dev.data[p].virt as *const u8, n) };
            buf[done as usize + off..done as usize + off + n].copy_from_slice(src);
        }
        done += this;
    }
    Ok(())
}

/// Submit one request and poll for its completion.
///
/// `kind` decides who owns the data pages: the device fills them for a read and
/// reads them for a write, which is the only difference between the two chains.
/// A flush carries no data at all.
/// True when the device declared itself read-only, so no write will ever succeed.
pub fn read_only() -> bool {
    BLK.lock().as_ref().is_some_and(|d| d.read_only)
}

/// Write `buf.len()` bytes starting at sector `lba`. `buf.len()` must be a multiple
/// of [`SECTOR_SIZE`].
///
/// The bytes are copied into the driver's DMA pages first: the device must never be
/// pointed at kernel memory the caller happens to own.
pub fn write_sectors(lba: u64, buf: &[u8]) -> Result<(), Error> {
    if buf.is_empty() || !(buf.len() as u64).is_multiple_of(SECTOR_SIZE) {
        return Err(Error::Invalid);
    }
    let mut guard = BLK.lock();
    let dev = guard.as_mut().ok_or(Error::NotFound)?;
    if dev.failed {
        return Err(Error::Fault);
    }
    if dev.read_only {
        return Err(Error::Denied);
    }
    let total_sectors = buf.len() as u64 / SECTOR_SIZE;
    if lba
        .checked_add(total_sectors)
        .is_none_or(|end| dev.capacity_sectors != 0 && end > dev.capacity_sectors)
    {
        return Err(Error::Invalid);
    }
    let chunk_bytes = dev.max_data_pages as u64 * PAGE_SIZE;
    let mut done = 0u64;
    while done < buf.len() as u64 {
        let this = chunk_bytes.min(buf.len() as u64 - done);
        let pages = (this as usize).div_ceil(PAGE_SIZE as usize);
        for p in 0..pages {
            let off = p * PAGE_SIZE as usize;
            let n = (this as usize - off).min(PAGE_SIZE as usize);
            // SAFETY: driver-owned DMA page; `n` bytes inside one page.
            let dst = unsafe { core::slice::from_raw_parts_mut(dev.data[p].virt as *mut u8, n) };
            dst.copy_from_slice(&buf[done as usize + off..done as usize + off + n]);
        }
        request(dev, BLK_T_OUT, lba + done / SECTOR_SIZE, this)?;
        done += this;
    }
    Ok(())
}

/// Ask the device to make earlier writes durable.
///
/// Without `VIRTIO_BLK_F_FLUSH` there is nothing to ask, and saying so is better
/// than pretending: the caller learns that "written" means "handed to the host".
pub fn flush() -> Result<(), Error> {
    let mut guard = BLK.lock();
    let dev = guard.as_mut().ok_or(Error::NotFound)?;
    if dev.failed {
        return Err(Error::Fault);
    }
    if dev.read_only || !dev.can_flush {
        return Ok(());
    }
    request(dev, BLK_T_FLUSH, 0, 0)
}

fn request(dev: &mut VirtioBlk, kind: u32, lba: u64, bytes: u64) -> Result<(), Error> {
    let pages = if kind == BLK_T_FLUSH { 0 } else { (bytes as usize).div_ceil(PAGE_SIZE as usize) };
    // The caller chunks by `max_data_pages`; never write past the descriptor table.
    if (pages == 0 && kind != BLK_T_FLUSH) || pages > dev.max_data_pages {
        return Err(Error::Invalid);
    }
    // SAFETY: the header page is driver-owned DMA memory.
    unsafe {
        core::ptr::write_volatile(
            dev.header.virt as *mut BlkReqHeader,
            BlkReqHeader { kind, reserved: 0, sector: lba },
        );
        // Status byte lives after the header in the same page.
        core::ptr::write_volatile((dev.header.virt + 16) as *mut u8, 0xFF);
    }

    let status_desc = (pages + 1) as u16;
    // A read has the device fill the data pages; a write has it read them, so the
    // WRITE flag (which means "device writes here") belongs only to the read path.
    let data_flags = if kind == BLK_T_IN { DESC_F_WRITE | DESC_F_NEXT } else { DESC_F_NEXT };
    // SAFETY: descriptor slots inside the queue page, indices below QUEUE_SIZE.
    unsafe {
        core::ptr::write_volatile(
            desc_ptr(dev, 0),
            Desc { addr: dev.header.phys, len: 16, flags: DESC_F_NEXT, next: status_desc.min(1) },
        );
        for p in 0..pages {
            let len = ((bytes as usize - p * PAGE_SIZE as usize).min(PAGE_SIZE as usize)) as u32;
            let last = p + 1 == pages;
            core::ptr::write_volatile(
                desc_ptr(dev, p + 1),
                Desc {
                    addr: dev.data[p].phys,
                    len,
                    flags: data_flags,
                    next: if last { status_desc } else { (p + 2) as u16 },
                },
            );
        }
        core::ptr::write_volatile(
            desc_ptr(dev, status_desc as usize),
            Desc { addr: dev.header.phys + 16, len: 1, flags: DESC_F_WRITE, next: 0 },
        );
        let slot = (dev.avail_idx % dev.size) as usize;
        core::ptr::write_volatile(avail_ring_ptr(dev, slot), 0);
        core::ptr::write_volatile(avail_flags_ptr(dev), 0);
        fence(Ordering::SeqCst);
        dev.avail_idx = dev.avail_idx.wrapping_add(1);
        core::ptr::write_volatile(avail_idx_ptr(dev), dev.avail_idx);
    }
    fence(Ordering::SeqCst);
    let notify_addr = dev.notify + dev.queue_notify_off as u64 * dev.notify_multiplier as u64;
    mmio_write::<u16>(notify_addr, 0);

    let mut spins = 0u64;
    loop {
        fence(Ordering::SeqCst);
        // SAFETY: used ring inside the queue page.
        let used = unsafe { core::ptr::read_volatile(used_idx_ptr(dev)) };
        if used != dev.last_used {
            dev.last_used = used;
            break;
        }
        spins += 1;
        if spins > POLL_LIMIT {
            // The request may still be in flight: the device could write into the
            // data pages, and bump the used index, long after we gave up. Reset it so
            // it stops touching driver memory, and never issue another request — a
            // late completion would otherwise be read as the next request's data.
            mmio_write::<u8>(dev.common + CC_DEVICE_STATUS, 0);
            dev.failed = true;
            println!("[kernel] virtio-blk: request timed out; device reset and taken offline");
            return Err(Error::WouldBlock);
        }
        core::hint::spin_loop();
    }
    // SAFETY: status byte written by the device.
    let status = unsafe { core::ptr::read_volatile((dev.header.virt + 16) as *const u8) };
    if status != 0 { Err(Error::Fault) } else { Ok(()) }
}
