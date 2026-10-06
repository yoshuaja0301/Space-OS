//! The bootloader's hand-over, checked before it is believed (PRD v0.2 B02,
//! ADR-0032).
//!
//! Nothing the bootloader says is used before it has passed the contract's checks
//! (`spaceabi::boot`): first the structure on its own, then the memory map it points
//! to, then the reservations against that map. Every range the kernel is about to
//! read is also looked up in the live page tables first, so a lie about an address
//! ends in a refusal and not in a fault. The boot image must hash to what the
//! bootloader measured, and the command line must be text.
//!
//! A refusal is a panic whose message names the field, its value and the rule it
//! breaks: the kernel cannot run on a hand-over it does not trust, and the panic
//! path is the one that already reports and ends the machine.

use core::fmt;

use spaceabi::boot::{
    BootInfo, MemRegion, NOT_COUNTED, PHYS_OFFSET, entropy_source, owner, recovery_reason, slot,
    validate_cmdline,
};
use spaceabi::sha256;

use crate::mm::range_is_mapped;

fn reject(why: fmt::Arguments) -> ! {
    panic!("BootInfo rejected: {why}")
}

fn hex(digest: &[u8; 32]) -> [u8; 64] {
    sha256::to_hex(digest)
}

/// Check everything the bootloader handed over and return it, or stop with the
/// reason. Takes the boot entropy out of the structure (`dev::entropy`) and wipes
/// it there.
pub fn accept(ptr: *const BootInfo) -> &'static BootInfo {
    let va = ptr as u64;
    let size = core::mem::size_of::<BootInfo>() as u64;
    if !va.is_multiple_of(8) || !range_is_mapped(va, size) {
        reject(format_args!("its address {va:#x} is not mapped, 8-byte aligned memory"));
    }
    {
        // SAFETY: mapped and aligned (checked above); `BootInfo` is plain integers,
        // so whatever bytes are there are a value of it.
        let bi = unsafe { &*ptr };
        if let Err(e) = bi.validate_header() {
            reject(format_args!("{e}"));
        }
        let map_va = PHYS_OFFSET + bi.memory_map.phys;
        if !range_is_mapped(map_va, bi.memory_map.len) {
            reject(format_args!("memory_map = {:#x}: not mapped", bi.memory_map.phys));
        }
        // SAFETY: mapped (checked above), page-aligned, and the header check put
        // `memory_map_entries` entries inside the range.
        let map: &[MemRegion] = unsafe {
            core::slice::from_raw_parts(map_va as *const MemRegion, bi.memory_map_entries as usize)
        };
        if let Err(e) = bi.validate(va - PHYS_OFFSET, map) {
            reject(format_args!("{e}"));
        }
        // Inside memory the map gives the kernel, below the end of the linear map --
        // and still looked up, because the linear map has holes where there is no RAM.
        for r in bi.reserved() {
            if !range_is_mapped(PHYS_OFFSET + r.range.phys, r.range.len) {
                reject(format_args!(
                    "the {} reserved at {:#x}: not mapped",
                    owner::name(r.owner),
                    r.range.phys
                ));
            }
        }
        if bi.cmdline.len != 0 {
            // SAFETY: inside the command line's reservation, mapped (checked above).
            let bytes = unsafe {
                core::slice::from_raw_parts(
                    (PHYS_OFFSET + bi.cmdline.phys) as *const u8,
                    bi.cmdline.len as usize,
                )
            };
            if let Err(e) = validate_cmdline(bytes) {
                reject(format_args!("{e}"));
            }
        }
        if bi.initrd.len != 0 {
            // SAFETY: inside the boot image's reservation, mapped (checked above).
            let image = unsafe {
                core::slice::from_raw_parts(
                    (PHYS_OFFSET + bi.initrd.phys) as *const u8,
                    bi.initrd.len as usize,
                )
            };
            let digest = sha256::digest(image);
            if digest != bi.initrd_sha256 {
                let (got, want) = (hex(&digest), hex(&bi.initrd_sha256));
                reject(format_args!(
                    "initrd: the boot image hashes to {}, not to the {} the bootloader measured",
                    core::str::from_utf8(&got).unwrap_or("?"),
                    core::str::from_utf8(&want).unwrap_or("?"),
                ));
            }
        }
        // The entropy goes to the entropy source, and leaves no copy behind.
        crate::dev::entropy::add_boot_entropy(&bi.entropy.bytes[..bi.entropy.len as usize]);
    }
    // SAFETY: the structure's page is the kernel's (reservation BOOT_INFO) and
    // mapped writable; no reference to it is alive across this write.
    unsafe { core::ptr::write_volatile(&raw mut (*ptr.cast_mut()).entropy.bytes, [0; 64]) };
    // SAFETY: as at the top; nothing writes it from here on.
    let bi: &'static BootInfo = unsafe { &*ptr };
    report(bi);
    bi
}

fn report(bi: &BootInfo) {
    println!(
        "[kernel] boot info v{} accepted: {} bytes, {} memory regions, {} reservations, initrd {} bytes, cmdline {} bytes, rsdp {:#x}",
        bi.version,
        bi.size,
        bi.memory_map_entries,
        bi.reservation_count,
        bi.initrd.len,
        bi.cmdline.len,
        bi.rsdp
    );
    for r in bi.reserved() {
        println!(
            "[kernel]   reserved {:#x}..{:#x} ({} KiB): {}",
            r.range.phys,
            r.range.phys + r.range.len,
            r.range.len / 1024,
            owner::name(r.owner)
        );
    }
    if bi.initrd.len != 0 {
        let h = hex(&bi.initrd_sha256);
        println!(
            "[kernel] boot image: sha256 {} as the bootloader measured it",
            core::str::from_utf8(&h).unwrap_or("?")
        );
    }
    // No heap yet: everything below prints without allocating.
    let s = &bi.boot_slot;
    let which = if s.slot == slot::RECOVERY { "recovery" } else { "normal" };
    let why = recovery_reason::name(s.reason);
    match (s.slot == slot::RECOVERY, s.attempts == NOT_COUNTED) {
        (false, true) => println!("[kernel] boot slot: {which}; boots are not counted"),
        (false, false) => {
            println!("[kernel] boot slot: {which}; {} boot(s) before this one did not come up", s.attempts)
        }
        (true, true) => println!("[kernel] boot slot: {which} ({why}); boots are not counted"),
        (true, false) => println!(
            "[kernel] boot slot: {which} ({why}); {} boot(s) before this one did not come up",
            s.attempts
        ),
    }
    match bi.entropy.source {
        entropy_source::NONE => println!("[kernel] boot entropy: none from the firmware"),
        source => println!(
            "[kernel] boot entropy: {} bytes from {}, mixed in and never counted as a source",
            bi.entropy.len,
            entropy_source::name(source)
        ),
    }
}
