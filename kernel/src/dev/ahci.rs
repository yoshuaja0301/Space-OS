//! SATA disks behind AHCI controllers (polled).
//!
//! Most PCs keep their disks on an AHCI controller, and QEMU's q35 machine has one
//! built in. For every controller the driver asks the firmware to let go of it if
//! the firmware says it holds it, switches it to AHCI mode with its interrupt off,
//! and stops every port: the command lists and FIS areas the firmware programmed
//! are in memory the kernel has since taken back, and the device must not write
//! there again. A port with a SATA disk attached then gets its own command list,
//! received-FIS area and command table in one page of DMA memory, and the disk is
//! asked who it is (IDENTIFY DEVICE).
//!
//! Transfers are READ/WRITE DMA EXT with LBA48 through a bounce buffer of eight
//! pages, one PRDT entry per page; FLUSH CACHE EXT makes writes durable. Nothing
//! interrupts: a command is issued by setting the port's command-issue bit and is
//! done when the bit clears. A task-file error restarts the port's command engine
//! and fails that command; a timeout takes the port out of use for good, since a
//! late completion would be taken for the next command's.
//!
//! Which disk holds the data volume is decided by `block`, not here: the disk the
//! firmware booted from is found here too, and must stay untouched there.

use alloc::string::String;
use alloc::vec::Vec;

use spaceabi::error::Error;

use super::dma::DmaPage;
use super::{pci, wait};
use crate::sync::SpinLock;

const SECTOR: usize = 512;
/// Data pages per command: 32 KiB, 64 sectors.
const DATA_PAGES: usize = 8;
const SECTORS_PER_COMMAND: usize = DATA_PAGES * 4096 / SECTOR;

/// A port's command engine stops within 500 ms (AHCI 1.3.1, 10.1.2).
const STOP_MS: u64 = 1_000;
/// The firmware gives the controller up within 25 ms, or 2 s when busy (10.6.3).
const HANDOFF_MS: u64 = 2_100;
/// A disk leaves BSY/DRQ before its port is started.
const READY_MS: u64 = 5_000;
/// One command, a cache flush of a slow disk included.
const COMMAND_MS: u64 = 30_000;

// HBA registers.
const CAP: u64 = 0x00;
const GHC: u64 = 0x04;
const PI: u64 = 0x0C;
const VS: u64 = 0x10;
const CAP2: u64 = 0x24;
const BOHC: u64 = 0x28;
/// The controller can reach memory above 4 GiB.
const CAP_S64A: u32 = 1 << 31;
const GHC_AE: u32 = 1 << 31;
const GHC_IE: u32 = 1 << 1;
const CAP2_BOH: u32 = 1 << 0;
const BOHC_OOS: u32 = 1 << 1;
const BOHC_BOS: u32 = 1 << 0;

// Port registers, relative to the port's 0x80-byte block.
const P_CLB: u64 = 0x00;
const P_CLBU: u64 = 0x04;
const P_FB: u64 = 0x08;
const P_FBU: u64 = 0x0C;
const P_IS: u64 = 0x10;
const P_IE: u64 = 0x14;
const P_CMD: u64 = 0x18;
const P_TFD: u64 = 0x20;
const P_SIG: u64 = 0x24;
const P_SSTS: u64 = 0x28;
const P_SERR: u64 = 0x30;
const P_CI: u64 = 0x38;

const CMD_ST: u32 = 1 << 0;
const CMD_FRE: u32 = 1 << 4;
const CMD_FR: u32 = 1 << 14;
const CMD_CR: u32 = 1 << 15;
const TFD_ERR: u32 = 1 << 0;
const TFD_DRQ: u32 = 1 << 3;
const TFD_BSY: u32 = 1 << 7;
const IS_TFES: u32 = 1 << 30;
/// Device detected and communication established.
const SSTS_DET_PRESENT: u32 = 3;
/// A SATA disk (as opposed to ATAPI, a port multiplier or an enclosure).
const SIG_ATA: u32 = 0x0000_0101;

const ATA_IDENTIFY: u8 = 0xEC;
const ATA_READ_DMA_EXT: u8 = 0x25;
const ATA_WRITE_DMA_EXT: u8 = 0x35;
const ATA_FLUSH_CACHE_EXT: u8 = 0xEA;

/// Layout of the port's control page: command list (1 KiB), received FISes
/// (256 bytes), then the one command table this driver uses.
const CL_OFF: u64 = 0x000;
const FB_OFF: u64 = 0x400;
const CT_OFF: u64 = 0x500;

struct Port {
    controller: pci::Address,
    number: u8,
    regs: u64,
    control: DmaPage,
    data: Vec<DmaPage>,
    sectors: u64,
    model: String,
    /// Set after a timeout or a port that would not restart: it is not used again.
    failed: bool,
}

