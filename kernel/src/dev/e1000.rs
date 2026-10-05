//! Intel 8254x and 82574 Ethernet ("e1000", "e1000e"), frames only (polled).
//!
//! The network card most hypervisors give a guest unless told otherwise -- QEMU's
//! `e1000` and `e1000e`, VirtualBox's "Intel PRO/1000 MT", VMware's `e1000` -- and
//! a long line of real Intel adapters. Like virtio-net, this driver moves Ethernet
//! frames and nothing else, never takes an interrupt, and is reached only through
//! the one lease `nic` hands out.
//!
//! Receive is a ring of descriptors, each with a 2 KiB buffer of its own: the card
//! writes a frame and sets the descriptor's DD bit, the driver copies the frame out
//! and hands the descriptor back by moving the tail. Transmit is a ring the same
//! size: a frame is copied into the next descriptor's buffer, marked end-of-packet
//! with the checksum left to the card, and the tail moves; DD says the card is done
//! with it. The card is reset first, so nothing the firmware set up survives.

use alloc::vec::Vec;

use spaceabi::error::Error;
use spaceabi::syscall::{FRAME_MAX, FRAME_MIN, NetInfo};

use super::dma::DmaPage;
use super::{pci, wait};
use crate::sync::SpinLock;

/// 82540EM (QEMU `e1000`, VirtualBox), 82545EM (VMware), 82574L (QEMU `e1000e`).
const DEVICE_IDS: [u16; 3] = [0x100E, 0x100F, 0x10D3];
const INTEL: u16 = 0x8086;

/// Descriptors per ring: 32 x 16 bytes, a multiple of the 128 bytes a ring must be.
const RING: usize = 32;
const BUF_SIZE: usize = 2048;
const BUFS_PER_PAGE: usize = 4096 / BUF_SIZE;
/// The card finishes a reset within microseconds; give it far longer.
const RESET_MS: u64 = 100;

// Registers.
const CTRL: u64 = 0x0000;
const STATUS: u64 = 0x0008;
const ICR: u64 = 0x00C0;
const IMC: u64 = 0x00D8;
const RCTL: u64 = 0x0100;
const TCTL: u64 = 0x0400;
const TIPG: u64 = 0x0410;
const RDBAL: u64 = 0x2800;
const RDBAH: u64 = 0x2804;
const RDLEN: u64 = 0x2808;
const RDH: u64 = 0x2810;
const RDT: u64 = 0x2818;
const TDBAL: u64 = 0x3800;
const TDBAH: u64 = 0x3804;
const TDLEN: u64 = 0x3808;
const TDH: u64 = 0x3810;
const TDT: u64 = 0x3818;
/// Multicast table: 128 registers.
const MTA: u64 = 0x5200;
const RAL0: u64 = 0x5400;
const RAH0: u64 = 0x5404;

const CTRL_ASDE: u32 = 1 << 5;
const CTRL_SLU: u32 = 1 << 6;
const CTRL_RST: u32 = 1 << 26;
const STATUS_LU: u32 = 1 << 1;
const RAH_AV: u32 = 1 << 31;
/// Receive on, broadcast accepted, 2 KiB buffers, CRC stripped.
const RCTL_EN: u32 = 1 << 1;
const RCTL_BAM: u32 = 1 << 15;
const RCTL_SECRC: u32 = 1 << 26;
/// Transmit on, short frames padded, collision threshold and distance for full duplex.
const TCTL_EN: u32 = 1 << 1;
const TCTL_PSP: u32 = 1 << 3;
const TCTL_CT: u32 = 0x0F << 4;
const TCTL_COLD: u32 = 0x40 << 12;
/// Inter-packet gap for copper (IPGT 10, IPGR1 8, IPGR2 6).
const TIPG_COPPER: u32 = 10 | (8 << 10) | (6 << 20);

const DESC_DD: u8 = 1 << 0;
const DESC_EOP: u8 = 1 << 1;
const TX_CMD_EOP: u8 = 1 << 0;
const TX_CMD_IFCS: u8 = 1 << 1;
const TX_CMD_RS: u8 = 1 << 3;

/// A MAC for a card that will not say what its own is: locally administered,
/// unicast, and recognisably ours (as virtio-net's).
const FALLBACK_MAC: [u8; 6] = [0x02, 0x53, 0x4F, 0x00, 0x00, 0x02];

struct Buf {
    phys: u64,
    virt: u64,
}

