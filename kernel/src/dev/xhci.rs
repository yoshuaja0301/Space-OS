//! USB host controllers of the xHCI kind (polled), and the keyboards on their ports
//! (ADR-0030).
//!
//! Every PC built since about 2012 has its USB ports on an xHCI controller, and so do
//! Arm servers and QEMU's `qemu-xhci`. A machine whose only keyboard is a USB one --
//! most of them, once the firmware has handed over and stopped emulating a PS/2
//! keyboard -- has no keyboard at all without this driver.
//!
//! Each controller is taken from the firmware (the legacy-support handshake), reset,
//! and given a device context array, a command ring and one event ring. Nothing
//! interrupts: the boot CPU's tick looks at the event ring ([`poll_tick`]), as it
//! looks at the network card. Devices on the root ports when the kernel starts are
//! reset, addressed and asked for their descriptors; a keyboard that speaks the boot
//! protocol is configured, and its interrupt endpoint is kept supplied with transfers
//! whose reports go to [`super::hid`]. Every wait has a limit, and a controller or
//! device that does not answer is reported and left alone, never waited on for ever.
//!
//! Not yet: hubs (a device behind one is not seen), devices plugged in after boot
//! (reported, not enumerated), other device classes (reported and left
//! unconfigured), and key repeat (a USB keyboard does not repeat; the host would).

use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, Ordering};

use super::dma::DmaPage;
use super::hid;
use super::{pci, wait};
use crate::sync::SpinLock;

/// Controllers and devices this driver takes on.
const MAX_CONTROLLERS: usize = 4;
const MAX_SLOTS: u32 = 32;
/// Transfers kept queued on a keyboard's interrupt endpoint.
const REPORTS_QUEUED: usize = 8;
/// One command or control transfer, and the controller's own start and stop.
const COMMAND_MS: u64 = 500;
const HALT_MS: u64 = 100;
const RESET_MS: u64 = 1_000;
/// The firmware has this long to give the controller up.
const HANDOFF_MS: u64 = 1_000;
/// USB 2.0 §7.1.7.3 and §9.2.6: a port takes up to 50 ms to reset, and a device
/// may ignore requests for 10 ms after.
const PORT_RESET_MS: u64 = 100;
const RESET_RECOVERY_MS: u64 = 10;
/// Events taken per tick, so a flood cannot hold the boot CPU.
const EVENTS_PER_TICK: usize = 64;

// Capability registers.
const CAP_LENGTH_VERSION: u64 = 0x00;
const CAP_HCSPARAMS1: u64 = 0x04;
const CAP_HCSPARAMS2: u64 = 0x08;
const CAP_HCCPARAMS1: u64 = 0x10;
const CAP_DBOFF: u64 = 0x14;
const CAP_RTSOFF: u64 = 0x18;
// Operational registers.
const OP_USBCMD: u64 = 0x00;
const OP_USBSTS: u64 = 0x04;
const OP_PAGESIZE: u64 = 0x08;
const OP_CRCR: u64 = 0x18;
const OP_DCBAAP: u64 = 0x30;
const OP_CONFIG: u64 = 0x38;
const OP_PORTS: u64 = 0x400;
const CMD_RUN: u32 = 1 << 0;
const CMD_RESET: u32 = 1 << 1;
const STS_HALTED: u32 = 1 << 0;
const STS_NOT_READY: u32 = 1 << 11;
// Port status and control.
const PORT_CONNECTED: u32 = 1 << 0;
const PORT_ENABLED: u32 = 1 << 1;
const PORT_RESET: u32 = 1 << 4;
const PORT_POWER: u32 = 1 << 9;
/// The bits a write keeps as they are: power, the indicator, the wake enables.
/// Enabled and the change bits are cleared by writing 1, so they are written as 0
/// unless they are meant to be cleared.
const PORT_KEEP: u32 = PORT_POWER | (3 << 14) | (7 << 25);
const PORT_CHANGES: u32 = 0x7F << 17;
const PORT_RESET_CHANGE: u32 = 1 << 21;
// Interrupter 0 of the runtime registers.
const IR0: u64 = 0x20;
const IR_IMAN: u64 = 0x00;
const IR_ERSTSZ: u64 = 0x08;
const IR_ERSTBA: u64 = 0x10;
const IR_ERDP: u64 = 0x18;
/// ERDP: the event handler is done (written as 1 to clear).
const ERDP_BUSY: u64 = 1 << 3;
// Extended capabilities.
const XCAP_LEGACY: u32 = 1;
const LEGACY_BIOS_OWNED: u32 = 1 << 16;
const LEGACY_OS_OWNED: u32 = 1 << 24;
/// USBLEGCTLSTS: the SMI enables (cleared) and the RW1C SMI status bits.
const LEGACY_SMI_ENABLES: u32 = (1 << 0) | (1 << 4) | (7 << 13);
const LEGACY_SMI_STATUS: u32 = 7 << 29;