// SAFETY: plain integers and addresses of memory owned by the driver, used under
// the `PORTS` lock only.
unsafe impl Send for Port {}

/// Every disk found, on every controller. The index is the disk's handle.
static PORTS: SpinLock<Vec<Port>> = SpinLock::new(Vec::new());

fn rd(addr: u64) -> u32 {
    // SAFETY: an MMIO register inside a mapped ABAR.
    unsafe { core::ptr::read_volatile(addr as *const u32) }
}

fn wr(addr: u64, v: u32) {
    // SAFETY: as above.
    unsafe { core::ptr::write_volatile(addr as *mut u32, v) }
}

/// Stop a port's command engine and FIS reception, as the port must be before
/// its memory is changed. False if it would not stop.
fn stop(regs: u64) -> bool {
    let cmd = rd(regs + P_CMD);
    if cmd & (CMD_ST | CMD_CR) != 0 {
        wr(regs + P_CMD, cmd & !CMD_ST);
        if !wait::until(STOP_MS, || rd(regs + P_CMD) & CMD_CR == 0) {
            return false;
        }
    }
    let cmd = rd(regs + P_CMD);
    if cmd & (CMD_FRE | CMD_FR) != 0 {
        wr(regs + P_CMD, cmd & !CMD_FRE);
        if !wait::until(STOP_MS, || rd(regs + P_CMD) & CMD_FR == 0) {
            return false;
        }
    }
    true
}

impl Port {
    fn reg(&self, off: u64) -> u64 {
        self.regs + off
    }

    /// Point the stopped port at its memory and start it. False if the disk
    /// stays busy, which only a reset of the link would clear.
    fn start(&self) -> bool {
        let base = self.control.phys;
        wr(self.reg(P_CLB), (base + CL_OFF) as u32);
        wr(self.reg(P_CLBU), ((base + CL_OFF) >> 32) as u32);
        wr(self.reg(P_FB), (base + FB_OFF) as u32);
        wr(self.reg(P_FBU), ((base + FB_OFF) >> 32) as u32);
        wr(self.reg(P_SERR), u32::MAX);
        wr(self.reg(P_IS), u32::MAX);
        wr(self.reg(P_IE), 0);
        wr(self.reg(P_CMD), rd(self.reg(P_CMD)) | CMD_FRE);
        if !wait::until(READY_MS, || rd(self.reg(P_TFD)) & (TFD_BSY | TFD_DRQ) == 0) {
            return false;
        }
        wr(self.reg(P_CMD), rd(self.reg(P_CMD)) | CMD_ST);
        true
    }

    /// After a task-file error the engine has stopped taking commands; restarting
    /// it clears the failed one (AHCI 1.3.1, 6.2.2.1).
    fn recover(&mut self) {
        if !stop(self.regs) || !self.start() {
            self.failed = true;
            println!(
                "[kernel] ahci: {} port {} did not restart after an error; not used again",
                self.controller, self.number
            );
        }
    }