struct E1000 {
    pci: pci::Address,
    regs: u64,
    rx_ring: DmaPage,
    tx_ring: DmaPage,
    rx_bufs: Vec<Buf>,
    tx_bufs: Vec<Buf>,
    /// Next receive descriptor the card fills.
    rx_next: usize,
    /// Next transmit descriptor to use, and how many are still the card's.
    tx_next: usize,
    tx_busy: usize,
    /// Oldest transmit descriptor still the card's.
    tx_oldest: usize,
    mac: [u8; 6],
    rx_frames: u64,
    tx_frames: u64,
    rx_dropped: u64,
    /// Kept alive for as long as the card uses them.
    _pages: Vec<DmaPage>,
}

// SAFETY: integers and addresses of driver-owned memory, used under `CARD`'s lock.
unsafe impl Send for E1000 {}

static CARD: SpinLock<Option<E1000>> = SpinLock::new(None);

fn rd(addr: u64) -> u32 {
    // SAFETY: a register inside the mapped BAR.
    unsafe { core::ptr::read_volatile(addr as *const u32) }
}

fn wr(addr: u64, v: u32) {
    // SAFETY: as above.
    unsafe { core::ptr::write_volatile(addr as *mut u32, v) }
}

/// Receive descriptor fields, at `ring + 16 * i`: buffer address (8), length (2),
/// checksum (2), status (1), errors (1), special (2).
fn rx_desc(ring: &DmaPage, i: usize) -> *mut u8 {
    (ring.virt + 16 * i as u64) as *mut u8
}

/// Transmit descriptor fields: buffer address (8), length (2), CSO (1), command (1),
/// status (1), CSS (1), special (2).
fn tx_desc(ring: &DmaPage, i: usize) -> *mut u8 {
    (ring.virt + 16 * i as u64) as *mut u8
}

impl E1000 {
    fn reg(&self, off: u64) -> u64 {
        self.regs + off
    }

    fn link_up(&self) -> bool {
        rd(self.reg(STATUS)) & STATUS_LU != 0
    }

    /// Take the next received frame into `out`; its length, or `None` when nothing
    /// is waiting. Frames the card marked bad, or that do not fit, are dropped and
    /// counted.
    fn take_frame(&mut self, out: &mut [u8; FRAME_MAX]) -> Option<usize> {
        loop {
            let d = rx_desc(&self.rx_ring, self.rx_next);
            // SAFETY: descriptor `rx_next` of the driver's receive ring.
            let (status, errors, len) = unsafe {
                let status = core::ptr::read_volatile(d.add(12));
                if status & DESC_DD == 0 {
                    return None;
                }
                crate::arch::dma_rmb();
                (
                    status,
                    core::ptr::read_volatile(d.add(13)),
                    core::ptr::read_volatile(d.add(8) as *const u16),
                )
            };
            let len = usize::from(len);
            let i = self.rx_next;
            let good = status & DESC_EOP != 0 && errors == 0 && (FRAME_MIN..=FRAME_MAX).contains(&len);
            if good {
                let b = &self.rx_bufs[i];
                // SAFETY: the card is done with this buffer (DD) and wrote `len` bytes.
                let src = unsafe { core::slice::from_raw_parts(b.virt as *const u8, len) };
                out[..len].copy_from_slice(src);
                self.rx_frames += 1;
            } else {
                self.rx_dropped += 1;
            }
            // Hand the descriptor back: clear its status, then move the tail to it.
            // SAFETY: as above.
            unsafe { core::ptr::write_volatile(d.add(12), 0) };
            crate::arch::dma_wmb();
            wr(self.reg(RDT), i as u32);
            self.rx_next = (i + 1) % RING;
            if good {
                return Some(len);
            }
        }
    }

    /// True when a received frame is waiting.
    fn rx_ready(&self) -> bool {
        // SAFETY: descriptor `rx_next` of the driver's receive ring.
        unsafe { core::ptr::read_volatile(rx_desc(&self.rx_ring, self.rx_next).add(12)) & DESC_DD != 0 }
    }

    /// Reclaim transmit descriptors the card has finished with.
    fn reclaim_tx(&mut self) {
        while self.tx_busy > 0 {
            // SAFETY: descriptor `tx_oldest` of the driver's transmit ring.
            let status = unsafe { core::ptr::read_volatile(tx_desc(&self.tx_ring, self.tx_oldest).add(12)) };
            if status & DESC_DD == 0 {
                break;
            }
            self.tx_oldest = (self.tx_oldest + 1) % RING;
            self.tx_busy -= 1;
        }
    }

