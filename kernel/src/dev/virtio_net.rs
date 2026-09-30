//! VirtIO 1.0 network device (virtio-net over modern PCI), frames only.
//!
//! The kernel moves Ethernet frames and nothing else: ARP, IP, TCP and everything
//! above belong to the network service in user space (PRD §2, "layanan kompleks
//! ditempatkan pada user-space"). One lease on the device exists at a time; whoever
//! holds it receives every frame and may transmit any frame, which is why leasing
//! needs its own root right (`NET`).
//!
//! Like the block driver, this one never takes an interrupt. Completions are found
//! by looking at the used rings: the transmit ring when a frame is sent, the receive
//! ring on every timer tick, which wakes whoever waits on the device. That bounds
//! the receive latency by one tick (1 ms) and keeps the driver clear of level-
//! triggered PCI interrupt lines shared with devices nobody services -- a line that
//! stays asserted is an interrupt storm, and a storm is a hung machine.

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, Ordering};

use spaceabi::error::Error;
use spaceabi::syscall::{FRAME_MAX, FRAME_MIN, NetInfo};

use crate::dev::pci;
use crate::dev::virtio::{DESC_F_WRITE, DmaPage, SplitQueue, Transport, VIRTIO_VENDOR, mmio_read};
use crate::mm::PAGE_SIZE;
use crate::sched::WaitQueue;
use crate::sync::SpinLock;

/// Transitional (0x1000) and modern (0x1041) virtio-net device ids.
const NET_DEVICE_IDS: [u16; 2] = [0x1000, 0x1041];

/// Feature bit 5: the device has a fixed MAC address in its configuration space.
const F_MAC: u32 = 1 << 5;
/// Feature bit 16: the configuration space reports link status.
const F_STATUS: u32 = 1 << 16;
/// Link-up bit of the configuration `status` field.
const S_LINK_UP: u16 = 1;

/// `struct virtio_net_hdr` as laid out once `VIRTIO_F_VERSION_1` is negotiated:
/// always 12 bytes, `num_buffers` included. The driver asks for no offloads, so
/// every header it sends is zero and every header it receives is ignored.
const HDR_LEN: usize = 12;
/// Each buffer holds one header and one whole frame. Without mergeable receive
/// buffers the device needs room for the largest frame in every receive buffer.
const BUF_SIZE: usize = 2048;
const BUFS_PER_PAGE: usize = PAGE_SIZE as usize / BUF_SIZE;

const RX_QUEUE: u16 = 0;
const TX_QUEUE: u16 = 1;
/// Largest ring either queue gets; the device may offer less.
const QUEUE_MAX: u16 = 32;
/// A queue with fewer entries than this is refused rather than limped along with.
const QUEUE_MIN: u16 = 2;

/// Offsets in the device configuration space.
const CFG_MAC: u64 = 0;
const CFG_STATUS: u64 = 6;

/// A MAC for a device that will not say what its own is: locally administered
/// (bit 1 of the first octet), unicast, and recognisably ours.
const FALLBACK_MAC: [u8; 6] = [0x02, 0x53, 0x4F, 0x00, 0x00, 0x01];

struct Buf {
    phys: u64,
    virt: u64,
}

struct VirtioNet {
    transport: Transport,
    rx: SplitQueue,
    tx: SplitQueue,
    rx_bufs: Vec<Buf>,
    tx_bufs: Vec<Buf>,
    /// Transmit buffers not currently owned by the device.
    tx_free: Vec<u16>,
    mac: [u8; 6],
    has_status: bool,
    rx_frames: u64,
    tx_frames: u64,
    rx_dropped: u64,
    /// Kept alive for as long as the device uses them.
    _pages: Vec<DmaPage>,
}

// SAFETY: every field is an integer, an address of driver-owned memory or a queue
// that owns its page; all access is serialised by `NET`'s lock.
unsafe impl Send for VirtioNet {}

static NET: SpinLock<Option<VirtioNet>> = SpinLock::new(None);
/// Threads waiting for a frame (`SYS_WAIT_ANY` on a device lease).
static WAITERS: WaitQueue = WaitQueue::new();
/// Set while a lease exists.
static LEASED: AtomicBool = AtomicBool::new(false);

pub fn init() {
    match probe() {
        Ok(Some(msg)) => println!("[kernel] virtio-net: {msg}"),
        Ok(None) => println!("[kernel] virtio-net: no device present"),
        Err(e) => println!("[kernel] virtio-net: initialisation failed: {e}"),
    }
}

fn buffers(count: u16, pages: &mut Vec<DmaPage>) -> Result<Vec<Buf>, Error> {
    let mut bufs = Vec::new();
    bufs.try_reserve_exact(count as usize).map_err(|_| Error::NoMemory)?;
    while bufs.len() < count as usize {
        let page = DmaPage::new()?;
        for i in 0..BUFS_PER_PAGE {
            if bufs.len() < count as usize {
                let off = (i * BUF_SIZE) as u64;
                bufs.push(Buf { phys: page.phys + off, virt: page.virt + off });
            }
        }
        pages.push(page);
    }
    Ok(bufs)
}