    /// Issue one ATA command in slot 0 and wait for it. `bytes` of the bounce
    /// buffer take part (0 for commands without data).
    fn command(&mut self, ata: u8, lba: u64, count: u16, bytes: usize, write: bool) -> Result<(), Error> {
        if self.failed {
            return Err(Error::Fault);
        }
        if !wait::until(COMMAND_MS, || rd(self.reg(P_TFD)) & (TFD_BSY | TFD_DRQ) == 0) {
            self.failed = true;
            println!(
                "[kernel] ahci: {} port {}: the disk stays busy; not used again",
                self.controller, self.number
            );
            return Err(Error::TimedOut);
        }
        let prdt_len = bytes.div_ceil(4096);
        let base = self.control.virt;
        let ct_phys = self.control.phys + CT_OFF;
        // SAFETY: the control page is ours and the port is idle (checked above).
        unsafe {
            // Command header 0: FIS length 5 dwords, write flag, PRDT length.
            let header = (base + CL_OFF) as *mut u32;
            let dw0 = 5 | if write { 1 << 6 } else { 0 } | ((prdt_len as u32) << 16);
            core::ptr::write_volatile(header, dw0);
            core::ptr::write_volatile(header.add(1), 0);
            core::ptr::write_volatile(header.add(2), ct_phys as u32);
            core::ptr::write_volatile(header.add(3), (ct_phys >> 32) as u32);
            // Command table: the host-to-device register FIS, then the PRDT.
            let ct = (base + CT_OFF) as *mut u8;
            core::ptr::write_bytes(ct, 0, 0x80 + DATA_PAGES * 16);
            let fis = [
                0x27u8,
                0x80, // a command, not a control update
                ata,
                0,
                lba as u8,
                (lba >> 8) as u8,
                (lba >> 16) as u8,
                1 << 6, // LBA addressing
                (lba >> 24) as u8,
                (lba >> 32) as u8,
                (lba >> 40) as u8,
                0,
                count as u8,
                (count >> 8) as u8,
                0,
                0,
            ];
            core::ptr::copy_nonoverlapping(fis.as_ptr(), ct, fis.len());
            let mut left = bytes;
            for (i, page) in self.data.iter().take(prdt_len).enumerate() {
                let entry = ct.add(0x80 + i * 16) as *mut u32;
                let len = left.min(4096);
                core::ptr::write_volatile(entry, page.phys as u32);
                core::ptr::write_volatile(entry.add(1), (page.phys >> 32) as u32);
                core::ptr::write_volatile(entry.add(2), 0);
                core::ptr::write_volatile(entry.add(3), (len - 1) as u32);
                left -= len;
            }
        }
        wr(self.reg(P_IS), u32::MAX);
        crate::arch::dma_mb();
        wr(self.reg(P_CI), 1);
        let mut error = false;
        let done = wait::until(COMMAND_MS, || {
            if rd(self.reg(P_IS)) & IS_TFES != 0 {
                error = true;
                return true;
            }
            rd(self.reg(P_CI)) & 1 == 0
        });
        if !done {
            self.failed = true;
            // Stop the engine if it still listens: the command may complete later,
            // into pages nobody reads again, but no new one starts.
            let _ = stop(self.regs);
            println!(
                "[kernel] ahci: {} port {} timed out on command {ata:#04x}; not used again",
                self.controller, self.number
            );
            return Err(Error::TimedOut);
        }
        if error || rd(self.reg(P_TFD)) & TFD_ERR != 0 {
            self.recover();
            return Err(Error::Fault);
        }
        Ok(())
    }

    fn identify(&mut self) -> Result<(), &'static str> {
        self.command(ATA_IDENTIFY, 0, 0, SECTOR, false).map_err(|_| "IDENTIFY DEVICE failed")?;
        // SAFETY: the first data page holds the 512-byte IDENTIFY answer.
        let id = unsafe { core::slice::from_raw_parts(self.data[0].virt as *const u16, 256) };
        if id[83] & (1 << 10) == 0 {
            return Err("no 48-bit addressing");
        }
        // Word 106 says whether logical sectors are longer than 512 bytes.
        if id[106] & 0xC000 == 0x4000 && id[106] & (1 << 12) != 0 {
            let words = u32::from(id[117]) | (u32::from(id[118]) << 16);
            if words != 256 {
                return Err("sectors are not 512 bytes");
            }
        }
        self.sectors = u64::from(id[100])
            | (u64::from(id[101]) << 16)
            | (u64::from(id[102]) << 32)
            | (u64::from(id[103]) << 48);
        if self.sectors == 0 {
            return Err("no capacity reported");
        }
        let mut model = String::new();
        for w in &id[27..47] {
            for b in [(w >> 8) as u8, *w as u8] {
                if (0x20..0x7F).contains(&b) {
                    model.push(b as char);
                }
            }
        }
        self.model = String::from(model.trim());
        Ok(())
    }
}

/// Find every AHCI controller and every SATA disk on them.
pub fn init() {
    for addr in pci::find_class(0x01, 0x06, 0x01) {
        init_controller(addr);
    }
}

fn init_controller(addr: pci::Address) {
    let Some(abar) = addr.bar_address(5) else {
        println!("[kernel] ahci: {addr} has no register BAR; not used");
        return;
    };
    addr.enable_memory_and_bus_master();
    addr.disable_intx();
    let Ok(hba) = crate::mm::mmio::map(abar, 0x1100) else {
        println!("[kernel] ahci: {addr}: no room to map the controller");
        return;
    };
    if rd(hba + CAP2) & CAP2_BOH != 0 {
        wr(hba + BOHC, rd(hba + BOHC) | BOHC_OOS);
        if !wait::until(HANDOFF_MS, || rd(hba + BOHC) & BOHC_BOS == 0) {
            println!("[kernel] ahci: {addr}: the firmware does not let go of the controller; not used");
            return;
        }
    }
    wr(hba + GHC, (rd(hba + GHC) | GHC_AE) & !GHC_IE);
    let wide = rd(hba + CAP) & CAP_S64A != 0;
    let implemented = rd(hba + PI);
    let version = rd(hba + VS);
    let mut found = 0;
    for n in 0..32u8 {
        if implemented & (1 << n) == 0 {
            continue;
        }
        let regs = hba + 0x100 + n as u64 * 0x80;
        if !stop(regs) {
            println!("[kernel] ahci: {addr} port {n} does not stop; not used");
            continue;
        }
        if rd(regs + P_SSTS) & 0xF != SSTS_DET_PRESENT || rd(regs + P_SIG) != SIG_ATA {
            continue; // nothing attached, or not a disk
        }
        match attach(addr, n, regs, wide) {
            Ok(port) => {
                println!(
                    "[kernel] ahci: {addr} port {n}: \"{}\", {} MiB, AHCI {}.{}",
                    port.model,
                    port.sectors * SECTOR as u64 / (1024 * 1024),
                    version >> 16,
                    (version >> 8) & 0xFF
                );
                PORTS.lock().push(port);
                found += 1;
            }
            Err(e) => println!("[kernel] ahci: {addr} port {n}: {e}; not used"),
        }
    }
    if found == 0 {
        println!("[kernel] ahci: {addr}: no SATA disk");
    }
}