// TRBs: types, and the bits of the control dword.
const TRB_NORMAL: u32 = 1;
const TRB_SETUP: u32 = 2;
const TRB_DATA: u32 = 3;
const TRB_STATUS: u32 = 4;
const TRB_LINK: u32 = 6;
const TRB_ENABLE_SLOT: u32 = 9;
const TRB_ADDRESS_DEVICE: u32 = 11;
const TRB_CONFIGURE_ENDPOINT: u32 = 12;
const TRB_EVALUATE_CONTEXT: u32 = 13;
const TRB_TRANSFER_EVENT: u32 = 32;
const TRB_COMMAND_COMPLETION: u32 = 33;
const TRB_PORT_STATUS_CHANGE: u32 = 34;
const TRB_CYCLE: u32 = 1 << 0;
const TRB_TOGGLE_CYCLE: u32 = 1 << 1;
const TRB_SHORT_OK: u32 = 1 << 2;
const TRB_IOC: u32 = 1 << 5;
const TRB_IMMEDIATE: u32 = 1 << 6;
const TRB_DIR_IN: u32 = 1 << 16;
const COMPLETION_SUCCESS: u32 = 1;
const COMPLETION_SHORT_PACKET: u32 = 13;

/// TRBs in one ring page; the last is the link back to the first.
const RING_TRBS: usize = 256;

// Endpoint context types.
const EP_CONTROL: u32 = 4;
const EP_INTERRUPT_IN: u32 = 7;

// Port speeds (PORTSC.PS, slot context).
const SPEED_FULL: u32 = 1;
const SPEED_LOW: u32 = 2;
const SPEED_HIGH: u32 = 3;
const SPEED_SUPER: u32 = 4;

// Standard requests and descriptors.
const REQ_GET_DESCRIPTOR: u8 = 6;
const REQ_SET_CONFIGURATION: u8 = 9;
const HID_SET_PROTOCOL: u8 = 0x0B;
const DESC_DEVICE: u16 = 1;
const DESC_CONFIGURATION: u16 = 2;
const DESC_INTERFACE: u8 = 4;
const DESC_ENDPOINT: u8 = 5;
const CLASS_HID: u8 = 3;
const SUBCLASS_BOOT: u8 = 1;
const PROTOCOL_KEYBOARD: u8 = 1;

/// Set once a controller is running, so the tick does not look before then.
static ACTIVE: AtomicBool = AtomicBool::new(false);
static CONTROLLERS: SpinLock<Vec<Controller>> = SpinLock::new(Vec::new());

fn rd32(addr: u64) -> u32 {
    // SAFETY: an MMIO register inside a mapped BAR.
    unsafe { core::ptr::read_volatile(addr as *const u32) }
}

fn wr32(addr: u64, v: u32) {
    // SAFETY: as above.
    unsafe { core::ptr::write_volatile(addr as *mut u32, v) }
}

/// 64-bit registers as two dwords, the low one first -- what every xHCI accepts.
fn wr64(addr: u64, v: u64) {
    wr32(addr, v as u32);
    wr32(addr + 4, (v >> 32) as u32);
}

fn mem_rd32(addr: u64) -> u32 {
    // SAFETY: memory in one of the driver's DMA pages.
    unsafe { core::ptr::read_volatile(addr as *const u32) }
}

fn mem_wr32(addr: u64, v: u32) {
    // SAFETY: as above.
    unsafe { core::ptr::write_volatile(addr as *mut u32, v) }
}

fn speed_name(speed: u32) -> &'static str {
    match speed {
        SPEED_FULL => "full speed",
        SPEED_LOW => "low speed",
        SPEED_HIGH => "high speed",
        SPEED_SUPER => "SuperSpeed",
        5 => "SuperSpeedPlus",
        _ => "unknown speed",
    }
}

/// A ring the driver fills and the controller consumes: a command ring or a
/// transfer ring. One page; the last TRB links back to the first and toggles the
/// cycle bit, which is how the controller tells new TRBs from old ones.
struct Ring {
    page: DmaPage,
    enqueue: usize,
    cycle: bool,
}

impl Ring {
    fn new(wide: bool) -> Option<Ring> {
        let page = page_for(wide)?;
        let link = page.virt + ((RING_TRBS - 1) * 16) as u64;
        mem_wr32(link, page.phys as u32);
        mem_wr32(link + 4, (page.phys >> 32) as u32);
        mem_wr32(link + 8, 0);
        mem_wr32(link + 12, (TRB_LINK << 10) | TRB_TOGGLE_CYCLE);
        Some(Ring { page, enqueue: 0, cycle: true })
    }

    /// Hand the controller one TRB; returns its address. The control dword, which
    /// carries the cycle bit, is written last.
    fn push(&mut self, trb: [u32; 4]) -> u64 {
        let phys = self.page.phys + (self.enqueue * 16) as u64;
        let virt = self.page.virt + (self.enqueue * 16) as u64;
        mem_wr32(virt, trb[0]);
        mem_wr32(virt + 4, trb[1]);
        mem_wr32(virt + 8, trb[2]);
        crate::arch::dma_wmb();
        mem_wr32(virt + 12, (trb[3] & !TRB_CYCLE) | u32::from(self.cycle));
        self.enqueue += 1;
        if self.enqueue == RING_TRBS - 1 {
            let link = self.page.virt + ((RING_TRBS - 1) * 16) as u64;
            crate::arch::dma_wmb();
            mem_wr32(link + 12, (TRB_LINK << 10) | TRB_TOGGLE_CYCLE | u32::from(self.cycle));
            self.enqueue = 0;
            self.cycle = !self.cycle;
        }
        phys
    }

