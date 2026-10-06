//! Boot protocol between `spaceboot` (UEFI) and `spacekernel` (ADR-0002, ADR-0032).
//!
//! The bootloader leaves an x86-64 machine in this state when it jumps to the
//! kernel entry point:
//!
//! * long mode, interrupts disabled, `EFER.NXE` and `CR0.WP` set;
//! * `CR3` points at page tables owned by the bootloader (`MemKind::KERNEL`) that
//!   map: the kernel image at its link address (top 2 GiB), all physical memory
//!   (plus the framebuffer) at [`PHYS_OFFSET`], and an identity map of physical
//!   memory that the kernel is expected to discard;
//! * `RSP` points at the top of a bootloader-provided kernel stack;
//! * `RDI` holds a pointer (in the [`PHYS_OFFSET`] window) to a [`BootInfo`].
//!
//! And an AArch64 machine (ADR-0028):
//!
//! * EL1, MMU and caches on, `DAIF` all masked;
//! * `TTBR1_EL1` points at bootloader tables (`MemKind::KERNEL`) mapping the kernel
//!   image at its link address, physical memory at [`PHYS_OFFSET`], and the console
//!   UART, if any, at [`EARLY_UART_VIRT`]; `TTBR0_EL1` still holds the firmware's
//!   identity map, which the kernel replaces before it reuses firmware memory;
//! * `MAIR_EL1` holds the attributes of [`mair`] at those indices;
//! * `SP` points at the top of a bootloader-provided kernel stack;
//! * `X0` holds a pointer (in the [`PHYS_OFFSET`] window) to a [`BootInfo`].
//!
//! ## The hand-over is checked before it is believed (PRD v0.2 §7.2, B02)
//!
//! The kernel reads nothing a [`BootInfo`] says before [`BootInfo::validate_header`]
//! has passed, and nothing it points to before [`BootInfo::validate`] has: the
//! identity (magic, version, size, flags), every range against the linear map, the
//! memory map itself (sorted, disjoint, kinds it knows), the reservations (every
//! range handed over lies inside the reservation of what it holds, and every
//! reservation inside memory the map gives the kernel), the parameters, the entropy
//! and the boot slot. A refusal names the field, its value and the rule it breaks.
//! The kernel also measures the boot image against the SHA-256 the bootloader took
//! when it loaded it, and reads the command line only as UTF-8 ([`validate_cmdline`]).
//!
//! Every address in the structure is **physical**; the kernel reaches it through the
//! linear map at [`PHYS_OFFSET`]. Each field below says its alignment, how long it
//! lives and who owns it. Nothing in a `BootInfo` is a secret: the entropy is not a
//! key, its quality is not assumed, and the kernel wipes it once it has taken it.

use core::fmt;
use core::mem::size_of;

/// Virtual base of the linear map of physical memory (PML4 slot 256).
pub const PHYS_OFFSET: u64 = 0xFFFF_8000_0000_0000;

/// The most physical address space the linear map can hold: from [`PHYS_OFFSET`] to
/// the kernel heap, 16 TiB further on (PML4 slots 256..288).
pub const LINEAR_MAP_MAX: u64 = 1 << 44;

/// Link address of the kernel image (top 2 GiB, `code-model=kernel`).
pub const KERNEL_VIRT_BASE: u64 = 0xFFFF_FFFF_8000_0000;

/// AArch64: where the bootloader maps the console UART's register page (device
/// memory), so the kernel can print before it maps devices itself (slot 384).
pub const EARLY_UART_VIRT: u64 = 0xFFFF_C000_0000_0000;

/// AArch64 `MAIR_EL1` attribute indices both sides use. They are the ones UEFI
/// firmware (EDK2 on AArch64) programs, so the bootloader can map with them while
/// still running on the firmware's tables, and checks that it does.
pub mod mair {
    /// Device-nGnRnE.
    pub const DEVICE: u64 = 0;
    /// Normal memory, inner and outer write-back.
    pub const NORMAL: u64 = 3;
    pub const DEVICE_ATTR: u8 = 0x00;
    pub const NORMAL_ATTR: u8 = 0xFF;
}

/// "SPACEBOO" – marks a valid [`BootInfo`].
pub const BOOT_INFO_MAGIC: u64 = 0x4F4F_4245_4341_5053;
/// Version 3 (ADR-0032): size, flags, memory-map format, the boot image's digest,
/// reservations, entropy and the boot slot.
pub const BOOT_INFO_VERSION: u32 = 3;

/// Layout of the memory-map entries: [`MemRegion`] as defined here.
pub const MEMORY_MAP_FORMAT: u32 = 1;

/// The longest command line the kernel takes, in bytes.
pub const CMDLINE_MAX: u64 = 4096;

/// Room for reservations in a [`BootInfo`].
pub const MAX_RESERVATIONS: usize = 16;

/// Room for boot entropy in a [`BootInfo`].
pub const ENTROPY_MAX: usize = 64;

/// What kind of UART [`BootInfo::uart`] is (the ACPI SPCR interface types).
pub mod uart_kind {
    pub const NONE: u32 = 0;
    /// ARM PL011 (SPCR interface type 3).
    pub const PL011: u32 = 3;
}

/// Size of the kernel stack allocated by the bootloader.
pub const BOOT_STACK_SIZE: usize = 64 * 1024;

const PAGE: u64 = 4096;
const HUGE: u64 = 2 * 1024 * 1024;

/// Which optional parts of a [`BootInfo`] are there. Each bit has to agree with its
/// field: a part the flags leave out is all zero, and one they name is filled in.
pub mod flags {
    /// `framebuffer` describes a linear framebuffer.
    pub const FRAMEBUFFER: u64 = 1 << 0;
    /// `rsdp` is the ACPI RSDP.
    pub const ACPI: u64 = 1 << 1;
    /// `initrd` holds the boot image and `initrd_sha256` its digest.
    pub const BOOT_IMAGE: u64 = 1 << 2;
    /// `cmdline` holds a command line.
    pub const CMDLINE: u64 = 1 << 3;
    /// `uart` is a console UART mapped at `EARLY_UART_VIRT`.
    pub const UART: u64 = 1 << 4;
    /// `boot_time` is the firmware clock's time.
    pub const BOOT_TIME: u64 = 1 << 5;
    /// `entropy` holds bytes from a firmware source.
    pub const ENTROPY: u64 = 1 << 6;
    /// Every bit this version defines.
    pub const ALL: u64 = (1 << 7) - 1;
}

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
    /// Kernel image, boot page tables, boot stack, boot info, memory map and initrd:
    /// what [`BootInfo::reservations`] lists, never handed out as free memory.
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

    /// RAM the kernel reaches through the linear map, so it must lie below
    /// `BootInfo::phys_map_end`.
    pub const fn is_ram(kind: u32) -> bool {
        matches!(kind, USABLE | BOOTLOADER_RECLAIMABLE | KERNEL | ACPI_RECLAIMABLE | ACPI_NVS)
    }
}