fn attach(controller: pci::Address, number: u8, regs: u64, wide: bool) -> Result<Port, &'static str> {
    const NO_MEMORY: &str = "no memory for its buffers";
    let control = DmaPage::new().map_err(|_| NO_MEMORY)?;
    let mut data = Vec::new();
    data.try_reserve(DATA_PAGES).map_err(|_| NO_MEMORY)?;
    for _ in 0..DATA_PAGES {
        data.push(DmaPage::new().map_err(|_| NO_MEMORY)?);
    }
    let reachable = |p: &DmaPage| wide || p.phys + 4096 <= 1 << 32;
    if !reachable(&control) || !data.iter().all(reachable) {
        return Err("the controller reaches only the first 4 GiB, and its buffers are above");
    }
    let mut port =
        Port { controller, number, regs, control, data, sectors: 0, model: String::new(), failed: false };
    if !port.start() {
        return Err("the disk stays busy");
    }
    port.identify()?;
    Ok(port)
}

/// Handles of the disks this driver can use.
pub fn disks() -> Vec<usize> {
    (0..PORTS.lock().len()).collect()
}

/// Where disk `disk` is, for the log.
pub fn name(disk: usize) -> String {
    match PORTS.lock().get(disk) {
        Some(p) => alloc::format!("AHCI {} port {}", p.controller, p.number),
        None => String::from("AHCI (gone)"),
    }
}

fn with_port<R>(disk: usize, f: impl FnOnce(&mut Port) -> Result<R, Error>) -> Result<R, Error> {
    let mut ports = PORTS.lock();
    let port = ports.get_mut(disk).ok_or(Error::NotFound)?;
    f(port)
}

pub fn capacity_sectors(disk: usize) -> u64 {
    with_port(disk, |p| Ok(p.sectors)).unwrap_or(0)
}

fn check(p: &Port, lba: u64, len: usize) -> Result<(), Error> {
    if !len.is_multiple_of(SECTOR) || lba.checked_add((len / SECTOR) as u64).is_none_or(|end| end > p.sectors)
    {
        return Err(Error::Invalid);
    }
    Ok(())
}

pub fn read_sectors(disk: usize, lba: u64, buf: &mut [u8]) -> Result<(), Error> {
    with_port(disk, |p| {
        check(p, lba, buf.len())?;
        for (i, chunk) in buf.chunks_mut(SECTORS_PER_COMMAND * SECTOR).enumerate() {
            let at = lba + (i * SECTORS_PER_COMMAND) as u64;
            p.command(ATA_READ_DMA_EXT, at, (chunk.len() / SECTOR) as u16, chunk.len(), false)?;
            for (j, piece) in chunk.chunks_mut(4096).enumerate() {
                // SAFETY: the data page holds what the disk just wrote into it.
                let src = unsafe { core::slice::from_raw_parts(p.data[j].virt as *const u8, piece.len()) };
                piece.copy_from_slice(src);
            }
        }
        Ok(())
    })
}

pub fn write_sectors(disk: usize, lba: u64, buf: &[u8]) -> Result<(), Error> {
    with_port(disk, |p| {
        check(p, lba, buf.len())?;
        for (i, chunk) in buf.chunks(SECTORS_PER_COMMAND * SECTOR).enumerate() {
            for (j, piece) in chunk.chunks(4096).enumerate() {
                // SAFETY: the data pages are the driver's own, idle between commands.
                let dst = unsafe { core::slice::from_raw_parts_mut(p.data[j].virt as *mut u8, piece.len()) };
                dst.copy_from_slice(piece);
            }
            let at = lba + (i * SECTORS_PER_COMMAND) as u64;
            p.command(ATA_WRITE_DMA_EXT, at, (chunk.len() / SECTOR) as u16, chunk.len(), true)?;
        }
        Ok(())
    })
}

pub fn flush(disk: usize) -> Result<(), Error> {
    with_port(disk, |p| p.command(ATA_FLUSH_CACHE_EXT, 0, 0, 0, false))
}