    /// Queue one frame. `WouldBlock` when every transmit descriptor is still the
    /// card's.
    fn send(&mut self, frame: &[u8]) -> Result<(), Error> {
        self.reclaim_tx();
        // One descriptor always stays free: a full ring would have head == tail,
        // which the card reads as empty.
        if self.tx_busy >= RING - 1 {
            return Err(Error::WouldBlock);
        }
        let i = self.tx_next;
        let b = &self.tx_bufs[i];
        let d = tx_desc(&self.tx_ring, i);
        // SAFETY: a driver-owned buffer the card is not using (reclaimed above) and
        // its descriptor; the frame fits in BUF_SIZE (checked by the caller).
        unsafe {
            core::ptr::copy_nonoverlapping(frame.as_ptr(), b.virt as *mut u8, frame.len());
            core::ptr::write_volatile(d as *mut u64, b.phys);
            core::ptr::write_volatile(d.add(8) as *mut u16, frame.len() as u16);
            core::ptr::write_volatile(d.add(10), 0);
            core::ptr::write_volatile(d.add(11), TX_CMD_EOP | TX_CMD_IFCS | TX_CMD_RS);
            core::ptr::write_volatile(d.add(12), 0);
            core::ptr::write_volatile(d.add(13), 0);
            core::ptr::write_volatile(d.add(14) as *mut u16, 0);
        }
        crate::arch::dma_mb();
        self.tx_next = (i + 1) % RING;
        self.tx_busy += 1;
        wr(self.reg(TDT), self.tx_next as u32);
        self.tx_frames += 1;
        Ok(())
    }

    /// Throw away every frame that has arrived.
    fn drain(&mut self) {
        let mut scratch = [0u8; FRAME_MAX];
        while self.take_frame(&mut scratch).is_some() {
            self.rx_frames -= 1;
            self.rx_dropped += 1;
        }
    }
}

fn bring_up(addr: pci::Address) -> Result<E1000, &'static str> {
    const NO_MEMORY: &str = "no memory for its rings";
    let bar = addr.bar_address(0).ok_or("no register BAR")?;
    addr.enable_memory_and_bus_master();
    addr.disable_intx();
    let regs = crate::mm::mmio::map(bar, 0x20000).map_err(|_| "no room to map its registers")?;

    // Reset, then keep every interrupt masked and clear what is pending.
    wr(regs + IMC, u32::MAX);
    wr(regs + CTRL, rd(regs + CTRL) | CTRL_RST);
    // The card must not be read for a moment after the reset starts.
    wait::pause(1);
    if !wait::until(RESET_MS, || rd(regs + CTRL) & CTRL_RST == 0) {
        return Err("it does not come out of reset");
    }
    wr(regs + IMC, u32::MAX);
    let _ = rd(regs + ICR);
    wr(regs + CTRL, rd(regs + CTRL) | CTRL_SLU | CTRL_ASDE);

    // The address the card loaded from its EEPROM, if it says it has one.
    let (ral, rah) = (rd(regs + RAL0), rd(regs + RAH0));
    let mut mac =
        [ral as u8, (ral >> 8) as u8, (ral >> 16) as u8, (ral >> 24) as u8, rah as u8, (rah >> 8) as u8];
    if rah & RAH_AV == 0 || mac == [0; 6] || mac[0] & 1 != 0 {
        mac = FALLBACK_MAC;
        let ral = u32::from_le_bytes([mac[0], mac[1], mac[2], mac[3]]);
        wr(regs + RAL0, ral);
        wr(regs + RAH0, u32::from(mac[4]) | (u32::from(mac[5]) << 8) | RAH_AV);
    }
    for i in 0..128 {
        wr(regs + MTA + 4 * i, 0);
    }

    let rx_ring = DmaPage::new().map_err(|_| NO_MEMORY)?;
    let tx_ring = DmaPage::new().map_err(|_| NO_MEMORY)?;
    let mut pages = Vec::new();
    let (mut rx_bufs, mut tx_bufs) = (Vec::new(), Vec::new());
    for bufs in [&mut rx_bufs, &mut tx_bufs] {
        bufs.try_reserve(RING).map_err(|_| NO_MEMORY)?;
        while bufs.len() < RING {
            let page = DmaPage::new().map_err(|_| NO_MEMORY)?;
            for k in 0..BUFS_PER_PAGE {
                let off = (k * BUF_SIZE) as u64;
                bufs.push(Buf { phys: page.phys + off, virt: page.virt + off });
            }
            pages.try_reserve(1).map_err(|_| NO_MEMORY)?;
            pages.push(page);
        }
    }

    // Receive ring: every descriptor points at its buffer; all but one are the card's.
    for (i, b) in rx_bufs.iter().enumerate() {
        // SAFETY: descriptor `i` of the new receive ring.
        unsafe { core::ptr::write_volatile(rx_desc(&rx_ring, i) as *mut u64, b.phys) };
    }
    wr(regs + RDBAL, rx_ring.phys as u32);
    wr(regs + RDBAH, (rx_ring.phys >> 32) as u32);
    wr(regs + RDLEN, (RING * 16) as u32);
    wr(regs + RDH, 0);
    wr(regs + RDT, (RING - 1) as u32);
    wr(regs + RCTL, RCTL_EN | RCTL_BAM | RCTL_SECRC);

    // Transmit ring: empty.
    wr(regs + TDBAL, tx_ring.phys as u32);
    wr(regs + TDBAH, (tx_ring.phys >> 32) as u32);
    wr(regs + TDLEN, (RING * 16) as u32);
    wr(regs + TDH, 0);
    wr(regs + TDT, 0);
    wr(regs + TIPG, TIPG_COPPER);
    wr(regs + TCTL, TCTL_EN | TCTL_PSP | TCTL_CT | TCTL_COLD);

    Ok(E1000 {
        pci: addr,
        regs,
        rx_ring,
        tx_ring,
        rx_bufs,
        tx_bufs,
        rx_next: 0,
        tx_next: 0,
        tx_busy: 0,
        tx_oldest: 0,
        mac,
        rx_frames: 0,
        tx_frames: 0,
        rx_dropped: 0,
        _pages: pages,
    })
}

