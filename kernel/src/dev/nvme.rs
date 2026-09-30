//! NVMe disks (polled).
//!
//! Each controller is reset -- which also ends whatever the firmware left it doing,
//! in memory the kernel has since taken back -- and brought up with one admin queue
//! pair, asked who it is and which namespaces it has (IDENTIFY), and given one I/O
//! queue pair. Commands are 64-byte submission entries; completion is seen by the
//! phase bit of the completion entry at the queue's head, so nothing interrupts.
//! Transfers go through a bounce buffer of eight pages described by PRP1 and a PRP
//! list; FLUSH makes writes durable. Only namespaces with 512-byte blocks and no
//! metadata are used (the file system works in 512-byte sectors); others are
//! reported and left alone. A command that times out takes the controller out of
//! use for good: a late completion would be taken for the next command's.

use alloc::string::String;
use alloc::vec::Vec;

use spaceabi::error::Error;

use super::dma::DmaPage;
use super::{pci, wait};
use crate::sync::SpinLock;

const SECTOR: usize = 512;
const DATA_PAGES: usize = 8;
const SECTORS_PER_COMMAND: usize = DATA_PAGES * 4096 / SECTOR;
/// Entries in each queue (a page of submission entries holds 64).
const ADMIN_DEPTH: u16 = 16;
const IO_DEPTH: u16 = 16;
/// Namespaces looked at per controller.
const MAX_NAMESPACES: u32 = 16;
/// Registers and the doorbells of queues 0 and 1.
const MAP_BYTES: u64 = 0x2000;
/// One command, a cache flush of a slow drive included.
const COMMAND_MS: u64 = 30_000;

// Controller registers.
const REG_CAP: u64 = 0x00;
const REG_VS: u64 = 0x08;
const REG_INTMS: u64 = 0x0C;
const REG_CC: u64 = 0x14;
const REG_CSTS: u64 = 0x1C;
const REG_AQA: u64 = 0x24;
const REG_ASQ: u64 = 0x28;
const REG_ACQ: u64 = 0x30;
const CC_EN: u32 = 1 << 0;
/// I/O completion entries of 2^4 bytes, submission entries of 2^6, 4 KiB pages,
/// the NVM command set.
const CC_IO_ENTRY_SIZES: u32 = (4 << 20) | (6 << 16);
const CSTS_RDY: u32 = 1 << 0;
const CSTS_CFS: u32 = 1 << 1;

const ADMIN_CREATE_SQ: u8 = 0x01;
const ADMIN_CREATE_CQ: u8 = 0x05;
const ADMIN_IDENTIFY: u8 = 0x06;
const IO_FLUSH: u8 = 0x00;
const IO_WRITE: u8 = 0x01;
const IO_READ: u8 = 0x02;

struct Queue {
    sq: DmaPage,
    cq: DmaPage,
    depth: u16,
    tail: u16,
    head: u16,
    phase: bool,
    sq_doorbell: u64,
    cq_doorbell: u64,
    next_cid: u16,
}

struct Namespace {
    id: u32,
    sectors: u64,
}

struct Controller {
    pci: pci::Address,
    /// Not used after bring-up, but the controller still points at its pages.
    #[allow(dead_code)]
    admin: Queue,
    io: Queue,
    data: Vec<DmaPage>,
    /// Holds the PRP list of transfers longer than two pages.
    prp_list: DmaPage,
    namespaces: Vec<Namespace>,
    failed: bool,
}

// SAFETY: plain integers and addresses of memory owned by the driver, used under
// the `CONTROLLERS` lock only.
unsafe impl Send for Controller {}

/// Every controller that came up. The index is the controller's handle.
static CONTROLLERS: SpinLock<Vec<Controller>> = SpinLock::new(Vec::new());

fn rd32(addr: u64) -> u32 {
    // SAFETY: an MMIO register inside a mapped BAR.
    unsafe { core::ptr::read_volatile(addr as *const u32) }
}

