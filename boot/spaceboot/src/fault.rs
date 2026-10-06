//! `bootinfo_fault=<name>` in `spaceos.cfg`: hand the kernel a BootInfo damaged in
//! one named way, so a test can watch the kernel refuse it (PRD v0.2 B02, ADR-0032).
//!
//! The damage is done after everything else, on the structure that is about to be
//! handed over, so the kernel sees exactly what a bootloader bug or a stray write
//! would have left there. A real configuration never has the line; a boot with it
//! says so on the console before boot services end.

use spaceabi::boot::{BootInfo, ENTROPY_MAX, MemRegion};

pub struct Fault {
    pub name: &'static str,
    /// What the kernel is handed, for the console.
    pub what: &'static str,
    apply: fn(&mut BootInfo, &mut [MemRegion]),
}

impl Fault {
    pub fn apply(&self, bi: &mut BootInfo, regions: &mut [MemRegion]) {
        (self.apply)(bi, regions)
    }
}

const FAULTS: &[Fault] = &[
    Fault { name: "magic", what: "a magic number one bit off", apply: |bi, _| bi.magic ^= 1 },
    Fault { name: "version", what: "BootInfo version 99", apply: |bi, _| bi.version = 99 },
    Fault { name: "size", what: "a size 8 bytes short", apply: |bi, _| bi.size -= 8 },
    Fault { name: "flags", what: "a flag no version defines", apply: |bi, _| bi.flags |= 1 << 63 },
    Fault {
        name: "phys_offset",
        what: "a linear map 1 TiB from where it is",
        apply: |bi, _| bi.phys_offset += 1 << 40,
    },
    Fault {
        name: "memmap_count",
        what: "one memory-map entry more than the array holds",
        apply: |bi, _| {
            bi.memory_map_entries = bi.memory_map.len / core::mem::size_of::<MemRegion>() as u64 + 1
        },
    },
    Fault {
        name: "memmap_overlap",
        what: "a memory map whose second entry repeats the first",
        apply: |_, regions| {
            if regions.len() > 1 {
                regions[1] = regions[0];
            }
        },
    },
    Fault {
        name: "kernel_image",
        what: "a kernel image moved past itself",
        apply: |bi, _| bi.kernel_image.phys += bi.kernel_image.len,
    },
    Fault {
        name: "initrd",
        what: "a boot image as long as the linear map",
        apply: |bi, _| bi.initrd.len = bi.phys_map_end,
    },
    Fault {
        name: "initrd_hash",
        what: "a boot image whose digest has one byte flipped",
        apply: |bi, _| bi.initrd_sha256[0] ^= 0xFF,
    },
    Fault {
        name: "cmdline_utf8",
        what: "a command line that is not UTF-8",
        apply: |bi, _| {
            if bi.cmdline.len != 0 {
                // SAFETY: the command line's pages are ours (MT_KERNEL) and the
                // firmware's identity map is still in place.
                unsafe { *(bi.cmdline.phys as *mut u8) = 0xFF };
            }
        },
    },
    Fault {
        name: "reservation",
        what: "two reservations over the same pages",
        apply: |bi, _| bi.reservations[1].range = bi.reservations[0].range,
    },
    Fault {
        name: "entropy",
        what: "more entropy than there is room for",
        apply: |bi, _| bi.entropy.len = ENTROPY_MAX as u32 + 1,
    },
    Fault { name: "slot", what: "boot slot 7", apply: |bi, _| bi.boot_slot.slot = 7 },
    Fault {
        name: "framebuffer",
        what: "a framebuffer whose lines are shorter than the screen",
        apply: |bi, _| bi.framebuffer.stride = bi.framebuffer.width.saturating_sub(1),
    },
];

/// The fault `spaceos.cfg` asks for, if it is one spaceboot knows.
pub fn find(name: &[u8]) -> Option<&'static Fault> {
    FAULTS.iter().find(|f| f.name.as_bytes() == name)
}