/// One physical memory region. Regions are sorted by start address and do not
/// overlap; both ends are page-aligned.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MemRegion {
    pub start: u64,
    pub len: u64,
    pub kind: u32,
    pub _pad: u32,
}

impl MemRegion {
    /// Exclusive end. Only for a region of a validated map: the check is what
    /// guarantees it does not overflow.
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

/// Linear framebuffer handed over from UEFI GOP. Physical, page-aligned; device
/// memory owned by the display, mapped by the linear map for the kernel's lifetime.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FramebufferInfo {
    /// 0 = no framebuffer available (and every other field 0).
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
    /// The rules a framebuffer description keeps (the bootloader drops one that
    /// does not, rather than hand it over). `phys_map_end` bounds where it may be.
    pub fn check(&self, phys_map_end: u64) -> Result<(), Invalid> {
        if self.present == 0 {
            return need(
                *self == FramebufferInfo::default(),
                "framebuffer",
                self.phys_addr,
                "absent, yet described",
            );
        }
        need(self.present == 1, "framebuffer.present", self.present.into(), "neither 0 nor 1")?;
        need(
            self.format <= fb_format::OTHER,
            "framebuffer.format",
            self.format.into(),
            "not a format this version defines",
        )?;
        need(self.bytes_per_pixel == 4, "framebuffer.bytes_per_pixel", self.bytes_per_pixel.into(), "not 4")?;
        need(self.width != 0 && self.height != 0, "framebuffer.width", self.width.into(), "an empty screen")?;
        need(
            self.stride >= self.width,
            "framebuffer.stride",
            self.stride.into(),
            "shorter than a line of the screen",
        )?;
        let bytes = u64::from(self.stride).checked_mul(u64::from(self.height)).and_then(|p| p.checked_mul(4));
        need(
            bytes.is_some_and(|b| b <= self.size),
            "framebuffer.size",
            self.size,
            "smaller than stride x height x 4",
        )?;
        need(
            self.phys_addr.is_multiple_of(PAGE),
            "framebuffer.phys_addr",
            self.phys_addr,
            "not page-aligned",
        )?;
        let end = self.phys_addr.checked_add(self.size);
        need(
            end.is_some_and(|e| e <= phys_map_end),
            "framebuffer.phys_addr",
            self.phys_addr,
            "outside the linear map",
        )
    }
}

/// A byte range in physical memory described by (address, length).
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PhysRange {
    pub phys: u64,
    pub len: u64,
}

impl PhysRange {
    /// Exclusive end, or `None` when it would not fit in 64 bits.
    pub const fn end(&self) -> Option<u64> {
        self.phys.checked_add(self.len)
    }

    /// True when every byte of `self` is in `outer` (an empty `self` must still
    /// start inside it).
    pub fn within(&self, outer: &PhysRange) -> bool {
        match (self.end(), outer.end()) {
            (Some(end), Some(outer_end)) => {
                self.phys >= outer.phys && end <= outer_end && self.phys < outer_end
            }
            _ => false,
        }
    }

    /// True when the two share a byte. A range that overflows overlaps everything.
    pub fn overlaps(&self, other: &PhysRange) -> bool {
        match (self.end(), other.end()) {
            (Some(a), Some(b)) => self.phys < b && other.phys < a,
            _ => true,
        }
    }
}

/// Who a reserved range belongs to, which also says how long it lives.
pub mod owner {
    /// The kernel image as loaded: for the kernel's lifetime.
    pub const KERNEL_IMAGE: u32 = 1;
    /// The boot image (`initrd`): the kernel loads programs from it for its lifetime.
    pub const BOOT_IMAGE: u32 = 2;
    /// The command line: the kernel keeps referring to it.
    pub const CMDLINE: u32 = 3;
    /// The stack the kernel is entered on, until it moves to a guarded one.
    pub const BOOT_STACK: u32 = 4;
    /// The page holding this [`super::BootInfo`].
    pub const BOOT_INFO: u32 = 5;
    /// The memory-map array, until the frame allocator has read it.
    pub const MEMORY_MAP: u32 = 6;
    /// The page tables the kernel is entered on: their upper half stays in use as
    /// the kernel's own.
    pub const PAGE_TABLES: u32 = 7;

    pub fn name(owner: u32) -> &'static str {
        match owner {
            KERNEL_IMAGE => "kernel image",
            BOOT_IMAGE => "boot image",
            CMDLINE => "command line",
            BOOT_STACK => "boot stack",
            BOOT_INFO => "boot info",
            MEMORY_MAP => "memory map",
            PAGE_TABLES => "page tables",
            _ => "?",
        }
    }
}

/// A range the kernel must not reclaim yet, and who it belongs to. Physical, whole
/// pages, inside a [`mem_kind::KERNEL`] region of the memory map.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Reservation {
    pub range: PhysRange,
    /// One of [`owner`].
    pub owner: u32,
    pub _pad: u32,
}

/// Where [`BootInfo::entropy`] came from.
pub mod entropy_source {
    pub const NONE: u32 = 0;
    /// The firmware's `EFI_RNG_PROTOCOL`, default algorithm.
    pub const UEFI_RNG: u32 = 1;

    pub fn name(source: u32) -> &'static str {
        match source {
            NONE => "none",
            UEFI_RNG => "the firmware's RNG protocol",
            _ => "?",
        }
    }
}

/// Random bytes the bootloader could get, and where from. Their quality is not
/// assumed: the kernel mixes them in, and never counts them as the source a key
/// needs.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Entropy {
    /// One of [`entropy_source`].
    pub source: u32,
    /// Valid bytes at the start of `bytes`; the rest are 0.
    pub len: u32,
    pub bytes: [u8; ENTROPY_MAX],
}

impl Entropy {
    pub const NONE: Entropy = Entropy { source: entropy_source::NONE, len: 0, bytes: [0; ENTROPY_MAX] };
}

/// Which way this boot went.
pub mod slot {
    /// The normal system.
    pub const NORMAL: u32 = 0;
    /// The recovery console (ADR-0027).
    pub const RECOVERY: u32 = 1;
}

/// Why a boot went to recovery.
pub mod recovery_reason {
    pub const NONE: u32 = 0;
    /// The operator asked (the recovery key).
    pub const OPERATOR: u32 = 1;
    /// [`super::FAILED_BOOTS`] boots in a row never came up.
    pub const FAILED_BOOTS: u32 = 2;