fn wr32(addr: u64, v: u32) {
    // SAFETY: as above.
    unsafe { core::ptr::write_volatile(addr as *mut u32, v) }
}

fn rd64(addr: u64) -> u64 {
    u64::from(rd32(addr)) | (u64::from(rd32(addr + 4)) << 32)
}

fn wr64(addr: u64, v: u64) {
    wr32(addr, v as u32);
    wr32(addr + 4, (v >> 32) as u32);
}

/// One submission entry: opcode, namespace, PRP1, PRP2 and command dwords 10-12.
#[derive(Default)]
struct Command {
    opcode: u8,
    nsid: u32,
    prp1: u64,
    prp2: u64,
    cdw10: u32,
    cdw11: u32,
    cdw12: u32,
}

impl Queue {
    fn new(regs: u64, qid: u16, stride: u64, depth: u16) -> Result<Queue, Error> {
        Ok(Queue {
            sq: DmaPage::new()?,
            cq: DmaPage::new()?,
            depth,
            tail: 0,
            head: 0,
            phase: true,
            sq_doorbell: regs + 0x1000 + (2 * qid as u64) * stride,
            cq_doorbell: regs + 0x1000 + (2 * qid as u64 + 1) * stride,
            next_cid: 0,
        })
    }

    /// Submit `c` and wait for its completion: dword 0 of the completion, or the
    /// command's failure.
    fn run(&mut self, c: &Command) -> Result<u32, Error> {
        let cid = self.next_cid;
        self.next_cid = self.next_cid.wrapping_add(1);
        let entry = [
            u32::from(c.opcode) | (u32::from(cid) << 16),
            c.nsid,
            0,
            0,
            0,
            0,
            c.prp1 as u32,
            (c.prp1 >> 32) as u32,
            c.prp2 as u32,
            (c.prp2 >> 32) as u32,
            c.cdw10,
            c.cdw11,
            c.cdw12,
            0,
            0,
            0,
        ];
        // SAFETY: the submission page is the driver's; slot `tail` is free (one
        // command at a time).
        unsafe {
            let slot = (self.sq.virt + u64::from(self.tail) * 64) as *mut u32;
            for (i, dw) in entry.iter().enumerate() {
                core::ptr::write_volatile(slot.add(i), *dw);
            }
        }
        self.tail = (self.tail + 1) % self.depth;
        core::sync::atomic::fence(core::sync::atomic::Ordering::SeqCst);
        wr32(self.sq_doorbell, u32::from(self.tail));
        let cq_entry = self.cq.virt + u64::from(self.head) * 16;
        let phase = u32::from(self.phase);
        // SAFETY: the completion page is the driver's; the device writes the entry.
        let status_word = || unsafe { core::ptr::read_volatile((cq_entry + 12) as *const u32) };
        if !wait::until(COMMAND_MS, || (status_word() >> 16) & 1 == phase) {
            return Err(Error::TimedOut);
        }
        core::sync::atomic::fence(core::sync::atomic::Ordering::SeqCst);
        let dw3 = status_word();
        // SAFETY: as above.
        let dw0 = unsafe { core::ptr::read_volatile(cq_entry as *const u32) };
        self.head += 1;
        if self.head == self.depth {
            self.head = 0;
            self.phase = !self.phase;
        }
        wr32(self.cq_doorbell, u32::from(self.head));
        let status = (dw3 >> 17) & 0x7FFF;
        if dw3 & 0xFFFF != u32::from(cid) || status != 0 {
            return Err(Error::Fault);
        }
        Ok(dw0)
    }
}

impl Controller {
    /// Run an I/O command whose data is the first `pages` pages of the bounce buffer.
    fn io(&mut self, pages: usize, mut c: Command) -> Result<(), Error> {
        if self.failed {
            return Err(Error::Fault);
        }
        if pages > 0 {
            c.prp1 = self.data[0].phys;
            c.prp2 = match pages {
                1 => 0,
                2 => self.data[1].phys,
                _ => {
                    let list = self.prp_list.virt as *mut u64;
                    for (i, page) in self.data[1..pages].iter().enumerate() {
                        // SAFETY: the PRP list page is the driver's; `i` < DATA_PAGES.
                        unsafe { core::ptr::write_volatile(list.add(i), page.phys) };
                    }
                    self.prp_list.phys
                }
            };
        }
        let r = self.io.run(&c).map(|_| ());
        if matches!(r, Err(Error::TimedOut)) {
            self.failed = true;
            println!("[kernel] nvme: {}: a command timed out; the controller is not used again", self.pci);
        }
        r
    }
}

