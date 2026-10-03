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
    /// Unsupported layout; the kernel leaves such framebuffers untouched.
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

impl FramebufferInfo {
    /// Validate the pixel layout and extent before addressing a linear framebuffer.
    pub const fn is_usable(&self) -> bool {
        if self.present == 0
            || !matches!(self.format, fb_format::RGBX | fb_format::BGRX)
            || self.bytes_per_pixel != 4
            || self.width == 0
            || self.height == 0
            || self.stride < self.width
            || self.phys_addr == 0
            || !self.phys_addr.is_multiple_of(4)
        {
            return false;
        }
        let row_bytes = self.stride as u64 * 4;
        let Some(required_bytes) = row_bytes.checked_mul(self.height as u64) else {
            return false;
        };
        let Some(phys_end) = self.phys_addr.checked_add(self.size) else {
            return false;
        };
        required_bytes <= self.size && PHYS_OFFSET.checked_add(phys_end).is_some()
    }
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

#[cfg(test)]
mod tests {
    use super::{FramebufferInfo, fb_format};

    const MODE: FramebufferInfo = FramebufferInfo {
        present: 1,
        format: fb_format::RGBX,
        width: 800,
        height: 600,
        stride: 832,
        bytes_per_pixel: 4,
        phys_addr: 0xE000_0000,
        size: 832 * 600 * 4,
    };

    #[test]
    fn framebuffer_is_usable_when_supported_modes_include_scanline_padding() {
        // Given: both supported pixel orders with a padded scanline.
        for format in [fb_format::RGBX, fb_format::BGRX] {
            let mode = FramebufferInfo { format, ..MODE };
            // When: the firmware metadata is checked.
            let usable = mode.is_usable();
            // Then: all reported rows fit inside the framebuffer.
            assert!(usable);
        }
    }

    #[test]
    fn framebuffer_is_rejected_when_metadata_cannot_back_safe_pixel_access() {
        // Given: invalid presence, format, geometry, alignment, or buffer extent.
        let modes = [
            FramebufferInfo { present: 0, ..MODE },
            FramebufferInfo { format: fb_format::OTHER, ..MODE },
            FramebufferInfo { format: u32::MAX, ..MODE },
            FramebufferInfo { bytes_per_pixel: 3, ..MODE },
            FramebufferInfo { width: 0, ..MODE },
            FramebufferInfo { height: 0, ..MODE },
            FramebufferInfo { stride: 0, ..MODE },
            FramebufferInfo { stride: MODE.width - 1, ..MODE },
            FramebufferInfo { phys_addr: 0, ..MODE },
            FramebufferInfo { phys_addr: MODE.phys_addr + 1, ..MODE },
            FramebufferInfo { size: MODE.size - 1, ..MODE },
            FramebufferInfo { phys_addr: u64::MAX - 3, ..MODE },
            FramebufferInfo { stride: u32::MAX, height: u32::MAX, size: u64::MAX, ..MODE },
        ];
        for mode in modes {
            // When: malformed firmware metadata is checked.
            let usable = mode.is_usable();
            // Then: it is rejected before pointer arithmetic or rendering.
            assert!(!usable, "accepted invalid framebuffer: {mode:?}");
        }
    }
}