/// Find the first supported card and bring it up.
pub fn init() {
    let Some(addr) = pci::find(INTEL, &DEVICE_IDS) else {
        return;
    };
    match bring_up(addr) {
        Ok(card) => {
            println!(
                "[kernel] e1000: {} (device {:04x}) ready, mac {}, link {}",
                card.pci,
                addr.device_id(),
                super::nic::fmt_mac(&card.mac),
                if card.link_up() { "up" } else { "down" }
            );
            *CARD.lock() = Some(card);
        }
        Err(e) => println!("[kernel] e1000: {addr}: {e}; not used"),
    }
}

pub fn present() -> bool {
    CARD.lock().is_some()
}

/// Throw away every frame that has arrived (a new lease starts empty).
pub fn drain() {
    if let Some(card) = CARD.lock().as_mut() {
        card.drain();
    }
}

pub fn info() -> Result<NetInfo, Error> {
    let g = CARD.lock();
    let card = g.as_ref().ok_or(Error::NotFound)?;
    Ok(NetInfo {
        mac: card.mac,
        link_up: card.link_up() as u8,
        _pad: 0,
        mtu: (FRAME_MAX - 14) as u32,
        frame_max: FRAME_MAX as u32,
        rx_frames: card.rx_frames,
        tx_frames: card.tx_frames,
        rx_dropped: card.rx_dropped,
    })
}

/// Queue one frame for transmission. `WouldBlock` when every transmit descriptor is
/// still the card's.
pub fn send(frame: &[u8]) -> Result<(), Error> {
    if frame.len() < FRAME_MIN {
        return Err(Error::Invalid);
    }
    if frame.len() > FRAME_MAX {
        return Err(Error::MsgSize);
    }
    CARD.lock().as_mut().ok_or(Error::NotFound)?.send(frame)
}

/// Take one received frame; `WouldBlock` when nothing is waiting.
pub fn recv(out: &mut [u8; FRAME_MAX]) -> Result<usize, Error> {
    CARD.lock().as_mut().ok_or(Error::NotFound)?.take_frame(out).ok_or(Error::WouldBlock)
}

/// True when a received frame is waiting.
pub fn rx_ready() -> bool {
    CARD.lock().as_ref().is_some_and(E1000::rx_ready)
}

/// [`rx_ready`] for the timer tick: a busy lock reads as "nothing yet".
pub fn rx_ready_nowait() -> bool {
    CARD.try_lock().is_some_and(|g| g.as_ref().is_some_and(E1000::rx_ready))
}