    pub fn name(reason: u32) -> &'static str {
        match reason {
            NONE => "none",
            OPERATOR => "operator",
            FAILED_BOOTS => "failed-boots",
            _ => "?",
        }
    }
}

/// Where the state recovery decides on is kept.
pub mod recovery_state {
    /// Nowhere: boots are not counted (no data volume, or `boot_count` is off).
    pub const NONE: u32 = 0;
    /// The count in `/spaceos/var/boots.txt` on the volume labelled `SPACEDATA`.
    pub const DATA_VOLUME: u32 = 1;
}

/// Boots in a row that never came up before the bootloader starts recovery itself.
pub const FAILED_BOOTS: u32 = 3;

/// [`BootSlot::attempts`] when nothing counts boots.
pub const NOT_COUNTED: u32 = u32::MAX;

/// The slot this boot runs, how many boots before it failed, and where that count
/// is kept.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct BootSlot {
    /// One of [`slot`].
    pub slot: u32,
    /// One of [`recovery_reason`]; `NONE` exactly when the slot is the normal one.
    pub reason: u32,
    /// Boots in a row before this one that never came up, or [`NOT_COUNTED`].
    pub attempts: u32,
    /// One of [`recovery_state`]; `NONE` exactly when `attempts` is `NOT_COUNTED`.
    pub state: u32,
}

/// Information passed from the bootloader to the kernel.
///
/// Physical, 8-byte aligned, in one page of its own (reservation
/// [`owner::BOOT_INFO`]). Owned by the kernel from the hand-over on.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct BootInfo {
    pub magic: u64,
    pub version: u32,
    /// `size_of::<BootInfo>()` of the version that wrote it.
    pub size: u32,
    /// [`flags`]: which optional parts are present.
    pub flags: u64,
    /// Always [`PHYS_OFFSET`]; reported so the kernel can sanity-check the contract.
    pub phys_offset: u64,
    /// Highest physical address covered by the linear map (exclusive): a non-zero
    /// multiple of 2 MiB, at most [`LINEAR_MAP_MAX`].
    pub phys_map_end: u64,
    /// The memory map: `memory_map_entries` [`MemRegion`]s at the start of this
    /// range. Physical, whole pages. Reservation [`owner::MEMORY_MAP`].
    pub memory_map: PhysRange,
    /// Number of valid entries in `memory_map` (the range may be larger).
    pub memory_map_entries: u64,
    /// `size_of::<MemRegion>()`.
    pub memory_map_entry_size: u32,
    /// [`MEMORY_MAP_FORMAT`].
    pub memory_map_format: u32,
    /// Kernel ELF image as loaded (physical extent of all PT_LOAD segments). Whole
    /// pages. Reservation [`owner::KERNEL_IMAGE`].
    pub kernel_image: PhysRange,
    /// Initial RAM disk (the boot image, a ustar archive) or `len == 0` when absent.
    /// Physical, starts on a page. Reservation [`owner::BOOT_IMAGE`].
    pub initrd: PhysRange,
    /// SHA-256 of the boot image as the bootloader loaded it (zero without one).
    pub initrd_sha256: [u8; 32],
    /// Kernel command line (UTF-8, not NUL-terminated, at most [`CMDLINE_MAX`]
    /// bytes) or `len == 0`. Physical, starts on a page. Reservation
    /// [`owner::CMDLINE`].
    pub cmdline: PhysRange,
    /// Physical address of the root of the bootloader's kernel tables: the PML4 on
    /// x86-64, the `TTBR1_EL1` table on AArch64. Page-aligned. Reservation
    /// [`owner::PAGE_TABLES`].
    pub boot_pml4: u64,
    /// Kernel stack the kernel is running on when it is entered:
    /// [`BOOT_STACK_SIZE`], page-aligned. Reservation [`owner::BOOT_STACK`].
    pub boot_stack: PhysRange,
    /// ACPI RSDP physical address or 0. Firmware memory; the kernel only reads it.
    pub rsdp: u64,
    pub framebuffer: FramebufferInfo,
    /// Seconds since the Unix epoch when the bootloader ran (UEFI `GetTime`, taken
    /// as UTC), or 0 when the firmware did not say.
    pub boot_time: u64,
    /// Console UART registers (physical; AArch64 only, from ACPI SPCR) and its
    /// [`uart_kind`]; mapped at [`EARLY_UART_VIRT`].
    pub uart: u64,
    pub uart_kind: u32,
    /// Valid entries at the start of `reservations`; the rest are zero.
    pub reservation_count: u32,
    /// Ranges the kernel must not reclaim yet, and their owners.
    pub reservations: [Reservation; MAX_RESERVATIONS],
    pub entropy: Entropy,
    pub boot_slot: BootSlot,
}

// One page holds it (reservation `owner::BOOT_INFO`).
const _: () = assert!(size_of::<BootInfo>() <= PAGE as usize);

/// Why the kernel refuses a [`BootInfo`]: the field, its value and the rule it
/// breaks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Invalid {
    pub field: &'static str,
    /// Which entry of a list (the memory map, the reservations), if the field is one.
    pub index: Option<u32>,
    pub value: u64,
    pub rule: &'static str,
    /// The one value the field may have, where there is exactly one.
    pub expected: Option<u64>,
}

impl fmt::Display for Invalid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.index {
            Some(i) => write!(f, "{} (entry {i}) = {:#x}: {}", self.field, self.value, self.rule)?,
            None => write!(f, "{} = {:#x}: {}", self.field, self.value, self.rule)?,
        }
        if let Some(e) = self.expected {
            write!(f, " (expected {e:#x})")?;
        }
        Ok(())
    }
}

fn need(ok: bool, field: &'static str, value: u64, rule: &'static str) -> Result<(), Invalid> {
    if ok { Ok(()) } else { Err(Invalid { field, index: None, value, rule, expected: None }) }
}

fn need_eq(value: u64, expected: u64, field: &'static str, rule: &'static str) -> Result<(), Invalid> {
    if value == expected {
        Ok(())
    } else {
        Err(Invalid { field, index: None, value, rule, expected: Some(expected) })
    }
}

fn at(i: usize, field: &'static str, value: u64, rule: &'static str) -> Invalid {
    Invalid { field, index: Some(i as u32), value, rule, expected: None }
}

/// The command line as text: UTF-8 and no longer than [`CMDLINE_MAX`].
pub fn validate_cmdline(bytes: &[u8]) -> Result<&str, Invalid> {
    need(bytes.len() as u64 <= CMDLINE_MAX, "cmdline.len", bytes.len() as u64, "longer than 4096 bytes")?;
    core::str::from_utf8(bytes).map_err(|e| Invalid {
        field: "cmdline",
        index: None,
        value: e.valid_up_to() as u64,
        rule: "not UTF-8 from this byte on",
        expected: None,
    })
}

