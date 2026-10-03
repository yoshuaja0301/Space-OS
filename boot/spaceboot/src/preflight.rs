use core::arch::x86_64::__cpuid;
use uefi::boot::{self, MemoryType};
use uefi::mem::memory_map::MemoryMap;
use uefi::proto::console::gop::GraphicsOutput;
use uefi::proto::media::block::BlockIO;
use uefi::table::cfg::ACPI2_GUID;

use crate::policy;

pub struct Report {
    pub vendor: [u8; 12],
    pub memory_mib: u64,
    pub regions: usize,
    pub block_handles: usize,
    pub graphics: bool,
    pub acpi: bool,
}

pub fn inspect() -> Result<Report, &'static str> {
    let basic = __cpuid(0);
    let mut vendor = [0; 12];
    vendor[..4].copy_from_slice(&basic.ebx.to_le_bytes());
    vendor[4..8].copy_from_slice(&basic.edx.to_le_bytes());
    vendor[8..].copy_from_slice(&basic.ecx.to_le_bytes());
    let extended = __cpuid(0x8000_0000);
    if extended.eax < 0x8000_0001 {
        return Err("CPU: extended feature information is unavailable.");
    }
    policy::cpu_check(__cpuid(0x8000_0001).edx)?;
    let map = boot::memory_map(MemoryType::LOADER_DATA).map_err(|_| "Cannot read UEFI memory map.")?;
    let mut pages = 0u64;
    for region in map.entries() {
        if matches!(
            region.ty,
            MemoryType::CONVENTIONAL
                | MemoryType::LOADER_CODE
                | MemoryType::LOADER_DATA
                | MemoryType::BOOT_SERVICES_CODE
                | MemoryType::BOOT_SERVICES_DATA
        ) {
            pages = pages.checked_add(region.page_count).ok_or("Invalid firmware memory size.")?;
        }
    }
    let memory_mib = pages / 256;
    if memory_mib < policy::MIN_MEMORY_MIB {
        return Err("Memory: less than 128 MiB available. More RAM is required.");
    }
    let report = Report {
        vendor,
        memory_mib,
        regions: map.entries().len(),
        block_handles: boot::find_handles::<BlockIO>().map_or(0, |handles| handles.len()),
        graphics: boot::get_handle_for_protocol::<GraphicsOutput>().is_ok(),
        acpi: uefi::system::with_config_table(|entries| entries.iter().any(|e| e.guid == ACPI2_GUID)),
    };
    Ok(report)
}
