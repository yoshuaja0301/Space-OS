//! Just enough ACPI to find the processors (RSDP → XSDT or RSDT → MADT) and how to
//! switch the machine off (the FADT and the DSDT's `\_S5`, used by `power`).
//!
//! The MADT lists every local APIC the firmware knows. Only those marked enabled are
//! CPUs that exist now ("online capable" ones are sockets for hot-plug). Tables are
//! read through the linear map, which covers ACPI memory (spaceboot maps every kind
//! of RAM); an address it does not cover is refused rather than guessed at, and a
//! checksum that does not add up means the table is not used.

use alloc::vec::Vec;

use crate::mm::{kernel_addr_is_mapped, phys_to_virt};

/// What the MADT says about the processors.
pub struct Processors {
    /// Local APIC IDs of the enabled processors, in table order.
    pub apic_ids: Vec<u32>,
}

/// `len` bytes at physical `pa`, if the linear map covers them.
fn bytes(pa: u64, len: usize) -> Option<&'static [u8]> {
    if len == 0 {
        return None;
    }
    let first = phys_to_virt(pa).as_u64();
    let last = first.checked_add(len as u64 - 1)?;
    // Check every page: a table can straddle into a hole.
    let mut page = first & !0xFFF;
    while page <= last {
        if !kernel_addr_is_mapped(page) {
            return None;
        }
        page += 0x1000;
    }
    // SAFETY: every page of the range is mapped (checked above); ACPI tables are
    // firmware-owned memory the kernel never writes.
    Some(unsafe { core::slice::from_raw_parts(first as *const u8, len) })
}

fn checksum_ok(b: &[u8]) -> bool {
    b.iter().fold(0u8, |a, x| a.wrapping_add(*x)) == 0
}

pub fn u32_at(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}

pub fn u64_at(b: &[u8], at: usize) -> u64 {
    u64::from(u32_at(b, at)) | (u64::from(u32_at(b, at + 4)) << 32)
}

/// A whole system description table at `pa`, if its header and checksum are sound.
fn table(pa: u64) -> Option<&'static [u8]> {
    let header = bytes(pa, 36)?;
    let len = u32_at(header, 4) as usize;
    if !(36..=1 << 20).contains(&len) {
        return None;
    }
    let t = bytes(pa, len)?;
    checksum_ok(t).then_some(t)
}

/// The system description table with signature `sig`, found through the RSDP.
pub fn find(rsdp_pa: u64, sig: &[u8; 4]) -> Result<&'static [u8], &'static str> {
    if rsdp_pa == 0 {
        return Err("the firmware gave no ACPI tables");
    }
    let rsdp = bytes(rsdp_pa, 20).ok_or("the RSDP is outside the linear map")?;
    if &rsdp[..8] != b"RSD PTR " || !checksum_ok(rsdp) {
        return Err("the RSDP is not valid");
    }
    // ACPI 2.0+: the XSDT, with 64-bit entries. ACPI 1.0: the RSDT, 32-bit.
    let revision = rsdp[15];
    let (root, entry_size) = match bytes(rsdp_pa, 36) {
        Some(ext) if revision >= 2 && checksum_ok(ext) && u64_at(ext, 24) != 0 => (u64_at(ext, 24), 8),
        _ => (u64::from(u32_at(rsdp, 16)), 4),
    };
    let root = table(root).ok_or("the root system description table is not readable")?;
    root[36..]
        .chunks_exact(entry_size)
        .map(|e| if entry_size == 8 { u64_at(e, 0) } else { u64::from(u32_at(e, 0)) })
        .filter_map(table)
        .find(|t| &t[..4] == sig)
        .ok_or("no such table")
}

/// A whole table at physical `pa` (one that another table points to, like the DSDT).
pub fn table_at(pa: u64) -> Option<&'static [u8]> {
    table(pa)
}

/// The processors, or why they could not be read.
pub fn processors(rsdp_pa: u64) -> Result<Processors, &'static str> {
    let madt = find(rsdp_pa, b"APIC").map_err(|e| if e == "no such table" { "no MADT" } else { e })?;
    let mut apic_ids = Vec::new();
    let mut at = 44;
    while at + 2 <= madt.len() {
        let (kind, len) = (madt[at], madt[at + 1] as usize);
        if len < 2 || at + len > madt.len() {
            break;
        }
        let e = &madt[at..at + len];
        match kind {
            // Processor local APIC: ACPI ID, APIC ID, flags (bit 0: enabled).
            0 if len >= 8 && u32_at(e, 4) & 1 != 0 => apic_ids.push(u32::from(e[3])),
            // Processor local x2APIC: reserved, x2APIC ID, flags, ACPI UID.
            9 if len >= 16 && u32_at(e, 8) & 1 != 0 => apic_ids.push(u32_at(e, 4)),
            _ => {}
        }
        at += len;
    }
    // A firmware may list a CPU twice (as xAPIC and as x2APIC).
    let mut seen = Vec::new();
    apic_ids.retain(|id| {
        let fresh = !seen.contains(id);
        seen.push(*id);
        fresh
    });
    if apic_ids.is_empty() {
        return Err("the MADT lists no enabled processor");
    }
    Ok(Processors { apic_ids })
}