impl BootInfo {
    /// Everything that can be checked without reading memory outside the
    /// structure. Until this passes, no other field may be believed.
    pub fn validate_header(&self) -> Result<(), Invalid> {
        // Identity first: nothing else means anything if these are wrong.
        need_eq(self.magic, BOOT_INFO_MAGIC, "magic", "not a Space OS BootInfo")?;
        need_eq(
            self.version.into(),
            BOOT_INFO_VERSION.into(),
            "version",
            "a BootInfo version this kernel does not read",
        )?;
        need_eq(
            self.size.into(),
            size_of::<BootInfo>() as u64,
            "size",
            "not the size of this version's BootInfo",
        )?;
        need(self.flags & !flags::ALL == 0, "flags", self.flags, "bits this version does not define")?;
        // The linear map everything else is reached through.
        need_eq(self.phys_offset, PHYS_OFFSET, "phys_offset", "not where the linear map is")?;
        need(
            self.phys_map_end != 0 && self.phys_map_end.is_multiple_of(HUGE),
            "phys_map_end",
            self.phys_map_end,
            "not a non-zero multiple of 2 MiB",
        )?;
        need(
            self.phys_map_end <= LINEAR_MAP_MAX,
            "phys_map_end",
            self.phys_map_end,
            "beyond what the linear map can hold",
        )?;
        // The memory map: where it is and what its entries are.
        self.whole_pages("memory_map", &self.memory_map)?;
        need_eq(
            self.memory_map_entry_size.into(),
            size_of::<MemRegion>() as u64,
            "memory_map_entry_size",
            "not the size of a memory-map entry",
        )?;
        need_eq(
            self.memory_map_format.into(),
            MEMORY_MAP_FORMAT.into(),
            "memory_map_format",
            "a format this kernel does not read",
        )?;
        let bytes = self.memory_map_entries.checked_mul(size_of::<MemRegion>() as u64);
        need(
            self.memory_map_entries != 0 && bytes.is_some_and(|b| b <= self.memory_map.len),
            "memory_map_entries",
            self.memory_map_entries,
            "none, or more than the array holds",
        )?;
        // What was loaded, and what the kernel runs on.
        self.whole_pages("kernel_image", &self.kernel_image)?;
        if self.initrd.len != 0 {
            self.in_linear_map("initrd", &self.initrd)?;
        }
        need(
            self.initrd.len != 0 || (self.initrd == PhysRange::default() && self.initrd_sha256 == [0; 32]),
            "initrd",
            self.initrd.phys,
            "absent, yet described",
        )?;
        need(self.cmdline.len <= CMDLINE_MAX, "cmdline.len", self.cmdline.len, "longer than 4096 bytes")?;
        if self.cmdline.len != 0 {
            self.in_linear_map("cmdline", &self.cmdline)?;
        }
        need(
            self.cmdline.len != 0 || self.cmdline.phys == 0,
            "cmdline",
            self.cmdline.phys,
            "absent, yet described",
        )?;
        need(
            self.boot_pml4.is_multiple_of(PAGE) && self.boot_pml4 < self.phys_map_end,
            "boot_pml4",
            self.boot_pml4,
            "not a page inside the linear map",
        )?;
        need_eq(self.boot_stack.len, BOOT_STACK_SIZE as u64, "boot_stack.len", "not the boot stack's size")?;
        self.whole_pages("boot_stack", &self.boot_stack)?;
        // Platform.
        self.framebuffer.check(self.phys_map_end)?;
        need(
            matches!(self.uart_kind, uart_kind::NONE | uart_kind::PL011),
            "uart_kind",
            self.uart_kind.into(),
            "not a UART this version defines",
        )?;
        need(
            (self.uart_kind == uart_kind::NONE) == (self.uart == 0),
            "uart",
            self.uart,
            "an address without a kind, or a kind without an address",
        )?;
        // Reservations: how many (their ranges need the memory map, see `validate`).
        need(
            self.reservation_count as usize <= MAX_RESERVATIONS,
            "reservation_count",
            self.reservation_count.into(),
            "more than there is room for",
        )?;
        let unused = &self.reservations[(self.reservation_count as usize).min(MAX_RESERVATIONS)..];
        need(
            unused.iter().all(|r| *r == Reservation::default()),
            "reservations",
            self.reservation_count.into(),
            "entries past the count are not zero",
        )?;
        self.check_entropy()?;
        self.check_slot()?;
        self.check_flags()
    }

    /// The whole hand-over: [`validate_header`](Self::validate_header), then the
    /// memory map the header points to (`map`, its `memory_map_entries` entries) and
    /// the reservations against it. `self_phys` is where this structure is.
    pub fn validate(&self, self_phys: u64, map: &[MemRegion]) -> Result<(), Invalid> {
        self.validate_header()?;
        need(
            map.len() as u64 == self.memory_map_entries,
            "memory_map_entries",
            self.memory_map_entries,
            "not the entries the kernel was given",
        )?;
        self.check_memory_map(map)?;
        self.check_reservations(self_phys, map)
    }

    /// A console UART the kernel may print to before anything is validated: only
    /// from a structure that is, by its magic and version, one this kernel reads.
    /// A wrong address still lands in the page the bootloader mapped.
    pub fn console_uart(&self) -> Option<u64> {
        let ours = self.magic == BOOT_INFO_MAGIC && self.version == BOOT_INFO_VERSION;
        (ours && self.uart_kind == uart_kind::PL011 && self.uart != 0).then_some(self.uart)
    }

    /// The valid reservations.
    pub fn reserved(&self) -> &[Reservation] {
        &self.reservations[..(self.reservation_count as usize).min(MAX_RESERVATIONS)]
    }

    /// A non-empty range of whole pages inside the linear map.
    fn whole_pages(&self, field: &'static str, r: &PhysRange) -> Result<(), Invalid> {
        need(r.len != 0 && r.len.is_multiple_of(PAGE), field, r.len, "not a whole number of pages")?;
        self.in_linear_map(field, r)
    }

    /// A non-empty range that starts on a page and ends inside the linear map.
    fn in_linear_map(&self, field: &'static str, r: &PhysRange) -> Result<(), Invalid> {
        need(r.phys.is_multiple_of(PAGE), field, r.phys, "does not start on a page")?;
        need(
            r.len != 0 && r.end().is_some_and(|e| e <= self.phys_map_end),
            field,
            r.phys,
            "outside the linear map",
        )
    }