fn identify(admin: &mut Queue, buf: &DmaPage, cns: u32, nsid: u32) -> Result<(), Error> {
    admin
        .run(&Command { opcode: ADMIN_IDENTIFY, nsid, prp1: buf.phys, cdw10: cns, ..Default::default() })
        .map(|_| ())
}

/// Clear CC.EN and wait for the controller to say it stopped.
fn disable(regs: u64, ready_ms: u64) -> bool {
    let cc = rd32(regs + REG_CC);
    if cc & CC_EN != 0 && rd32(regs + REG_CSTS) & CSTS_RDY == 0 {
        // Still coming up: it must finish before it may be told to stop.
        wait::until(ready_ms, || rd32(regs + REG_CSTS) & (CSTS_RDY | CSTS_CFS) != 0);
    }
    wr32(regs + REG_CC, cc & !CC_EN);
    wait::until(ready_ms, || rd32(regs + REG_CSTS) & CSTS_RDY == 0)
}

fn bring_up(addr: pci::Address, regs: u64) -> Result<Controller, &'static str> {
    const NO_MEMORY: &str = "no memory for its queues";
    let cap = rd64(regs + REG_CAP);
    let stride = 4u64 << ((cap >> 32) & 0xF);
    // CAP.TO: how long enabling or disabling may take, in 500 ms units.
    let ready_ms = (((cap >> 24) & 0xFF) * 500).max(500);
    let max_entries = ((cap & 0xFFFF) + 1).min(u64::from(u16::MAX)) as u16;
    if (cap >> 48) & 0xF != 0 {
        return Err("it cannot use 4 KiB pages");
    }
    if (cap >> 37) & 1 == 0 {
        return Err("it has no NVM command set");
    }
    if 0x1000 + 4 * stride > MAP_BYTES {
        return Err("its doorbells are too far apart");
    }
    if !disable(regs, ready_ms) {
        return Err("it does not stop");
    }
    let admin_depth = ADMIN_DEPTH.min(max_entries);
    let io_depth = IO_DEPTH.min(max_entries);
    let mut admin = Queue::new(regs, 0, stride, admin_depth).map_err(|_| NO_MEMORY)?;
    let io = Queue::new(regs, 1, stride, io_depth).map_err(|_| NO_MEMORY)?;
    let buf = DmaPage::new().map_err(|_| NO_MEMORY)?;
    let prp_list = DmaPage::new().map_err(|_| NO_MEMORY)?;
    let mut data = Vec::new();
    data.try_reserve(DATA_PAGES).map_err(|_| NO_MEMORY)?;
    for _ in 0..DATA_PAGES {
        data.push(DmaPage::new().map_err(|_| NO_MEMORY)?);
    }

    wr32(regs + REG_INTMS, u32::MAX);
    wr32(regs + REG_AQA, (u32::from(admin_depth - 1) << 16) | u32::from(admin_depth - 1));
    wr64(regs + REG_ASQ, admin.sq.phys);
    wr64(regs + REG_ACQ, admin.cq.phys);
    wr32(regs + REG_CC, CC_IO_ENTRY_SIZES | CC_EN);
    if !wait::until(ready_ms, || rd32(regs + REG_CSTS) & (CSTS_RDY | CSTS_CFS) != 0)
        || rd32(regs + REG_CSTS) & CSTS_CFS != 0
    {
        return Err("it does not come up");
    }

    identify(&mut admin, &buf, 1, 0).map_err(|_| "IDENTIFY CONTROLLER failed")?;
    // SAFETY: the page holds the 4 KiB identify-controller answer.
    let ctrl = unsafe { core::slice::from_raw_parts(buf.virt as *const u8, 4096) };
    let count = u32::from_le_bytes([ctrl[516], ctrl[517], ctrl[518], ctrl[519]]);
    let model: String =
        ctrl[24..64].iter().filter(|b| (0x20..0x7F).contains(*b)).map(|&b| b as char).collect();
    let model = String::from(model.trim());

    let size = u32::from(io_depth - 1) << 16;
    // Completion queue 1, physically contiguous, no interrupt.
    admin
        .run(&Command {
            opcode: ADMIN_CREATE_CQ,
            prp1: io.cq.phys,
            cdw10: size | 1,
            cdw11: 1,
            ..Default::default()
        })
        .map_err(|_| "it refuses an I/O completion queue")?;
    // Submission queue 1 feeding completion queue 1, physically contiguous.
    admin
        .run(&Command {
            opcode: ADMIN_CREATE_SQ,
            prp1: io.sq.phys,
            cdw10: size | 1,
            cdw11: (1 << 16) | 1,
            ..Default::default()
        })
        .map_err(|_| "it refuses an I/O submission queue")?;

    let mut namespaces = Vec::new();
    for id in 1..=count.min(MAX_NAMESPACES) {
        if identify(&mut admin, &buf, 0, id).is_err() {
            continue;
        }
        // SAFETY: the page holds the identify-namespace answer.
        let ns = unsafe { core::slice::from_raw_parts(buf.virt as *const u8, 4096) };
        let sectors = u64::from_le_bytes([ns[0], ns[1], ns[2], ns[3], ns[4], ns[5], ns[6], ns[7]]);
        if sectors == 0 {
            continue; // not an active namespace
        }
        // FLBAS: bits 3:0, and 6:5 above them, pick the LBA format in use.
        let format = usize::from(ns[26] & 0xF) | (usize::from((ns[26] >> 5) & 3) << 4);
        let lbaf = 128 + format * 4;
        let metadata = u16::from_le_bytes([ns[lbaf], ns[lbaf + 1]]);
        let lbads = ns[lbaf + 2];
        if metadata != 0 {
            println!("[kernel] nvme: {addr} namespace {id}: blocks carry metadata; not used");
        } else if lbads != 9 {
            println!(
                "[kernel] nvme: {addr} namespace {id}: {}-byte blocks, only 512 is supported; not used",
                1u64.checked_shl(u32::from(lbads)).unwrap_or(0)
            );
        } else {
            namespaces.push(Namespace { id, sectors });
        }
    }
    let version = rd32(regs + REG_VS);
    println!(
        "[kernel] nvme: {addr}: \"{model}\", NVMe {}.{}, {} usable namespace(s) among IDs 1-{}",
        version >> 16,
        (version >> 8) & 0xFF,
        namespaces.len(),
        count.min(MAX_NAMESPACES)
    );
    for ns in &namespaces {
        println!(
            "[kernel] nvme: {addr} namespace {}: {} MiB",
            ns.id,
            ns.sectors * SECTOR as u64 / (1024 * 1024)
        );
    }
    Ok(Controller { pci: addr, admin, io, data, prp_list, namespaces, failed: false })
}

