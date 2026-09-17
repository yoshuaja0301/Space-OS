//! Device discovery and drivers.
//!
//! Stage 3 of the roadmap: PCI enumeration and a polled VirtIO block driver, which
//! together give the kernel a guest disk to read the model from (requirement D01).
//! Drivers live in the kernel for the MVP; moving them to user space with scoped
//! MMIO/IRQ capabilities is Developer Preview work (see docs/adr/0007).

pub mod pci;
pub mod virtio_blk;

pub fn init() {
    pci::init();
    virtio_blk::init();
}
