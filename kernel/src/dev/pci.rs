//! PCI configuration space: enough to find a device, read its BARs and enable
//! memory space plus bus mastering. How a configuration register is reached is the
//! architecture's (`arch::pci`: the 0xCF8/0xCFC ports on x86-64, ECAM on AArch64);
//! PCIe extended capabilities are not needed by the devices the system uses.

use alloc::vec::Vec;

use crate::arch;
use crate::sync::SpinLock;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Address {
    pub bus: u8,
    pub device: u8,
    pub function: u8,
}

impl core::fmt::Display for Address {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{:02x}:{:02x}.{}", self.bus, self.device, self.function)
    }
}

impl Address {
    pub fn read32(&self, offset: u8) -> u32 {
        arch::pci::read32(self.bus, self.device, self.function, offset)
    }

    pub fn write32(&self, offset: u8, value: u32) {
        arch::pci::write32(self.bus, self.device, self.function, offset, value)
    }

    pub fn read16(&self, offset: u8) -> u16 {
        (self.read32(offset & !3) >> ((offset & 2) * 8)) as u16
    }

    pub fn read8(&self, offset: u8) -> u8 {
        (self.read32(offset & !3) >> ((offset & 3) * 8)) as u8
    }

    pub fn vendor_id(&self) -> u16 {
        self.read16(0x00)
    }

    pub fn device_id(&self) -> u16 {
        self.read16(0x02)
    }

    pub fn header_type(&self) -> u8 {
        self.read8(0x0E)
    }

    /// Enable memory-space decoding and bus mastering (DMA).
    pub fn enable_memory_and_bus_master(&self) {
        let cmd = self.read32(0x04);
        self.write32(0x04, cmd | 0x6);
    }

    /// Stop the function from asserting its legacy INTx line (command bit 10).
    /// For drivers that poll: a line nobody services must never be left asserted.
    pub fn disable_intx(&self) {
        let cmd = self.read32(0x04);
        self.write32(0x04, cmd | (1 << 10));
    }

    /// Base address of BAR `index`, or `None` when the BAR is unused or I/O space.
    /// 64-bit BARs consume two slots; pass the lower index.
    pub fn bar_address(&self, index: u8) -> Option<u64> {
        let offset = 0x10 + index * 4;
        let low = self.read32(offset);
        if low & 1 != 0 {
            return None; // I/O space
        }
        let base = (low & 0xFFFF_FFF0) as u64;
        if (low >> 1) & 3 == 2 {
            let high = self.read32(offset + 4) as u64;
            let addr = base | (high << 32);
            (addr != 0).then_some(addr)
        } else {
            (base != 0).then_some(base)
        }
    }

    /// Walk the capability list, yielding `(id, offset)` pairs.
    pub fn capabilities(&self) -> Vec<(u8, u8)> {
        let mut caps = Vec::new();
        if self.read16(0x06) & (1 << 4) == 0 {
            return caps; // no capability list
        }
        let mut ptr = self.read8(0x34) & 0xFC;
        let mut guard = 0;
        while ptr != 0 && guard < 48 {
            caps.push((self.read8(ptr), ptr));
            ptr = self.read8(ptr + 1) & 0xFC;
            guard += 1;
        }
        caps
    }
}

static DEVICES: SpinLock<Vec<Address>> = SpinLock::new(Vec::new());

pub fn init() {
    let mut found = Vec::new();
    for bus in 0u16..=255 {
        for device in 0u8..32 {
            let base = Address { bus: bus as u8, device, function: 0 };
            if base.vendor_id() == 0xFFFF {
                continue;
            }
            let multi = base.header_type() & 0x80 != 0;
            let functions = if multi { 8 } else { 1 };
            for function in 0..functions {
                let addr = Address { bus: bus as u8, device, function };
                if addr.vendor_id() != 0xFFFF {
                    found.push(addr);
                }
            }
        }
    }
    println!("[kernel] pci: {} functions present", found.len());
    *DEVICES.lock() = found;
}

/// Every function of class `class`, subclass `subclass` and programming interface
/// `prog_if` (e.g. 01/06/01 for AHCI, 01/08/02 for NVMe), in bus order.
pub fn find_class(class: u8, subclass: u8, prog_if: u8) -> Vec<Address> {
    DEVICES
        .lock()
        .iter()
        .copied()
        .filter(|a| a.read8(0x0B) == class && a.read8(0x0A) == subclass && a.read8(0x09) == prog_if)
        .collect()
}

/// First device matching `vendor` and any of `devices`.
/// Every function with `vendor` and one of `devices`, in bus order.
pub fn find_all(vendor: u16, devices: &[u16]) -> Vec<Address> {
    DEVICES
        .lock()
        .iter()
        .copied()
        .filter(|a| a.vendor_id() == vendor && devices.contains(&a.device_id()))
        .collect()
}

pub fn find(vendor: u16, devices: &[u16]) -> Option<Address> {
    DEVICES.lock().iter().copied().find(|a| a.vendor_id() == vendor && devices.contains(&a.device_id()))
}