/// Find every NVMe controller and bring it up.
pub fn init() {
    for addr in pci::find_class(0x01, 0x08, 0x02) {
        let Some(bar) = addr.bar_address(0) else {
            println!("[kernel] nvme: {addr} has no register BAR; not used");
            continue;
        };
        addr.enable_memory_and_bus_master();
        addr.disable_intx();
        let Ok(regs) = crate::mm::mmio::map(bar, MAP_BYTES) else {
            println!("[kernel] nvme: {addr}: no room to map the controller");
            continue;
        };
        match bring_up(addr, regs) {
            Ok(c) => CONTROLLERS.lock().push(c),
            Err(e) => {
                // Whatever it was doing, it does no more of it.
                disable(regs, 500);
                println!("[kernel] nvme: {addr}: {e}; not used");
            }
        }
    }
}

/// `(controller, namespace)` of every namespace this driver can use.
pub fn namespaces() -> Vec<(usize, u32)> {
    CONTROLLERS
        .lock()
        .iter()
        .enumerate()
        .flat_map(|(c, ctrl)| ctrl.namespaces.iter().map(move |ns| (c, ns.id)))
        .collect()
}

/// Where namespace `ns` of controller `c` is, for the log.
pub fn name(c: usize, ns: u32) -> String {
    match CONTROLLERS.lock().get(c) {
        Some(ctrl) => alloc::format!("NVMe {} namespace {ns}", ctrl.pci),
        None => String::from("NVMe (gone)"),
    }
}