    /// The TRB at physical address `phys`, if it is one of this ring's.
    fn trb_at(&self, phys: u64) -> Option<[u32; 4]> {
        let off = phys.checked_sub(self.page.phys)?;
        if off >= (RING_TRBS * 16) as u64 || off % 16 != 0 {
            return None;
        }
        let v = self.page.virt + off;
        Some([mem_rd32(v), mem_rd32(v + 4), mem_rd32(v + 8), mem_rd32(v + 12)])
    }
}

/// The ring the controller fills: one segment, one page of event TRBs.
struct EventRing {
    page: DmaPage,
    /// The segment table: one entry, the page above.
    #[allow(dead_code)]
    table: DmaPage,
    dequeue: usize,
    cycle: bool,
}

impl EventRing {
    fn next(&mut self) -> Option<[u32; 4]> {
        let v = self.page.virt + (self.dequeue * 16) as u64;
        if (mem_rd32(v + 12) & TRB_CYCLE != 0) != self.cycle {
            return None;
        }
        crate::arch::dma_rmb();
        let ev = [mem_rd32(v), mem_rd32(v + 4), mem_rd32(v + 8), mem_rd32(v + 12)];
        self.dequeue += 1;
        if self.dequeue == RING_TRBS {
            self.dequeue = 0;
            self.cycle = !self.cycle;
        }
        Some(ev)
    }

    fn dequeue_pointer(&self) -> u64 {
        self.page.phys + (self.dequeue * 16) as u64
    }
}

/// A keyboard's interrupt endpoint and what it last reported.
struct KeyboardEndpoint {
    dci: u32,
    ring: Ring,
    /// Report buffers, 64 bytes apart.
    reports: DmaPage,
    state: hid::Keyboard,
    working: bool,
}

struct Device {
    slot: u32,
    port: u32,
    /// The device's contexts, as the controller keeps them and as the driver
    /// gives them; held for as long as the slot is in use.
    #[allow(dead_code)]
    output: DmaPage,
    #[allow(dead_code)]
    input: DmaPage,
    ep0: Ring,
    buffer: DmaPage,
    keyboard: Option<KeyboardEndpoint>,
}

struct Controller {
    pci: pci::Address,
    op: u64,
    runtime: u64,
    doorbells: u64,
    /// Bytes per context: 32, or 64 when HCCPARAMS1.CSZ says so.
    context: u64,
    /// The controller reaches all of memory (HCCPARAMS1.AC64); without it, every
    /// page it is given must lie below 4 GiB.
    wide: bool,
    ports: u32,
    dcbaa: DmaPage,
    commands: Ring,
    events: EventRing,
    devices: Vec<Device>,
    /// Ports already looked at (at boot, or reported since), by port number.
    ports_seen: Vec<bool>,
}

/// A request on the default control endpoint.
struct Setup {
    request_type: u8,
    request: u8,
    value: u16,
    index: u16,
    length: u16,
}

/// A page the controller can reach: below 4 GiB unless it has 64-bit addressing.
/// One it cannot reach is given back and the controller goes without: at boot,
/// when this runs, frames are still handed out from the bottom of memory up, so a
/// page above 4 GiB means there is none lower to be had.
fn page_for(wide: bool) -> Option<DmaPage> {
    let page = DmaPage::new().ok()?;
    if !wide && page.phys + 4096 > 1 << 32 {
        crate::mm::frame::free_phys(page.phys);
        return None;
    }
    Some(page)
}

impl Controller {
    fn port_sc(&self, port: u32) -> u64 {
        self.op + OP_PORTS + 0x10 * u64::from(port - 1)
    }

    fn ring_doorbell(&self, slot: u32, target: u32) {
        crate::arch::dma_mb();
        wr32(self.doorbells + 4 * u64::from(slot), target);
    }

    /// Tell the controller how far the event ring has been read.
    fn events_done(&self) {
        wr64(self.runtime + IR0 + IR_ERDP, self.events.dequeue_pointer() | ERDP_BUSY);
    }

    /// Wait for the event `want` picks out, handling every other event on the way
    /// (a keyboard's reports keep arriving while another device is set up).
    fn wait_event(&mut self, ms: u64, want: impl Fn(&[u32; 4]) -> bool) -> Option<[u32; 4]> {
        let mut found = None;
        wait::until(ms, || {
            while let Some(ev) = self.events.next() {
                self.events_done();
                if want(&ev) {
                    found = Some(ev);
                    return true;
                }
                self.handle(ev);
            }
            false
        });
        found
    }