    fn check_entropy(&self) -> Result<(), Invalid> {
        let e = &self.entropy;
        need(
            matches!(e.source, entropy_source::NONE | entropy_source::UEFI_RNG),
            "entropy.source",
            e.source.into(),
            "not a source this version defines",
        )?;
        need(
            e.len as usize <= ENTROPY_MAX,
            "entropy.len",
            e.len.into(),
            "more than the 64 bytes there is room for",
        )?;
        need(
            (e.source == entropy_source::NONE) == (e.len == 0),
            "entropy.len",
            e.len.into(),
            "bytes without a source, or a source without bytes",
        )?;
        need(
            e.bytes[(e.len as usize).min(ENTROPY_MAX)..].iter().all(|&b| b == 0),
            "entropy.bytes",
            e.len.into(),
            "bytes past the length are not zero",
        )
    }

    fn check_slot(&self) -> Result<(), Invalid> {
        let s = &self.boot_slot;
        need(
            matches!(s.slot, slot::NORMAL | slot::RECOVERY),
            "boot_slot.slot",
            s.slot.into(),
            "neither the normal nor the recovery slot",
        )?;
        need(
            matches!(
                s.reason,
                recovery_reason::NONE | recovery_reason::OPERATOR | recovery_reason::FAILED_BOOTS
            ),
            "boot_slot.reason",
            s.reason.into(),
            "not a reason this version defines",
        )?;
        need(
            (s.slot == slot::RECOVERY) == (s.reason != recovery_reason::NONE),
            "boot_slot.reason",
            s.reason.into(),
            "a recovery boot needs a reason, and only a recovery boot has one",
        )?;
        need(
            matches!(s.state, recovery_state::NONE | recovery_state::DATA_VOLUME),
            "boot_slot.state",
            s.state.into(),
            "not a place this version defines",
        )?;
        need(
            (s.state == recovery_state::NONE) == (s.attempts == NOT_COUNTED),
            "boot_slot.attempts",
            s.attempts.into(),
            "a count with nowhere it is kept, or a place without a count",
        )?;
        need(
            s.reason != recovery_reason::FAILED_BOOTS
                || (s.attempts != NOT_COUNTED && s.attempts >= FAILED_BOOTS),
            "boot_slot.attempts",
            s.attempts.into(),
            "recovery for failed boots without that many failed boots",
        )
    }

    fn check_flags(&self) -> Result<(), Invalid> {
        let parts: [(u64, bool, &'static str); 7] = [
            (
                flags::FRAMEBUFFER,
                self.framebuffer.present != 0,
                "the framebuffer bit disagrees with the framebuffer",
            ),
            (flags::ACPI, self.rsdp != 0, "the ACPI bit disagrees with rsdp"),
            (flags::BOOT_IMAGE, self.initrd.len != 0, "the boot-image bit disagrees with initrd"),
            (flags::CMDLINE, self.cmdline.len != 0, "the command-line bit disagrees with cmdline"),
            (flags::UART, self.uart_kind != uart_kind::NONE, "the UART bit disagrees with uart_kind"),
            (flags::BOOT_TIME, self.boot_time != 0, "the boot-time bit disagrees with boot_time"),
            (
                flags::ENTROPY,
                self.entropy.source != entropy_source::NONE,
                "the entropy bit disagrees with entropy",
            ),
        ];
        for (bit, there, rule) in parts {
            need((self.flags & bit != 0) == there, "flags", self.flags, rule)?;
        }
        Ok(())
    }

    fn check_memory_map(&self, map: &[MemRegion]) -> Result<(), Invalid> {
        let mut prev_end = 0u64;
        let mut usable = false;
        for (i, r) in map.iter().enumerate() {
            if !(mem_kind::USABLE..=mem_kind::BAD).contains(&r.kind) {
                return Err(at(i, "memory map kind", r.kind.into(), "not a kind this version defines"));
            }
            if r.len == 0 || !r.start.is_multiple_of(PAGE) || !r.len.is_multiple_of(PAGE) {
                return Err(at(i, "memory map start", r.start, "empty, or not whole pages"));
            }
            let Some(end) = r.start.checked_add(r.len) else {
                return Err(at(i, "memory map length", r.len, "runs past the end of the address space"));
            };
            if r.start < prev_end {
                return Err(at(
                    i,
                    "memory map start",
                    r.start,
                    "out of order, or overlapping the entry before it",
                ));
            }
            if mem_kind::is_ram(r.kind) && end > self.phys_map_end {
                return Err(at(i, "memory map start", r.start, "RAM beyond the linear map"));
            }
            usable |= r.kind == mem_kind::USABLE;
            prev_end = end;
        }
        need(usable, "memory_map", self.memory_map.phys, "no usable RAM at all")
    }

    fn check_reservations(&self, self_phys: u64, map: &[MemRegion]) -> Result<(), Invalid> {
        let list = self.reserved();
        let mut seen = 0u32;
        for (i, r) in list.iter().enumerate() {
            if !(owner::KERNEL_IMAGE..=owner::PAGE_TABLES).contains(&r.owner) {
                return Err(at(i, "reservation owner", r.owner.into(), "not an owner this version defines"));
            }
            if seen & (1 << r.owner) != 0 {
                return Err(at(i, "reservation owner", r.owner.into(), "reserved twice"));
            }
            seen |= 1 << r.owner;
            if r.range.len == 0
                || !r.range.phys.is_multiple_of(PAGE)
                || !r.range.len.is_multiple_of(PAGE)
                || r._pad != 0
            {
                return Err(at(i, "reservation", r.range.phys, "empty, or not whole pages"));
            }
            // In memory the kernel never hands out as free.
            let kernel_memory = map.iter().any(|m| {
                m.kind == mem_kind::KERNEL && r.range.within(&PhysRange { phys: m.start, len: m.len })
            });
            if !kernel_memory {
                return Err(at(i, "reservation", r.range.phys, "not inside memory the map gives the kernel"));
            }
            if list[..i].iter().any(|o| o.range.overlaps(&r.range)) {
                return Err(at(i, "reservation", r.range.phys, "overlaps another reservation"));
            }
        }
        // Every range handed over lies in the reservation of what it is.
        let covered = |who: u32, r: &PhysRange| list.iter().any(|x| x.owner == who && r.within(&x.range));
        let this = PhysRange { phys: self_phys, len: size_of::<BootInfo>() as u64 };
        let page_tables = PhysRange { phys: self.boot_pml4, len: PAGE };
        need(
            covered(owner::KERNEL_IMAGE, &self.kernel_image),
            "kernel_image",
            self.kernel_image.phys,
            "not inside the kernel image's reservation",
        )?;
        need(
            covered(owner::BOOT_STACK, &self.boot_stack),
            "boot_stack",
            self.boot_stack.phys,
            "not inside the boot stack's reservation",
        )?;
        need(
            covered(owner::MEMORY_MAP, &self.memory_map),
            "memory_map",
            self.memory_map.phys,
            "not inside the memory map's reservation",
        )?;
        need(covered(owner::BOOT_INFO, &this), "BootInfo", self_phys, "not inside its own reservation")?;
        need(
            covered(owner::PAGE_TABLES, &page_tables),
            "boot_pml4",
            self.boot_pml4,
            "not inside the page tables' reservation",
        )?;
        let image_reserved = seen & (1 << owner::BOOT_IMAGE) != 0;
        if self.initrd.len != 0 {
            need(
                covered(owner::BOOT_IMAGE, &self.initrd),
                "initrd",
                self.initrd.phys,
                "not inside the boot image's reservation",
            )?;
        } else {
            need(
                !image_reserved,
                "reservations",
                owner::BOOT_IMAGE.into(),
                "a boot image reserved, and none handed over",
            )?;
        }
        let cmdline_reserved = seen & (1 << owner::CMDLINE) != 0;
        if self.cmdline.len != 0 {
            need(
                covered(owner::CMDLINE, &self.cmdline),
                "cmdline",
                self.cmdline.phys,
                "not inside the command line's reservation",
            )
        } else {
            need(
                !cmdline_reserved,
                "reservations",
                owner::CMDLINE.into(),
                "a command line reserved, and none handed over",
            )
        }
    }
}

#[cfg(test)]
mod tests {
    extern crate std;

