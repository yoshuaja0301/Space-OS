//! Device discovery and drivers.
//!
//! Stage 3 of the roadmap: PCI enumeration, a polled VirtIO block driver, which
//! gives the kernel a guest disk to read the model from (requirement D01), and a
//! polled VirtIO network driver that moves Ethernet frames for the user-space
//! network service (the transport the cloud adapter of I01 needs).
//! Drivers live in the kernel for the MVP; moving them to user space with scoped
//! MMIO/IRQ capabilities is Developer Preview work (see docs/adr/0007).

pub mod pci;
pub mod virtio;
pub mod virtio_blk;
pub mod virtio_net;

pub fn init() {
    pci::init();
    virtio_blk::init();
    virtio_net::init();
}
