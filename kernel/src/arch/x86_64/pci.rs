//! PCI configuration space over the legacy 0xCF8/0xCFC ports (mechanism #1).

use x86_64::instructions::port::Port;

use crate::sync::SpinLock;

const CONFIG_ADDRESS: u16 = 0xCF8;
const CONFIG_DATA: u16 = 0xCFC;

/// Serialises the address/data port pair.
static CONFIG_LOCK: SpinLock<()> = SpinLock::new(());

fn encode(bus: u8, device: u8, function: u8, offset: u8) -> u32 {
    0x8000_0000
        | ((bus as u32) << 16)
        | ((device as u32) << 11)
        | ((function as u32) << 8)
        | ((offset as u32) & 0xFC)
}

pub fn read32(bus: u8, device: u8, function: u8, offset: u8) -> u32 {
    let _g = CONFIG_LOCK.lock();
    // SAFETY: the standard PCI configuration mechanism #1 port pair.
    unsafe {
        Port::<u32>::new(CONFIG_ADDRESS).write(encode(bus, device, function, offset));
        Port::<u32>::new(CONFIG_DATA).read()
    }
}

pub fn write32(bus: u8, device: u8, function: u8, offset: u8, value: u32) {
    let _g = CONFIG_LOCK.lock();
    // SAFETY: as above.
    unsafe {
        Port::<u32>::new(CONFIG_ADDRESS).write(encode(bus, device, function, offset));
        Port::<u32>::new(CONFIG_DATA).write(value);
    }
}