fn probe() -> Result<Option<alloc::string::String>, Error> {
    let Some(addr) = pci::find(VIRTIO_VENDOR, &NET_DEVICE_IDS) else {
        return Ok(None);
    };
    addr.enable_memory_and_bus_master();
    // The driver polls: keep the function off the shared legacy interrupt line
    // entirely, so it can never hold one asserted.
    addr.disable_intx();
    let Some(t) = Transport::probe(addr, 8)? else {
        println!("[kernel] virtio-net: device has no modern virtio capabilities; ignored");
        return Ok(None);
    };
    if !t.reset() {
        println!("[kernel] virtio-net: device never finished resetting; ignored");
        return Ok(None);
    }
    // MAC and link status are information the driver wants; every offload is left
    // off, so frames cross the ring exactly as they go on the wire.
    let Some(features) = t.negotiate(F_MAC | F_STATUS) else {
        println!("[kernel] virtio-net: device refused the feature set or lacks VIRTIO_F_VERSION_1; ignored");
        t.set_status(0);
        return Ok(None);
    };
    if t.num_queues() < 2 {
        println!("[kernel] virtio-net: device has fewer than two queues; ignored");
        t.set_status(0);
        return Ok(None);
    }
    let Some(mut rx) = t.setup_queue(RX_QUEUE, QUEUE_MAX, QUEUE_MIN)? else {
        println!("[kernel] virtio-net: receive queue unusable; ignored");
        t.set_status(0);
        return Ok(None);
    };
    let Some(tx) = t.setup_queue(TX_QUEUE, QUEUE_MAX, QUEUE_MIN)? else {
        println!("[kernel] virtio-net: transmit queue unusable; ignored");
        t.set_status(0);
        return Ok(None);
    };
    let mut pages = Vec::new();
    let rx_bufs = buffers(rx.size, &mut pages)?;
    let tx_bufs = buffers(tx.size, &mut pages)?;
    let mut tx_free = Vec::new();
    tx_free.try_reserve_exact(tx.size as usize).map_err(|_| Error::NoMemory)?;
    tx_free.extend((0..tx.size).rev());

    // Every receive buffer goes to the device up front, one descriptor each.
    for (i, b) in rx_bufs.iter().enumerate() {
        rx.set_desc(i as u16, b.phys, BUF_SIZE as u32, DESC_F_WRITE, 0);
        rx.push_avail(i as u16);
    }

    let has_mac = features & F_MAC != 0 && t.device_cfg != 0;
    let has_status = features & F_STATUS != 0 && t.device_cfg != 0;
    let mac = if has_mac {
        let mut m = [0u8; 6];
        for (i, b) in m.iter_mut().enumerate() {
            *b = mmio_read::<u8>(t.device_cfg + CFG_MAC + i as u64);
        }
        m
    } else {
        FALLBACK_MAC
    };
    t.driver_ok();
    rx.notify();

    let dev = VirtioNet {
        transport: t,
        rx,
        tx,
        rx_bufs,
        tx_bufs,
        tx_free,
        mac,
        has_status,
        rx_frames: 0,
        tx_frames: 0,
        rx_dropped: 0,
        _pages: pages,
    };
    let msg = alloc::format!(
        "pci {:02x}:{:02x}.{} mac {} ({}), {} receive and {} transmit buffers, link {}, status {:#x}",
        addr.bus,
        addr.device,
        addr.function,
        fmt_mac(&dev.mac),
        if has_mac { "from device" } else { "fallback" },
        dev.rx.size,
        dev.tx.size,
        if dev.link_up() { "up" } else { "down" },
        dev.transport.status()
    );
    *NET.lock() = Some(dev);
    Ok(Some(msg))
}

pub fn fmt_mac(m: &[u8; 6]) -> alloc::string::String {
    alloc::format!("{:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}", m[0], m[1], m[2], m[3], m[4], m[5])
}

impl VirtioNet {
    fn link_up(&self) -> bool {
        if !self.has_status {
            return true;
        }
        mmio_read::<u16>(self.transport.device_cfg + CFG_STATUS) & S_LINK_UP != 0
    }

    /// Give receive buffer `id` back to the device.
    fn repost(&mut self, id: u16) {
        self.rx.push_avail(id);
        self.rx.notify();
    }

    /// Take the next received frame into `out`; its length, or `None` when nothing
    /// is waiting. Frames the device got wrong are dropped and counted.
    fn take_frame(&mut self, out: &mut [u8; FRAME_MAX]) -> Option<usize> {
        while let Some((id, len)) = self.rx.pop_used() {
            // The device names the buffer it filled. One we never gave it cannot be
            // trusted or reposted; count it and move on.
            if id >= self.rx_bufs.len() as u32 {
                self.rx_dropped += 1;
                continue;
            }
            let id = id as u16;
            let len = len as usize;
            if !(HDR_LEN + FRAME_MIN..=HDR_LEN + FRAME_MAX).contains(&len) {
                self.rx_dropped += 1;
                self.repost(id);
                continue;
            }
            let n = len - HDR_LEN;
            let b = &self.rx_bufs[id as usize];
            // SAFETY: a driver-owned DMA buffer the device has finished with; the
            // frame lies inside it (checked against BUF_SIZE via FRAME_MAX above).
            let src = unsafe { core::slice::from_raw_parts((b.virt + HDR_LEN as u64) as *const u8, n) };
            out[..n].copy_from_slice(src);
            self.repost(id);
            self.rx_frames += 1;
            return Some(n);
        }
        None
    }