    /// Run one command; the completion event's dwords when it succeeds.
    fn command(&mut self, trb: [u32; 4]) -> Result<[u32; 4], &'static str> {
        let at = self.commands.push(trb);
        self.ring_doorbell(0, 0);
        let ev = self
            .wait_event(COMMAND_MS, |ev| {
                (ev[3] >> 10) & 0x3F == TRB_COMMAND_COMPLETION
                    && (u64::from(ev[0]) | (u64::from(ev[1]) << 32)) == at
            })
            .ok_or("a command was never completed")?;
        if ev[2] >> 24 != COMPLETION_SUCCESS {
            return Err("a command failed");
        }
        Ok(ev)
    }

    /// One control transfer on device `d`'s default endpoint; IN data lands in its
    /// buffer page, OUT data is taken from it.
    fn control(&mut self, d: usize, s: Setup) -> Result<(), &'static str> {
        let dev = &mut self.devices[d];
        let slot = dev.slot;
        let input = s.request_type & 0x80 != 0;
        // Transfer type: 0 no data, 2 OUT data, 3 IN data.
        let trt = match (s.length, input) {
            (0, _) => 0,
            (_, false) => 2,
            (_, true) => 3,
        };
        dev.ep0.push([
            u32::from(s.request_type) | (u32::from(s.request) << 8) | (u32::from(s.value) << 16),
            u32::from(s.index) | (u32::from(s.length) << 16),
            8,
            TRB_IMMEDIATE | (TRB_SETUP << 10) | (trt << 16),
        ]);
        if s.length > 0 {
            let dir = if input { TRB_DIR_IN } else { 0 };
            dev.ep0.push([
                dev.buffer.phys as u32,
                (dev.buffer.phys >> 32) as u32,
                u32::from(s.length),
                (TRB_DATA << 10) | dir,
            ]);
        }
        // The status stage goes the other way from the data (IN when there is none).
        let status_dir = if s.length > 0 && input { 0 } else { TRB_DIR_IN };
        let status = dev.ep0.push([0, 0, 0, (TRB_STATUS << 10) | status_dir | TRB_IOC]);
        self.ring_doorbell(slot, 1);
        let ev = self
            .wait_event(COMMAND_MS, |ev| {
                let code = ev[2] >> 24;
                (ev[3] >> 10) & 0x3F == TRB_TRANSFER_EVENT
                    && ev[3] >> 24 == slot
                    && (ev[3] >> 16) & 0x1F == 1
                    && ((u64::from(ev[0]) | (u64::from(ev[1]) << 32)) == status
                        || (code != COMPLETION_SUCCESS && code != COMPLETION_SHORT_PACKET))
            })
            .ok_or("a control transfer was never completed")?;
        match ev[2] >> 24 {
            COMPLETION_SUCCESS | COMPLETION_SHORT_PACKET => Ok(()),
            6 => Err("the device stalled a request"),
            _ => Err("a control transfer failed"),
        }
    }

    /// The device's buffer page, as bytes.
    fn buffer(&self, d: usize, len: usize) -> &[u8] {
        // SAFETY: the page is the driver's; the controller wrote it and is done.
        unsafe { core::slice::from_raw_parts(self.devices[d].buffer.virt as *const u8, len.min(4096)) }
    }

    /// Reset the device on `port`, give it an address and, when it is a boot
    /// keyboard, configure it and start taking its reports.
    fn attach(&mut self, port: u32) -> Result<(), &'static str> {
        let sc_at = self.port_sc(port);
        let sc = rd32(sc_at);
        if sc & PORT_ENABLED == 0 {
            // USB 2 ports are enabled by a reset; USB 3 ones enable themselves.
            wr32(sc_at, (sc & PORT_KEEP) | PORT_RESET);
            if !wait::until(PORT_RESET_MS, || rd32(sc_at) & PORT_RESET_CHANGE != 0) {
                return Err("the port did not finish its reset");
            }
            wr32(sc_at, (rd32(sc_at) & PORT_KEEP) | PORT_CHANGES);
            if rd32(sc_at) & PORT_ENABLED == 0 {
                return Err("the port is not enabled after its reset");
            }
            wait::pause(RESET_RECOVERY_MS);
        }
        let speed = (rd32(sc_at) >> 10) & 0xF;
        let ev = self.command([0, 0, 0, TRB_ENABLE_SLOT << 10])?;
        let slot = ev[3] >> 24;
        if slot == 0 || slot > MAX_SLOTS {
            return Err("the controller gave an unusable slot");
        }
        let w = self.wide;
        let (Some(output), Some(input), Some(ep0), Some(buffer)) =
            (page_for(w), page_for(w), Ring::new(w), page_for(w))
        else {
            return Err("no memory the controller can reach for the device");
        };
        mem_wr32(self.dcbaa.virt + 8 * u64::from(slot), output.phys as u32);
        mem_wr32(self.dcbaa.virt + 8 * u64::from(slot) + 4, (output.phys >> 32) as u32);
        // The default endpoint's packet size: 8 until the device says otherwise at
        // full speed, fixed at the others.
        let mps0 = match speed {
            SPEED_HIGH => 64,
            SPEED_SUPER.. => 512,
            _ => 8,
        };
        let c = self.context;
        let ictx = input.virt;
        mem_wr32(ictx + 4, 0b11); // add the slot and endpoint 0
        mem_wr32(ictx + c, (1 << 27) | (speed << 20)); // one context entry
        mem_wr32(ictx + c + 4, port << 16);
        let ep0_ctx = ictx + 2 * c;
        mem_wr32(ep0_ctx + 4, (3 << 1) | (EP_CONTROL << 3) | (mps0 << 16));
        mem_wr32(ep0_ctx + 8, ep0.page.phys as u32 | 1);
        mem_wr32(ep0_ctx + 12, (ep0.page.phys >> 32) as u32);
        mem_wr32(ep0_ctx + 16, 8);
        let input_phys = input.phys;
        self.devices.push(Device { slot, port, output, input, ep0, buffer, keyboard: None });
        let d = self.devices.len() - 1;
        self.command([
            input_phys as u32,
            (input_phys >> 32) as u32,
            0,
            (TRB_ADDRESS_DEVICE << 10) | (slot << 24),
        ])?;

        let get = |kind: u16, length: u16| Setup {
            request_type: 0x80,
            request: REQ_GET_DESCRIPTOR,
            value: kind << 8,
            index: 0,
            length,
        };
        self.control(d, get(DESC_DEVICE, 8))?;
        let real_mps0 = u32::from(self.buffer(d, 8)[7]);
        if speed == SPEED_FULL && real_mps0 != mps0 && matches!(real_mps0, 16 | 32 | 64) {
            mem_wr32(ictx, 0);
            mem_wr32(ictx + 4, 0b10); // evaluate endpoint 0 only
            mem_wr32(ep0_ctx + 4, (3 << 1) | (EP_CONTROL << 3) | (real_mps0 << 16));
            self.command([
                input_phys as u32,
                (input_phys >> 32) as u32,
                0,
                (TRB_EVALUATE_CONTEXT << 10) | (slot << 24),
            ])?;
        }
        self.control(d, get(DESC_DEVICE, 18))?;
        let desc = self.buffer(d, 18);
        let vendor = u16::from_le_bytes([desc[8], desc[9]]);
        let product = u16::from_le_bytes([desc[10], desc[11]]);
        self.control(d, get(DESC_CONFIGURATION, 9))?;
        let total = u16::from_le_bytes([self.buffer(d, 9)[2], self.buffer(d, 9)[3]]).clamp(9, 4096);
        let config_value = self.buffer(d, 9)[5];
        self.control(d, get(DESC_CONFIGURATION, total))?;

        // The first boot keyboard interface (alternate setting 0) and its interrupt
        // IN endpoint.
        let mut found = None;
        let mut iface: Option<(u8, bool)> = None;
        let desc = self.buffer(d, usize::from(total));
        let mut at = 0;
        while at + 2 <= desc.len() {
            let len = usize::from(desc[at]);
            if len < 2 || at + len > desc.len() {
                break;
            }
            let b = &desc[at..at + len];
            match b[1] {
                DESC_INTERFACE if len >= 9 => {
                    let keyboard =
                        b[3] == 0 && b[5] == CLASS_HID && b[6] == SUBCLASS_BOOT && b[7] == PROTOCOL_KEYBOARD;
                    iface = Some((b[2], keyboard));
                }
                DESC_ENDPOINT if len >= 7 => {
                    if let Some((number, true)) = iface
                        && b[2] & 0x80 != 0
                        && b[3] & 3 == 3
                        && found.is_none()
                    {
                        let mps = u32::from(u16::from_le_bytes([b[4], b[5]]) & 0x7FF);
                        found = Some((number, b[2], mps, b[6]));
                    }
                }
                _ => {}
            }
            at += len;
        }
        let Some((interface, address, mps, interval)) = found else {
            println!(
                "[kernel] usb: {} port {port} ({}): device {vendor:04x}:{product:04x} is not a boot keyboard; left unconfigured",
                self.pci,
                speed_name(speed)
            );
            return Ok(());
        };

        self.control(
            d,
            Setup {
                request_type: 0x00,
                request: REQ_SET_CONFIGURATION,
                value: config_value.into(),
                index: 0,
                length: 0,
            },
        )?;
        // The boot protocol: the eight-byte report the decoder knows, whatever the
        // report descriptor would describe.
        self.control(
            d,
            Setup {
                request_type: 0x21,
                request: HID_SET_PROTOCOL,
                value: 0,
                index: interface.into(),
                length: 0,
            },
        )?;

        let dci = u32::from(address & 0xF) * 2 + 1;
        // xHCI intervals are 2^n units of 125 us: from frames (ms) at full and low
        // speed, from 2^(bInterval-1) microframes at the others.
        let xinterval = match speed {
            SPEED_FULL | SPEED_LOW => (31 - u32::from(interval.max(1)).leading_zeros() + 3).clamp(3, 10),
            _ => u32::from(interval.clamp(1, 16)) - 1,
        };
        let (Some(ring), Some(reports)) = (Ring::new(self.wide), page_for(self.wide)) else {
            return Err("no memory the controller can reach for the keyboard");
        };
        let length = mps.clamp(1, 64);
        for w in 0..(8 * c / 4) {
            mem_wr32(ictx + 4 * w, 0);
        }
        mem_wr32(ictx + 4, 1 | (1 << dci)); // add the slot and the endpoint
        mem_wr32(ictx + c, (dci << 27) | (speed << 20));
        mem_wr32(ictx + c + 4, port << 16);
        let ep = ictx + c * (1 + u64::from(dci));
        for w in 0..(c / 4) {
            mem_wr32(ep + 4 * w, 0);
        }
        mem_wr32(ep, xinterval << 16);
        mem_wr32(ep + 4, (3 << 1) | (EP_INTERRUPT_IN << 3) | (mps << 16));
        mem_wr32(ep + 8, ring.page.phys as u32 | 1);
        mem_wr32(ep + 12, (ring.page.phys >> 32) as u32);
        mem_wr32(ep + 16, 8 | (mps << 16));
        self.command([
            input_phys as u32,
            (input_phys >> 32) as u32,
            0,
            (TRB_CONFIGURE_ENDPOINT << 10) | (slot << 24),
        ])?;
        let mut kb = KeyboardEndpoint { dci, ring, reports, state: hid::Keyboard::default(), working: true };
        for i in 0..REPORTS_QUEUED {
            let buf = kb.reports.phys + 64 * i as u64;
            kb.ring.push([
                buf as u32,
                (buf >> 32) as u32,
                length,
                (TRB_NORMAL << 10) | TRB_IOC | TRB_SHORT_OK,
            ]);
        }
        self.devices[d].keyboard = Some(kb);
        self.ring_doorbell(slot, dci);
        println!(
            "[kernel] usb: {} port {port} ({}): keyboard {vendor:04x}:{product:04x} ready -- boot protocol, interface {interface}, endpoint {address:#04x}, {length}-byte reports every {} us",
            self.pci,
            speed_name(speed),
            125u32 << xinterval
        );
        Ok(())
    }

    /// One event that nothing is waiting for: a keyboard's report, or a port.
    fn handle(&mut self, ev: [u32; 4]) {
        match (ev[3] >> 10) & 0x3F {
            TRB_TRANSFER_EVENT => {
                let (slot, ep) = (ev[3] >> 24, (ev[3] >> 16) & 0x1F);
                let trb_at = u64::from(ev[0]) | (u64::from(ev[1]) << 32);
                let Some(dev) = self.devices.iter_mut().find(|d| d.slot == slot) else { return };
                let Some(kb) = dev.keyboard.as_mut().filter(|k| k.dci == ep && k.working) else { return };
                let Some(trb) = kb.ring.trb_at(trb_at) else { return };
                let buf = u64::from(trb[0]) | (u64::from(trb[1]) << 32);
                let Some(off) = buf.checked_sub(kb.reports.phys).filter(|&o| o + 64 <= 4096) else { return };
                let code = ev[2] >> 24;
                if code != COMPLETION_SUCCESS && code != COMPLETION_SHORT_PACKET {
                    // A halted endpoint or a device that went away: no more reports.
                    kb.working = false;
                    kb.state.release_all();
                    println!(
                        "[kernel] usb: keyboard on port {}: transfer failed (code {code}); stopped",
                        dev.port
                    );
                    return;
                }
                let got = (trb[2] & 0x1_FFFF).saturating_sub(ev[2] & 0xFF_FFFF) as usize;
                let mut report = [0u8; 8];
                for (i, b) in report.iter_mut().enumerate().take(got.min(8)) {
                    // SAFETY: inside the report page; the controller is done with it.
                    *b = unsafe { core::ptr::read_volatile((kb.reports.virt + off + i as u64) as *const u8) };
                }
                if got >= 8 {
                    kb.state.report(&report);
                }
                kb.ring.push([
                    trb[0],
                    trb[1],
                    trb[2] & 0x1_FFFF,
                    (TRB_NORMAL << 10) | TRB_IOC | TRB_SHORT_OK,
                ]);
                let dci = kb.dci;
                self.ring_doorbell(slot, dci);
            }
            TRB_PORT_STATUS_CHANGE => {
                let port = ev[0] >> 24;
                if port == 0 || port > self.ports {
                    return;
                }
                let sc_at = self.port_sc(port);
                let sc = rd32(sc_at);
                wr32(sc_at, (sc & PORT_KEEP) | (sc & PORT_CHANGES));
                let known = self.devices.iter_mut().find(|d| d.port == port);
                if sc & PORT_CONNECTED == 0 {
                    if let Some(dev) = known
                        && let Some(kb) = dev.keyboard.as_mut()
                        && kb.working
                    {
                        kb.working = false;
                        kb.state.release_all();
                        println!("[kernel] usb: keyboard on port {port} disconnected");
                    }
                } else if known.is_none() && !self.ports_seen[port as usize] {
                    self.ports_seen[port as usize] = true;
                    println!(
                        "[kernel] usb: {} port {port}: a device was connected after boot; not used (no hot-plug yet)",
                        self.pci
                    );
                }
            }
            _ => {}
        }
    }

    fn poll(&mut self) {
        let mut n = 0;
        while n < EVENTS_PER_TICK {
            let Some(ev) = self.events.next() else { break };
            self.handle(ev);
            n += 1;
        }
        if n > 0 {
            self.events_done();
        }
    }
}

