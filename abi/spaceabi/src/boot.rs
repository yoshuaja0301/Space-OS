//! Boot protocol between `spaceboot` (UEFI) and `spacekernel`.
//!
//! The bootloader leaves the machine in this state when it jumps to the kernel
//! entry point:
//!
//! * long mode, interrupts disabled, `EFER.NXE` and `CR0.WP` set;
//! * `CR3` points at page tables owned by the bootloader (`MemKind::KERNEL`) that
//!   map: the kernel image at its link address (top 2 GiB), all physical memory
//!   (plus the framebuffer) at [`PHYS_OFFSET`], and an identity map of physical
//!   memory that the kernel is expected to discard;
//! * `RSP` points at the top of a bootloader-provided kernel stack;
//! * `RDI` holds a pointer (in the [`PHYS_OFFSET`] window) to a [`BootInfo`].

/// Virtual base of the linear map of physical memory (PML4 slot 256).
pub const PHYS_OFFSET: u64 = 0xFFFF_8000_0000_0000;

/// Link address of the kernel image (top 2 GiB, `code-model=kernel`).
pub const KERNEL_VIRT_BASE: u64 = 0xFFFF_FFFF_8000_0000;

/// "SPACEBOO" – marks a valid [`BootInfo`].
pub const BOOT_INFO_MAGIC: u64 = 0x4F4F_4245_4341_5053;
pub const BOOT_INFO_VERSION: u32 = 1;

/// Size of the kernel stack allocated by the bootloader.
pub const BOOT_STACK_SIZE: usize = 64 * 1024;

/// Physical memory region kinds reported in [`BootInfo::memory_map`].
pub mod mem_kind {
    /// Free RAM the kernel may use.
    pub const USABLE: u32 = 1;
    /// Reserved by firmware/hardware. Never touch.
    pub const RESERVED: u32 = 2;
    /// Memory used by the bootloader (UEFI boot services, loader pools). The kernel may
    /// reclaim it once it no longer depends on anything in it (Space OS reclaims it
    /// immediately: everything the kernel needs is copied into `KERNEL` regions).
    pub const BOOTLOADER_RECLAIMABLE: u32 = 3;
    /// Kernel image, boot page tables, boot stack, boot info, memory map and initrd.
    pub const KERNEL: u32 = 4;
    /// ACPI tables that may be reclaimed after they have been parsed.
    pub const ACPI_RECLAIMABLE: u32 = 5;
    /// ACPI non-volatile storage.
    pub const ACPI_NVS: u32 = 6;
    /// Memory-mapped I/O.
    pub const MMIO: u32 = 7;
    /// Framebuffer (also covered by the linear map).
    pub const FRAMEBUFFER: u32 = 8;
    /// Memory reported bad by firmware.
    pub const BAD: u32 = 9;
}

/// One physical memory region. Regions are sorted by start address and do not overlap.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MemRegion {
    pub start: u64,
    pub len: u64,
    pub kind: u32,
    pub _pad: u32,
}

impl MemRegion {
    pub const fn end(&self) -> u64 {
        self.start + self.len
    }
}

pub mod fb_format {
    /// 32 bits per pixel, byte order R, G, B, X.
    pub const RGBX: u32 = 0;
    /// 32 bits per pixel, byte order B, G, R, X.
    pub const BGRX: u32 = 1;
    /// Some other layout; the kernel only clears such framebuffers.
    pub const OTHER: u32 = 2;
}

/// Linear framebuffer handed over from UEFI GOP.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct FramebufferInfo {
    /// 0 = no framebuffer available.
    pub present: u32,
    /// One of [`fb_format`].
    pub format: u32,
    pub width: u32,
    pub height: u32,
    /// Pixels per scanline (may exceed `width`).
    pub stride: u32,
    pub bytes_per_pixel: u32,
    pub phys_addr: u64,
    pub size: u64,
}

/// A byte range in physical memory described by (address, length).
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct PhysRange {
    pub phys: u64,
    pub len: u64,
}

/// Information passed from the bootloader to the kernel.
///
/// All pointers are physical addresses; the kernel reaches them through the
/// [`PHYS_OFFSET`] linear map.
#[repr(C)]
#[derive(Debug)]
pub struct BootInfo {
    pub magic: u64,
    pub version: u32,
    pub _pad: u32,
    /// Always [`PHYS_OFFSET`]; reported so the kernel can sanity-check the contract.
    pub phys_offset: u64,
    /// Highest physical address covered by the linear map (exclusive).
    pub phys_map_end: u64,
    /// Physical address of a [`MemRegion`] array and its length.
    pub memory_map: PhysRange,
    /// Number of valid entries in `memory_map` (the range may be larger).
    pub memory_map_entries: u64,
    /// Kernel ELF image as loaded (physical extent of all PT_LOAD segments).
    pub kernel_image: PhysRange,
    /// Initial RAM disk (ustar archive) or `len == 0` when absent.
    pub initrd: PhysRange,
    /// Kernel command line (UTF-8, not NUL-terminated) or `len == 0`.
    pub cmdline: PhysRange,
    /// Physical address of the boot PML4 built by the bootloader.
    pub boot_pml4: u64,
    /// Kernel stack the kernel is running on when it is entered.
    pub boot_stack: PhysRange,
    /// ACPI RSDP physical address or 0.
    pub rsdp: u64,
    pub framebuffer: FramebufferInfo,
}

impl BootInfo {
    pub fn is_valid(&self) -> bool {
        self.magic == BOOT_INFO_MAGIC && self.version == BOOT_INFO_VERSION
    }
}