pub fn capacity_sectors(c: usize, ns: u32) -> u64 {
    CONTROLLERS
        .lock()
        .get(c)
        .and_then(|ctrl| ctrl.namespaces.iter().find(|x| x.id == ns).map(|x| x.sectors))
        .unwrap_or(0)
}

fn with_ns<R>(
    c: usize,
    ns: u32,
    lba: u64,
    len: usize,
    f: impl FnOnce(&mut Controller) -> Result<R, Error>,
) -> Result<R, Error> {
    let mut g = CONTROLLERS.lock();
    let ctrl = g.get_mut(c).ok_or(Error::NotFound)?;
    let sectors = ctrl.namespaces.iter().find(|x| x.id == ns).ok_or(Error::NotFound)?.sectors;
    if !len.is_multiple_of(SECTOR) || lba.checked_add((len / SECTOR) as u64).is_none_or(|e| e > sectors) {
        return Err(Error::Invalid);
    }
    f(ctrl)
}

fn rw(ns: u32, opcode: u8, lba: u64, sectors: usize) -> Command {
    Command {
        opcode,
        nsid: ns,
        cdw10: lba as u32,
        cdw11: (lba >> 32) as u32,
        cdw12: (sectors - 1) as u32,
        ..Default::default()
    }
}

pub fn read_sectors(c: usize, ns: u32, lba: u64, buf: &mut [u8]) -> Result<(), Error> {
    with_ns(c, ns, lba, buf.len(), |ctrl| {
        for (i, chunk) in buf.chunks_mut(SECTORS_PER_COMMAND * SECTOR).enumerate() {
            let at = lba + (i * SECTORS_PER_COMMAND) as u64;
            ctrl.io(chunk.len().div_ceil(4096), rw(ns, IO_READ, at, chunk.len() / SECTOR))?;
            for (j, piece) in chunk.chunks_mut(4096).enumerate() {
                // SAFETY: the data page holds what the controller just wrote there.
                let src = unsafe { core::slice::from_raw_parts(ctrl.data[j].virt as *const u8, piece.len()) };
                piece.copy_from_slice(src);
            }
        }
        Ok(())
    })
}

pub fn write_sectors(c: usize, ns: u32, lba: u64, buf: &[u8]) -> Result<(), Error> {
    with_ns(c, ns, lba, buf.len(), |ctrl| {
        for (i, chunk) in buf.chunks(SECTORS_PER_COMMAND * SECTOR).enumerate() {
            for (j, piece) in chunk.chunks(4096).enumerate() {
                // SAFETY: the data pages are the driver's own, idle between commands.
                let dst =
                    unsafe { core::slice::from_raw_parts_mut(ctrl.data[j].virt as *mut u8, piece.len()) };
                dst.copy_from_slice(piece);
            }
            let at = lba + (i * SECTORS_PER_COMMAND) as u64;
            ctrl.io(chunk.len().div_ceil(4096), rw(ns, IO_WRITE, at, chunk.len() / SECTOR))?;
        }
        Ok(())
    })
}

pub fn flush(c: usize, ns: u32) -> Result<(), Error> {
    with_ns(c, ns, 0, 0, |ctrl| ctrl.io(0, Command { opcode: IO_FLUSH, nsid: ns, ..Default::default() }))
}