/// Take the controller from the firmware: ask for it through the legacy-support
/// capability and stop the firmware's SMIs. A firmware that does not let go within
/// the limit is overruled.
fn take_from_firmware(base: u64, xecp: u64, addr: pci::Address) {
    let mut at = xecp;
    for _ in 0..64 {
        if at == 0 {
            return;
        }
        let cap = rd32(base + at);
        if cap & 0xFF == XCAP_LEGACY {
            let reg = base + at;
            if cap & LEGACY_BIOS_OWNED != 0 {
                wr32(reg, cap | LEGACY_OS_OWNED);
                if !wait::until(HANDOFF_MS, || rd32(reg) & LEGACY_BIOS_OWNED == 0) {
                    println!(
                        "[kernel] xhci: {addr}: the firmware did not hand the controller over; taking it"
                    );
                    wr32(reg, (rd32(reg) & !LEGACY_BIOS_OWNED) | LEGACY_OS_OWNED);
                }
            } else {
                wr32(reg, cap | LEGACY_OS_OWNED);
            }
            let ctl = rd32(reg + 4);
            wr32(reg + 4, (ctl & !LEGACY_SMI_ENABLES) | LEGACY_SMI_STATUS);
            return;
        }
        let next = u64::from((cap >> 8) & 0xFF);
        if next == 0 {
            return;
        }
        at += next * 4;
    }
}

