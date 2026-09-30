//! Device discovery and drivers.
//!
//! Stage 3 of the roadmap: PCI enumeration; polled block drivers -- VirtIO, AHCI
//! (SATA) and NVMe -- one of whose disks holds the data volume the model is read
//! from (requirement D01, ADR-0025); a polled VirtIO network driver that moves
//! Ethernet frames for the user-space network service (the transport the cloud
//! adapter of I01 needs); and an entropy source for keys (virtio-rng, else RDRAND).
//! Drivers live in the kernel for the MVP; moving them to user space with scoped
//! MMIO/IRQ capabilities is Developer Preview work (see docs/adr/0007).

pub mod ahci;
pub mod block;
pub mod dma;
pub mod entropy;
pub mod nvme;
pub mod pci;
pub mod virtio;
pub mod virtio_blk;
pub mod virtio_net;
pub mod virtio_rng;
pub mod wait;

pub fn init() {
    pci::init();
    virtio_blk::init();
    ahci::init();
    nvme::init();
    // Of every disk found, the one holding the data volume.
    block::init();
    virtio_net::init();
    virtio_rng::init();
    crate::println!("[kernel] entropy: {}", entropy::describe());
}
