//! Initial RAM disk: a ustar archive placed in memory by the bootloader.

use spaceabi::boot::BootInfo;
use spaceabi::tar::Tar;

use crate::mm::phys_to_virt;
use crate::sync::SpinLock;

static INITRD: SpinLock<Option<&'static [u8]>> = SpinLock::new(None);

pub fn init(bi: &BootInfo) {
    if bi.initrd.len == 0 {
        println!("[kernel] no initrd");
        return;
    }
    // SAFETY: KERNEL-typed memory that stays mapped for the lifetime of the kernel.
    let data: &'static [u8] = unsafe {
        core::slice::from_raw_parts(phys_to_virt(bi.initrd.phys).as_ptr::<u8>(), bi.initrd.len as usize)
    };
    let count = Tar::new(data).entries().count();
    *INITRD.lock() = Some(data);
    println!("[kernel] initrd: {} bytes, {} files", data.len(), count);
    for e in Tar::new(data).entries() {
        println!("[kernel]   {} ({} bytes)", e.name, e.data.len());
    }
}

pub fn find(name: &str) -> Option<&'static [u8]> {
    let data = (*INITRD.lock())?;
    Tar::new(data).find(name)
}