fn bring_up(addr: pci::Address, bar: u64) -> Result<Controller, &'static str> {
    const NO_MEMORY: &str = "no memory it can reach for its rings";
    let caps = crate::mm::mmio::map(bar, 0x1000).map_err(|_| "no room to map the controller")?;
    let first = rd32(caps + CAP_LENGTH_VERSION);
    let cap_length = u64::from(first & 0xFF);
    let version = first >> 16;
    let hcs1 = rd32(caps + CAP_HCSPARAMS1);
    let hcs2 = rd32(caps + CAP_HCSPARAMS2);
    let hcc1 = rd32(caps + CAP_HCCPARAMS1);
    let db_off = u64::from(rd32(caps + CAP_DBOFF) & !3);
    let rt_off = u64::from(rd32(caps + CAP_RTSOFF) & !0x1F);
    let slots = (hcs1 & 0xFF).min(MAX_SLOTS);
    let ports = (hcs1 >> 24) & 0xFF;
    let scratchpads = ((hcs2 >> 27) & 0x1F) | (((hcs2 >> 21) & 0x1F) << 5);
    let context = if hcc1 & (1 << 2) != 0 { 64 } else { 32 };
    let wide = hcc1 & 1 != 0;
    let port_power = hcc1 & (1 << 3) != 0;
    let xecp = u64::from(hcc1 >> 16) * 4;
    if cap_length < 0x20 || slots == 0 || ports == 0 {
        return Err("its capability registers make no sense");
    }
    // Everything the driver touches: the ports, interrupter 0, the doorbells it
    // rings, and the extended capabilities.
    let span = (cap_length + OP_PORTS + 0x10 * u64::from(ports))
        .max(rt_off + IR0 + 0x20)
        .max(db_off + 4 * (u64::from(slots) + 1))
        .max(xecp + 0x1000);
    let base = crate::mm::mmio::map(bar, span).map_err(|_| "no room to map the controller")?;
    let op = base + cap_length;
    take_from_firmware(base, xecp, addr);

    // Stop it, then reset it: whatever the firmware left it doing ends here.
    wr32(op + OP_USBCMD, rd32(op + OP_USBCMD) & !CMD_RUN);
    if !wait::until(HALT_MS, || rd32(op + OP_USBSTS) & STS_HALTED != 0) {
        return Err("it does not stop");
    }
    wr32(op + OP_USBCMD, CMD_RESET);
    wait::pause(1);
    if !wait::until(RESET_MS, || {
        rd32(op + OP_USBCMD) & CMD_RESET == 0 && rd32(op + OP_USBSTS) & STS_NOT_READY == 0
    }) {
        return Err("it does not come out of reset");
    }
    if rd32(op + OP_PAGESIZE) & 1 == 0 {
        return Err("it cannot use 4 KiB pages");
    }

    let dcbaa = page_for(wide).ok_or(NO_MEMORY)?;
    if scratchpads > 0 {
        // Pages the controller keeps its own state in: an array of their addresses
        // in slot 0 of the context array.
        if scratchpads > 512 {
            return Err("it wants more scratchpad pages than this driver gives");
        }
        let array = page_for(wide).ok_or(NO_MEMORY)?;
        for i in 0..u64::from(scratchpads) {
            let page = page_for(wide).ok_or(NO_MEMORY)?;
            mem_wr32(array.virt + 8 * i, page.phys as u32);
            mem_wr32(array.virt + 8 * i + 4, (page.phys >> 32) as u32);
        }
        mem_wr32(dcbaa.virt, array.phys as u32);
        mem_wr32(dcbaa.virt + 4, (array.phys >> 32) as u32);
    }
    let commands = Ring::new(wide).ok_or(NO_MEMORY)?;
    let event_page = page_for(wide).ok_or(NO_MEMORY)?;
    let table = page_for(wide).ok_or(NO_MEMORY)?;
    mem_wr32(table.virt, event_page.phys as u32);
    mem_wr32(table.virt + 4, (event_page.phys >> 32) as u32);
    mem_wr32(table.virt + 8, RING_TRBS as u32);

    wr32(op + OP_CONFIG, (rd32(op + OP_CONFIG) & !0xFF) | slots);
    wr64(op + OP_DCBAAP, dcbaa.phys);
    wr64(op + OP_CRCR, commands.page.phys | 1);
    let ir = base + rt_off + IR0;
    wr32(ir + IR_IMAN, 0b01); // clear a pending flag; interrupts stay off
    wr32(ir + IR_ERSTSZ, 1);
    wr64(ir + IR_ERSTBA, table.phys);
    wr64(ir + IR_ERDP, event_page.phys);
    crate::arch::dma_mb();
    wr32(op + OP_USBCMD, CMD_RUN);
    if !wait::until(HALT_MS, || rd32(op + OP_USBSTS) & STS_HALTED == 0) {
        return Err("it does not start");
    }
    if port_power {
        // A controller that switches port power leaves its ports off after a reset.
        for port in 1..=ports {
            let sc_at = op + OP_PORTS + 0x10 * u64::from(port - 1);
            wr32(sc_at, (rd32(sc_at) & PORT_KEEP) | PORT_POWER);
        }
        // USB 2.0 §11.11: power-on to power-good, 20 ms at the most for a root port.
        wait::pause(20);
    }
    println!(
        "[kernel] xhci: {addr}: xHCI {}.{}, {ports} ports, {slots} slots, {context}-byte contexts{}",
        version >> 8,
        (version >> 4) & 0xF,
        if scratchpads > 0 {
            alloc::format!(", {scratchpads} scratchpad pages")
        } else {
            alloc::string::String::new()
        }
    );
    Ok(Controller {
        pci: addr,
        op,
        runtime: base + rt_off,
        doorbells: base + db_off,
        context,
        wide,
        ports,
        dcbaa,
        commands,
        events: EventRing { page: event_page, table, dequeue: 0, cycle: true },
        devices: Vec::new(),
        ports_seen: alloc::vec![false; ports as usize + 1],
    })
}