    use std::vec::Vec;

    use super::*;

    const MIB: u64 = 1024 * 1024;

    /// A machine like the lab's: 2 GiB of RAM, a framebuffer, and the bootloader's
    /// allocations in one `KERNEL` region at 16 MiB.
    struct Fixture {
        bi: BootInfo,
        map: Vec<MemRegion>,
        self_phys: u64,
    }

    fn region(start: u64, len: u64, kind: u32) -> MemRegion {
        MemRegion { start, len, kind, _pad: 0 }
    }

    fn reservation(phys: u64, len: u64, owner: u32) -> Reservation {
        Reservation { range: PhysRange { phys, len }, owner, _pad: 0 }
    }

    fn fixture() -> Fixture {
        let kernel = 16 * MIB;
        let map = std::vec![
            region(0, 0xA0000, mem_kind::USABLE),
            region(0x100000, 15 * MIB, mem_kind::USABLE),
            region(kernel, 16 * MIB, mem_kind::KERNEL),
            region(32 * MIB, 2015 * MIB, mem_kind::USABLE),
            region(0x7FF0_0000, MIB, mem_kind::ACPI_NVS),
            region(0x8000_0000, 16 * MIB, mem_kind::FRAMEBUFFER),
            region(0xFEC0_0000, 0x1000, mem_kind::MMIO),
        ];
        let image = PhysRange { phys: kernel, len: 4 * MIB };
        let initrd = PhysRange { phys: kernel + 4 * MIB, len: 4_038_144 };
        let cmdline = PhysRange { phys: kernel + 8 * MIB, len: 13 };
        let stack = PhysRange { phys: kernel + 8 * MIB + PAGE, len: BOOT_STACK_SIZE as u64 };
        let self_phys = stack.phys + stack.len;
        let memory_map = PhysRange { phys: self_phys + PAGE, len: 16 * PAGE };
        let tables = PhysRange { phys: memory_map.phys + memory_map.len, len: 40 * PAGE };
        let mut reservations = [Reservation::default(); MAX_RESERVATIONS];
        let list = [
            reservation(image.phys, image.len, owner::KERNEL_IMAGE),
            reservation(initrd.phys, 4_038_144u64.div_ceil(PAGE) * PAGE, owner::BOOT_IMAGE),
            reservation(cmdline.phys, PAGE, owner::CMDLINE),
            reservation(stack.phys, stack.len, owner::BOOT_STACK),
            reservation(self_phys, PAGE, owner::BOOT_INFO),
            reservation(memory_map.phys, memory_map.len, owner::MEMORY_MAP),
            reservation(tables.phys, tables.len, owner::PAGE_TABLES),
        ];
        reservations[..list.len()].copy_from_slice(&list);
        let mut entropy = Entropy::NONE;
        entropy.source = entropy_source::UEFI_RNG;
        entropy.len = 32;
        entropy.bytes[..32].fill(0xA5);
        let bi = BootInfo {
            magic: BOOT_INFO_MAGIC,
            version: BOOT_INFO_VERSION,
            size: size_of::<BootInfo>() as u32,
            flags: flags::ALL & !flags::UART,
            phys_offset: PHYS_OFFSET,
            phys_map_end: 0x8100_0000,
            memory_map,
            memory_map_entries: map.len() as u64,
            memory_map_entry_size: size_of::<MemRegion>() as u32,
            memory_map_format: MEMORY_MAP_FORMAT,
            kernel_image: image,
            initrd,
            initrd_sha256: [7; 32],
            cmdline,
            boot_pml4: tables.phys,
            boot_stack: stack,
            rsdp: 0x7FF7_E014,
            framebuffer: FramebufferInfo {
                present: 1,
                format: fb_format::BGRX,
                width: 1280,
                height: 800,
                stride: 1280,
                bytes_per_pixel: 4,
                phys_addr: 0x8000_0000,
                size: 16 * MIB,
            },
            boot_time: 1_790_000_000,
            uart: 0,
            uart_kind: uart_kind::NONE,
            reservation_count: list.len() as u32,
            reservations,
            entropy,
            boot_slot: BootSlot {
                slot: slot::NORMAL,
                reason: recovery_reason::NONE,
                attempts: 0,
                state: recovery_state::DATA_VOLUME,
            },
        };
        Fixture { bi, map, self_phys }
    }

    fn check(f: &Fixture) -> Result<(), Invalid> {
        f.bi.validate(f.self_phys, &f.map)
    }

    /// Change the fixture with `damage` and expect a refusal naming `field`.
    fn refused(field: &str, damage: impl FnOnce(&mut Fixture)) -> Invalid {
        let mut f = fixture();
        damage(&mut f);
        match check(&f) {
            Ok(()) => panic!("a BootInfo with a damaged {field} was accepted"),
            Err(e) => {
                assert_eq!(e.field, field, "refused for the wrong reason: {e}");
                e
            }
        }
    }

    #[test]
    fn the_fixture_is_accepted() {
        let f = fixture();
        check(&f).unwrap();
        assert_eq!(size_of::<BootInfo>(), 720);
        assert_eq!(size_of::<MemRegion>(), 24);
    }

