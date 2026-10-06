//! Device discovery and drivers.
//!
//! Stage 3 of the roadmap: PCI enumeration; polled block drivers -- VirtIO, AHCI
//! (SATA) and NVMe -- one of whose disks holds the data volume the model is read
//! from (requirement D01, ADR-0025); polled network drivers -- VirtIO and Intel
//! e1000 -- that move Ethernet frames for the user-space network service, behind
//! one lease (the transport the cloud adapter of I01 needs, ADR-0026); and an
//! entropy source for keys (virtio-rng, else RDRAND); and USB keyboards on xHCI
//! controllers (ADR-0030).
//! Drivers live in the kernel for the MVP; moving them to user space with scoped
//! MMIO/IRQ capabilities is Developer Preview work (see docs/adr/0007).

pub mod ahci;
pub mod block;
pub mod dma;
pub mod e1000;
pub mod entropy;
pub mod hid;
pub mod nic;
pub mod nvme;
pub mod pci;
pub mod virtio;
pub mod virtio_blk;
pub mod virtio_net;
pub mod virtio_rng;
pub mod wait;
pub mod xhci;

pub fn init() {
    pci::init();
    virtio_blk::init();
    ahci::init();
    nvme::init();
    // Of every disk found, the one holding the data volume.
    block::init();
    nic::init();
    virtio_rng::init();
    // Firmware entropy from the hand-over is mixed into what a source gives, and
    // is never a source itself: "none" stays none (ADR-0032).
    let mixed = if entropy::boot_entropy_mixed() {
        "; boot entropy from the firmware mixed in, not counted"
    } else {
        ""
    };
    crate::println!("[kernel] entropy: {}{mixed}", entropy::describe());
    // USB keyboards (ADR-0030).
    xhci::init();
}