/// Find every xHCI controller, bring it up, and set up the keyboards on its ports.
pub fn init() {
    for addr in pci::find_class(0x0C, 0x03, 0x30).into_iter().take(MAX_CONTROLLERS) {
        let Some(bar) = addr.bar_address(0) else {
            println!("[kernel] xhci: {addr} has no register BAR; not used");
            continue;
        };
        addr.enable_memory_and_bus_master();
        addr.disable_intx();
        let mut c = match bring_up(addr, bar) {
            Ok(c) => c,
            Err(e) => {
                println!("[kernel] xhci: {addr}: {e}; not used");
                continue;
            }
        };
        // Let the ports settle: a device needs up to 100 ms after power to show
        // up (USB 2.0 §7.1.7.3).
        wait::pause(100);
        // Every port with a device now is the boot's to set up: none of them is
        // reported as plugged in later, whichever order their events come in.
        let connected: Vec<u32> =
            (1..=c.ports).filter(|&p| rd32(c.port_sc(p)) & PORT_CONNECTED != 0).collect();
        for &port in &connected {
            c.ports_seen[port as usize] = true;
        }
        for &port in &connected {
            if let Err(e) = c.attach(port) {
                println!(
                    "[kernel] usb: {addr} port {port}: {e}; device not used (USBSTS {:#x})",
                    rd32(c.op + OP_USBSTS)
                );
            }
            // Its connection is known; the change bits are cleared.
            let sc_at = c.port_sc(port);
            wr32(sc_at, (rd32(sc_at) & PORT_KEEP) | PORT_CHANGES);
        }
        // Events from the setup (port changes) are taken now, not on the first tick.
        c.poll();
        let keyboards = c.devices.iter().filter(|d| d.keyboard.is_some()).count();
        println!(
            "[kernel] xhci: {addr}: {} device(s) on its ports, {keyboards} keyboard(s)",
            connected.len()
        );
        let mut all = CONTROLLERS.lock();
        if all.try_reserve(1).is_ok() {
            all.push(c);
        }
    }
    if !CONTROLLERS.lock().is_empty() {
        ACTIVE.store(true, Ordering::Release);
    }
}

/// Timer-tick hook on the boot CPU (interrupts disabled): take what the
/// controllers reported since the last tick. A tick that finds the driver busy
/// leaves it to the next one.
pub fn poll_tick() {
    if !ACTIVE.load(Ordering::Acquire) {
        return;
    }
    if let Some(mut all) = CONTROLLERS.try_lock() {
        for c in all.iter_mut() {
            c.poll();
        }
    }
}