    #[test]
    fn identity_is_checked_first() {
        let e = refused("magic", |f| f.bi.magic ^= 1);
        assert_eq!(e.expected, Some(BOOT_INFO_MAGIC));
        let e = refused("version", |f| {
            f.bi.version = 2;
            f.bi.size = 1; // would also be wrong: the version is what is reported
        });
        assert_eq!(
            std::format!("{e}"),
            "version = 0x2: a BootInfo version this kernel does not read (expected 0x3)"
        );
        refused("size", |f| f.bi.size -= 8);
        refused("flags", |f| f.bi.flags |= 1 << 63);
    }

    #[test]
    fn the_linear_map_is_checked() {
        refused("phys_offset", |f| f.bi.phys_offset = 0xFFFF_9000_0000_0000);
        refused("phys_map_end", |f| f.bi.phys_map_end = 0);
        refused("phys_map_end", |f| f.bi.phys_map_end += PAGE);
        refused("phys_map_end", |f| f.bi.phys_map_end = LINEAR_MAP_MAX + HUGE);
    }

    #[test]
    fn the_memory_map_header_is_checked() {
        refused("memory_map", |f| f.bi.memory_map.phys += 8);
        refused("memory_map", |f| f.bi.memory_map.len = 0);
        refused("memory_map", |f| f.bi.memory_map.phys = f.bi.phys_map_end);
        refused("memory_map", |f| f.bi.memory_map.phys = u64::MAX - PAGE + 1);
        refused("memory_map_entry_size", |f| f.bi.memory_map_entry_size = 32);
        refused("memory_map_format", |f| f.bi.memory_map_format = 2);
        refused("memory_map_entries", |f| f.bi.memory_map_entries = 0);
        refused("memory_map_entries", |f| f.bi.memory_map_entries = 16 * PAGE / 24 + 1);
        refused("memory_map_entries", |f| f.bi.memory_map_entries = u64::MAX);
        // A count whose size in bytes wraps around to 8: small enough to pass a
        // check that multiplies without looking. The kernel builds its view of the
        // map from this count, so the header alone has to refuse it.
        let mut f = fixture();
        f.bi.memory_map_entries = u64::MAX / 24 + 1;
        assert_eq!(f.bi.validate_header().unwrap_err().field, "memory_map_entries");
        // The header says one thing, the kernel was given another.
        refused("memory_map_entries", |f| {
            f.map.pop();
        });
    }

    #[test]
    fn the_memory_map_entries_are_checked() {
        let e = refused("memory map kind", |f| f.map[2].kind = 42);
        assert_eq!(e.index, Some(2));
        refused("memory map kind", |f| f.map[0].kind = 0);
        refused("memory map start", |f| f.map[1].start += 1);
        refused("memory map start", |f| f.map[1].len = 0);
        refused("memory map start", |f| f.map.swap(0, 1));
        let e = refused("memory map start", |f| f.map[1].len += PAGE * 2048);
        assert_eq!(e.index, Some(2));
        refused("memory map start", |f| {
            let last = f.map.len() - 1;
            f.map[last] = region(4 * 1024 * MIB, MIB, mem_kind::USABLE);
        });
        refused("memory map length", |f| {
            let last = f.map.len() - 1;
            f.map[last] = region(u64::MAX - PAGE + 1, PAGE, mem_kind::RESERVED);
        });
        refused("memory_map", |f| {
            for r in f.map.iter_mut().filter(|r| r.kind == mem_kind::USABLE) {
                r.kind = mem_kind::BOOTLOADER_RECLAIMABLE;
            }
        });
    }

    #[test]
    fn images_and_hand_over_state_are_checked() {
        refused("kernel_image", |f| f.bi.kernel_image.len = 0);
        refused("kernel_image", |f| f.bi.kernel_image.len += 1);
        refused("kernel_image", |f| f.bi.kernel_image.phys = 0x7000_0000);
        refused("kernel_image", |f| f.bi.kernel_image.phys += 4 * MIB);
        refused("initrd", |f| f.bi.initrd.phys += 1);
        refused("initrd", |f| f.bi.initrd.len = f.bi.phys_map_end);
        refused("initrd", |f| f.bi.initrd.phys = 64 * MIB);
        refused("initrd", |f| {
            f.bi.initrd.len = 0; // described as absent, yet still pointing somewhere
        });
        refused("cmdline.len", |f| f.bi.cmdline.len = CMDLINE_MAX + 1);
        refused("cmdline", |f| f.bi.cmdline.phys = 64 * MIB);
        refused("boot_pml4", |f| f.bi.boot_pml4 += 8);
        refused("boot_pml4", |f| f.bi.boot_pml4 = 64 * MIB);
        refused("boot_stack.len", |f| f.bi.boot_stack.len = 4 * PAGE);
        refused("boot_stack", |f| f.bi.boot_stack.phys += 4 * PAGE);
        refused("BootInfo", |f| f.self_phys += PAGE);
    }

    #[test]
    fn the_platform_is_checked() {
        refused("framebuffer.stride", |f| f.bi.framebuffer.stride = 1279);
        refused("framebuffer.size", |f| f.bi.framebuffer.size = 1);
        refused("framebuffer.format", |f| f.bi.framebuffer.format = 3);
        refused("framebuffer.phys_addr", |f| f.bi.framebuffer.phys_addr += 4);
        refused("framebuffer.phys_addr", |f| f.bi.framebuffer.phys_addr = f.bi.phys_map_end);
        refused("framebuffer.size", |f| {
            f.bi.framebuffer.stride = u32::MAX;
            f.bi.framebuffer.height = u32::MAX;
        });
        refused("framebuffer", |f| f.bi.framebuffer.present = 0);
        refused("uart_kind", |f| f.bi.uart_kind = 7);
        refused("uart", |f| f.bi.uart = 0x900_0000);
        refused("flags", |f| f.bi.rsdp = 0);
        refused("flags", |f| f.bi.boot_time = 0);
        refused("flags", |f| f.bi.flags &= !flags::FRAMEBUFFER);
    }

