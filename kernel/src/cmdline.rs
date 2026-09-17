//! Kernel command line (`cmdline=` from `spaceos.cfg`), `key=value` tokens separated by whitespace.

use spaceabi::boot::BootInfo;

use crate::mm::phys_to_virt;
use crate::sync::SpinLock;

static CMDLINE: SpinLock<&'static str> = SpinLock::new("");

pub fn init(bi: &BootInfo) {
    if bi.cmdline.len == 0 {
        return;
    }
    // SAFETY: the bootloader copied the command line into KERNEL memory that stays mapped.
    let bytes: &'static [u8] = unsafe {
        core::slice::from_raw_parts(phys_to_virt(bi.cmdline.phys).as_ptr::<u8>(), bi.cmdline.len as usize)
    };
    let s = core::str::from_utf8(bytes).unwrap_or("");
    *CMDLINE.lock() = s.trim();
    println!("[kernel] cmdline: {:?}", s.trim());
}

pub fn get(key: &str) -> Option<&'static str> {
    let line = *CMDLINE.lock();
    line.split_whitespace().find_map(|tok| {
        let (k, v) = tok.split_once('=')?;
        (k == key).then_some(v)
    })
}