    /// Reclaim transmit buffers the device has finished sending.
    fn reclaim_tx(&mut self) {
        while let Some((id, _)) = self.tx.pop_used() {
            if (id as usize) < self.tx_bufs.len() && !self.tx_free.contains(&(id as u16)) {
                // Capacity was reserved for every buffer at probe time.
                self.tx_free.push(id as u16);
            }
        }
    }
}

/// True when a network device is present and usable.
pub fn present() -> bool {
    NET.lock().is_some()
}

/// A lease on the device. Dropping the last reference releases it.
pub struct NicLease {
    _private: (),
}

impl Drop for NicLease {
    fn drop(&mut self) {
        LEASED.store(false, Ordering::Release);
    }
}

/// Take the device. Frames that arrived for a previous holder are discarded: a new
/// owner must not read traffic that was addressed to the old one's conversations.
pub fn lease() -> Result<Arc<NicLease>, Error> {
    if !present() {
        return Err(Error::NotFound);
    }
    if LEASED.swap(true, Ordering::AcqRel) {
        return Err(Error::Busy);
    }
    if let Some(dev) = NET.lock().as_mut() {
        let mut scratch = [0u8; FRAME_MAX];
        while dev.take_frame(&mut scratch).is_some() {
            dev.rx_frames -= 1;
            dev.rx_dropped += 1;
        }
    }
    Ok(Arc::new(NicLease { _private: () }))
}

pub fn info() -> Result<NetInfo, Error> {
    let g = NET.lock();
    let dev = g.as_ref().ok_or(Error::NotFound)?;
    Ok(NetInfo {
        mac: dev.mac,
        link_up: dev.link_up() as u8,
        _pad: 0,
        mtu: (FRAME_MAX - 14) as u32,
        frame_max: FRAME_MAX as u32,
        rx_frames: dev.rx_frames,
        tx_frames: dev.tx_frames,
        rx_dropped: dev.rx_dropped,
    })
}

/// Queue one frame for transmission. `WouldBlock` when every transmit buffer is
/// still owned by the device.
pub fn send(frame: &[u8]) -> Result<(), Error> {
    if frame.len() < FRAME_MIN {
        return Err(Error::Invalid);
    }
    if frame.len() > FRAME_MAX {
        return Err(Error::MsgSize);
    }
    let mut g = NET.lock();
    let dev = g.as_mut().ok_or(Error::NotFound)?;
    dev.reclaim_tx();
    let Some(id) = dev.tx_free.pop() else {
        return Err(Error::WouldBlock);
    };
    let b = &dev.tx_bufs[id as usize];
    // SAFETY: driver-owned DMA buffer not held by the device (it came off the free
    // list); header and frame fit in BUF_SIZE.
    unsafe {
        core::ptr::write_bytes(b.virt as *mut u8, 0, HDR_LEN);
        core::ptr::copy_nonoverlapping(frame.as_ptr(), (b.virt + HDR_LEN as u64) as *mut u8, frame.len());
    }
    let (phys, len) = (b.phys, (HDR_LEN + frame.len()) as u32);
    dev.tx.set_desc(id, phys, len, 0, 0);
    dev.tx.push_avail(id);
    dev.tx.notify();
    dev.tx_frames += 1;
    Ok(())
}

/// Take one received frame; `WouldBlock` when nothing is waiting.
pub fn recv(out: &mut [u8; FRAME_MAX]) -> Result<usize, Error> {
    let mut g = NET.lock();
    let dev = g.as_mut().ok_or(Error::NotFound)?;
    dev.take_frame(out).ok_or(Error::WouldBlock)
}

/// True when a received frame is waiting.
pub fn rx_ready() -> bool {
    NET.lock().as_ref().is_some_and(|d| d.rx.has_used())
}

/// Queue of threads waiting for a frame.
pub fn waiters() -> &'static WaitQueue {
    &WAITERS
}

/// Timer-tick hook (interrupts disabled): wake frame waiters when one has arrived.
pub fn poll_tick() {
    // `try_lock`: never spin in interrupt context. With one CPU and spinlocks that
    // disable interrupts the lock is always free here, but a busy lock only costs
    // one tick of latency, whereas spinning on it would cost the machine.
    let ready = match NET.try_lock() {
        Some(g) => g.as_ref().is_some_and(|d| d.rx.has_used()),
        None => false,
    };
    if ready {
        WAITERS.wake_all();
    }
}