    #[test]
    fn reservations_are_checked() {
        refused("reservation_count", |f| f.bi.reservation_count = MAX_RESERVATIONS as u32 + 1);
        refused("reservations", |f| f.bi.reservations[MAX_RESERVATIONS - 1].owner = 1);
        let e = refused("reservation owner", |f| f.bi.reservations[3].owner = 9);
        assert_eq!(e.index, Some(3));
        refused("reservation owner", |f| f.bi.reservations[3].owner = owner::KERNEL_IMAGE);
        refused("reservation", |f| f.bi.reservations[2].range.len = 0);
        refused("reservation", |f| f.bi.reservations[2].range.phys += 8);
        // Outside the kernel's memory: in RAM the allocator would hand out.
        refused("reservation", |f| f.bi.reservations[6].range.phys = 64 * MIB);
        refused("reservation", |f| f.bi.reservations[6].range = f.bi.reservations[5].range);
        // Every range in the reservation of what it is.
        refused("boot_stack", |f| {
            f.bi.reservations[3] = f.bi.reservations[6];
            f.bi.reservations[6] = Reservation::default();
            f.bi.reservation_count = 6;
        });
        refused("initrd", |f| f.bi.reservations[1].range.len = PAGE);
        refused("reservations", |f| {
            f.bi.initrd = PhysRange::default();
            f.bi.initrd_sha256 = [0; 32];
            f.bi.flags &= !flags::BOOT_IMAGE;
        });
    }

    #[test]
    fn entropy_and_the_boot_slot_are_checked() {
        refused("entropy.len", |f| f.bi.entropy.len = ENTROPY_MAX as u32 + 1);
        refused("entropy.len", |f| f.bi.entropy.len = 0);
        refused("entropy.source", |f| f.bi.entropy.source = 9);
        refused("entropy.bytes", |f| f.bi.entropy.bytes[40] = 1);
        refused("boot_slot.slot", |f| f.bi.boot_slot.slot = 7);
        refused("boot_slot.reason", |f| f.bi.boot_slot.reason = recovery_reason::OPERATOR);
        refused("boot_slot.reason", |f| f.bi.boot_slot.slot = slot::RECOVERY);
        refused("boot_slot.state", |f| f.bi.boot_slot.state = 5);
        refused("boot_slot.attempts", |f| f.bi.boot_slot.attempts = NOT_COUNTED);
        refused("boot_slot.attempts", |f| {
            f.bi.boot_slot = BootSlot {
                slot: slot::RECOVERY,
                reason: recovery_reason::FAILED_BOOTS,
                attempts: 1,
                state: recovery_state::DATA_VOLUME,
            }
        });
        // Each way recovery can legitimately be entered is accepted.
        let mut f = fixture();
        f.bi.boot_slot = BootSlot {
            slot: slot::RECOVERY,
            reason: recovery_reason::FAILED_BOOTS,
            attempts: 3,
            state: recovery_state::DATA_VOLUME,
        };
        check(&f).unwrap();
        f.bi.boot_slot = BootSlot {
            slot: slot::RECOVERY,
            reason: recovery_reason::OPERATOR,
            attempts: NOT_COUNTED,
            state: 0,
        };
        check(&f).unwrap();
    }

    #[test]
    fn the_command_line_is_text() {
        assert_eq!(validate_cmdline(b"init=bin/init").unwrap(), "init=bin/init");
        let e = validate_cmdline(b"init=\xFFbin").unwrap_err();
        assert_eq!(e.value, 5);
        assert!(validate_cmdline(&[b'a'; 4097]).is_err());
    }

    /// What the kernel relies on once `validate` has said yes, checked independently
    /// of how `validate` checks it.
    fn kernel_can_trust(f: &Fixture) {
        let bi = &f.bi;
        let end = bi.phys_map_end;
        let inside = |r: &PhysRange| r.phys.checked_add(r.len).is_some_and(|e| e <= end);
        assert!(inside(&bi.memory_map) && inside(&bi.kernel_image) && inside(&bi.boot_stack));
        assert!(bi.memory_map_entries as usize * 24 <= bi.memory_map.len as usize);
        for w in f.map.windows(2) {
            assert!(w[0].start + w[0].len <= w[1].start, "regions overlap");
        }
        for r in &f.map {
            if mem_kind::is_ram(r.kind) {
                assert!(r.start + r.len <= end, "RAM beyond the linear map");
            }
        }
        let free = |p: &PhysRange| {
            f.map.iter().any(|m| {
                matches!(m.kind, mem_kind::USABLE | mem_kind::BOOTLOADER_RECLAIMABLE)
                    && p.phys < m.start + m.len
                    && m.start < p.phys + p.len.max(1)
            })
        };
        for r in bi.reserved() {
            assert!(!free(&r.range), "a reservation in memory the allocator hands out");
        }
        assert!(!free(&bi.kernel_image) && !free(&bi.boot_stack) && !free(&bi.memory_map));
        assert!(bi.initrd.len == 0 || (inside(&bi.initrd) && !free(&bi.initrd)));
        assert!(bi.cmdline.len <= CMDLINE_MAX);
        assert!(bi.entropy.len as usize <= ENTROPY_MAX);
    }

    /// Random damage, anywhere in the structure or the map: the validator never
    /// panics (overflow included: tests build with overflow checks), and whatever
    /// it accepts keeps every promise the kernel relies on.
    #[test]
    fn random_damage_is_refused_or_harmless() {
        let mut seed = 0x9E37_79B9_7F4A_7C15u64;
        let mut next = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        let size = size_of::<BootInfo>();
        let mut accepted = 0;
        for _ in 0..200_000 {
            let mut f = fixture();
            for _ in 0..1 + next() % 3 {
                if next() % 4 == 0 {
                    // A memory-map entry.
                    let i = (next() % f.map.len() as u64) as usize;
                    let word = (next() % 3) as usize;
                    let v = interesting(next());
                    match word {
                        0 => f.map[i].start = v,
                        1 => f.map[i].len = v,
                        _ => f.map[i].kind = v as u32,
                    }
                } else {
                    // Eight bytes of the structure, at any offset.
                    let off = (next() % (size as u64 - 7)) as usize;
                    let v = interesting(next()).to_le_bytes();
                    // SAFETY: `BootInfo` is plain integers: every byte pattern is a value.
                    let bytes = unsafe {
                        core::slice::from_raw_parts_mut(&mut f.bi as *mut BootInfo as *mut u8, size)
                    };
                    bytes[off..off + 8].copy_from_slice(&v);
                }
            }
            if check(&f).is_ok() {
                accepted += 1;
                kernel_can_trust(&f);
            }
        }
        // Damage that happens to keep every rule (a different timestamp, other
        // entropy bytes) is accepted; most is not.
        assert!(accepted > 0 && accepted < 100_000, "accepted {accepted}");
    }

    /// Values near the edges, where arithmetic overflows and bounds are off by one.
    fn interesting(r: u64) -> u64 {
        match r % 8 {
            0 => 0,
            1 => u64::MAX,
            2 => u64::MAX - (r >> 3) % 4096,
            3 => (r >> 3) % 0x1_0000_0000,
            4 => ((r >> 3) % 4096) * PAGE,
            5 => 1 << ((r >> 3) % 64),
            6 => 16 * MIB + ((r >> 3) % 64) * PAGE,
            _ => r,
        }
    }
}
