use super::*;

pub(super) const TARGET_GUID: &str = "53504143-454f-4000-8000-000000000002";
const SENTINEL: &[u8] = b"Space OS setup QA: preserve existing Microsoft files.\0\xff";
const FILES: [&str; 4] = [
    "EFI/BOOT/BOOTX64.EFI",
    "EFI/SPACEOS/spacekernel.elf",
    "EFI/SPACEOS/initrd.tar",
    "EFI/SPACEOS/spaceos.cfg",
];
const TARGET_END: u64 = ESP_SIZE - 33 * 512;

const fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = !0_u32;
    let mut index = 0;
    while index < bytes.len() {
        crc ^= bytes[index] as u32;
        let mut bit = 0;
        while bit < 8 {
            crc = (crc >> 1) ^ (0xedb8_8320 & 0_u32.wrapping_sub(crc & 1));
            bit += 1;
        }
        index += 1;
    }
    !crc
}

fn write_gpt(file: &mut fs::File, partition_end: u64, unique: u8) -> Result<(), String> {
    let last = file.metadata().map_err(|e| e.to_string())?.len() / 512 - 1;
    let mut mbr = [0_u8; 512];
    mbr[446 + 4] = 0xee;
    mbr[446 + 1..446 + 4].copy_from_slice(&[0, 2, 0]);
    mbr[446 + 5..446 + 8].copy_from_slice(&[0xff; 3]);
    mbr[446 + 8..446 + 12].copy_from_slice(&1_u32.to_le_bytes());
    mbr[446 + 12..446 + 16].copy_from_slice(&u32::try_from(last).map_err(|e| e.to_string())?.to_le_bytes());
    mbr[510..512].copy_from_slice(&[0x55, 0xaa]);
    file.seek(SeekFrom::Start(0)).map_err(|e| e.to_string())?;
    file.write_all(&mbr).map_err(|e| e.to_string())?;
    let mut entries = [0_u8; 128 * 128];
    entries[..16].copy_from_slice(&[
        0x28, 0x73, 0x2a, 0xc1, 0x1f, 0xf8, 0xd2, 0x11, 0xba, 0x4b, 0x00, 0xa0, 0xc9, 0x3e, 0xc9, 0x3b,
    ]);
    entries[16..32].copy_from_slice(&[
        0x43, 0x41, 0x50, 0x53, 0x4f, 0x45, 0x00, 0x40, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, unique,
    ]);
    entries[32..40].copy_from_slice(&(PART_START / 512).to_le_bytes());
    entries[40..48].copy_from_slice(&(partition_end / 512 - 1).to_le_bytes());
    for (slot, code) in entries[56..128].chunks_exact_mut(2).zip("SETUP QA ESP".encode_utf16()) {
        slot.copy_from_slice(&code.to_le_bytes());
    }
    for (current, backup, entry_lba) in [(1, last, 2), (last, 1, last - 32)] {
        let mut header = [0_u8; 512];
        header[..8].copy_from_slice(b"EFI PART");
        header[8..12].copy_from_slice(&0x0001_0000_u32.to_le_bytes());
        header[12..16].copy_from_slice(&92_u32.to_le_bytes());
        header[24..32].copy_from_slice(&current.to_le_bytes());
        header[32..40].copy_from_slice(&backup.to_le_bytes());
        header[40..48].copy_from_slice(&34_u64.to_le_bytes());
        header[48..56].copy_from_slice(&(last - 33).to_le_bytes());
        header[56..72].copy_from_slice(&entries[16..32]);
        header[71] = unique + 16;
        header[72..80].copy_from_slice(&entry_lba.to_le_bytes());
        header[80..84].copy_from_slice(&128_u32.to_le_bytes());
        header[84..88].copy_from_slice(&128_u32.to_le_bytes());
        header[88..92].copy_from_slice(&crc32(&entries).to_le_bytes());
        let checksum = crc32(&header[..92]);
        header[16..20].copy_from_slice(&checksum.to_le_bytes());
        file.seek(SeekFrom::Start(entry_lba * 512)).map_err(|e| e.to_string())?;
        file.write_all(&entries).map_err(|e| e.to_string())?;
        file.seek(SeekFrom::Start(current * 512)).map_err(|e| e.to_string())?;
        file.write_all(&header).map_err(|e| e.to_string())?;
    }
    file.sync_all().map_err(|e| e.to_string())
}

pub(super) fn prepare(source: &Path, target: &Path) -> Result<(), String> {
    let mut file = fs::OpenOptions::new().read(true).write(true).open(source).map_err(|e| e.to_string())?;
    // Put backup GPT outside the existing source FAT so its bytes stay intact.
    file.set_len(ESP_SIZE + 33 * 512).map_err(|e| e.to_string())?;
    write_gpt(&mut file, ESP_SIZE, 1)?;
    let mut file = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .open(target)
        .map_err(|e| e.to_string())?;
    file.set_len(ESP_SIZE).map_err(|e| e.to_string())?;
    write_gpt(&mut file, TARGET_END, 2)?;
    let slice = fscommon::StreamSlice::new(file, PART_START, TARGET_END).map_err(|e| e.to_string())?;
    let mut slice = fscommon::BufStream::new(slice);
    fatfs::format_volume(
        &mut slice,
        fatfs::FormatVolumeOptions::new().fat_type(fatfs::FatType::Fat32).volume_label(*b"SETUP QA   "),
    )
    .map_err(|e| e.to_string())?;
    {
        let volume =
            fatfs::FileSystem::new(&mut slice, fatfs::FsOptions::new()).map_err(|e| e.to_string())?;
        volume
            .root_dir()
            .create_dir("EFI")
            .map_err(|e| e.to_string())?
            .create_dir("Microsoft")
            .map_err(|e| e.to_string())?
            .create_file("sentinel.bin")
            .map_err(|e| e.to_string())?
            .write_all(SENTINEL)
            .map_err(|e| e.to_string())?;
    }
    slice.flush().map_err(|e| e.to_string())
}

pub(super) fn files(path: &Path, end: u64) -> Result<Vec<Vec<u8>>, String> {
    let file = fs::File::open(path).map_err(|e| e.to_string())?;
    let slice = fscommon::StreamSlice::new(file, PART_START, end).map_err(|e| e.to_string())?;
    let volume = fatfs::FileSystem::new(slice, fatfs::FsOptions::new()).map_err(|e| e.to_string())?;
    FILES
        .iter()
        .map(|name| {
            let mut bytes = Vec::new();
            volume
                .root_dir()
                .open_file(name)
                .map_err(|e| format!("{name}: {e}"))?
                .read_to_end(&mut bytes)
                .map_err(|e| e.to_string())?;
            Ok(bytes)
        })
        .collect()
}

pub(super) fn verify(target: &Path, expected: &[Vec<u8>], original: &[u8]) -> Result<(), String> {
    if files(target, TARGET_END)? != expected {
        return Err("installed boot files differ from installer source".into());
    }
    let bytes = fs::read(target).map_err(|e| e.to_string())?;
    let start = usize::try_from(PART_START).map_err(|e| e.to_string())?;
    let end = usize::try_from(TARGET_END).map_err(|e| e.to_string())?;
    if bytes[..start] != original[..start] || bytes[end..] != original[end..] {
        return Err("installation changed the target partition table".into());
    }
    let file = fs::File::open(target).map_err(|e| e.to_string())?;
    let slice = fscommon::StreamSlice::new(file, PART_START, TARGET_END).map_err(|e| e.to_string())?;
    let volume = fatfs::FileSystem::new(slice, fatfs::FsOptions::new()).map_err(|e| e.to_string())?;
    let mut sentinel = Vec::new();
    volume
        .root_dir()
        .open_file("EFI/Microsoft/sentinel.bin")
        .map_err(|e| e.to_string())?
        .read_to_end(&mut sentinel)
        .map_err(|e| e.to_string())?;
    if sentinel != SENTINEL {
        return Err("installation modified the Microsoft sentinel".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc_matches_ieee_check_vector() {
        assert_eq!(super::crc32(b"123456789"), 0xcbf4_3926);
    }

    #[test]
    fn target_fixture_has_valid_gpt_and_preserves_existing_os_directory() -> Result<(), String> {
        let directory = std::env::temp_dir().join(format!("spaceos-setup-fixture-{}", std::process::id()));
        fs::create_dir_all(&directory).map_err(|e| e.to_string())?;
        let source = directory.join("source.img");
        let target = directory.join("target.img");
        fs::File::create(&source).and_then(|file| file.set_len(ESP_SIZE)).map_err(|e| e.to_string())?;
        prepare(&source, &target)?;
        let bytes = fs::read(&target).map_err(|e| e.to_string())?;
        assert_eq!(bytes.len() as u64, ESP_SIZE);
        assert_eq!(&bytes[510..512], &[0x55, 0xaa]);
        assert_eq!(bytes[450], 0xee);
        let last = bytes.len() / 512 - 1;
        for sector in [1, last] {
            let mut header = bytes[sector * 512..sector * 512 + 92].to_vec();
            assert_eq!(&header[..8], b"EFI PART");
            let checksum = u32::from_le_bytes(header[16..20].try_into().map_err(|e| format!("{e}"))?);
            header[16..20].fill(0);
            assert_eq!(crc32(&header), checksum);
            let entry_lba = u64::from_le_bytes(header[72..80].try_into().map_err(|e| format!("{e}"))?);
            let offset = usize::try_from(entry_lba * 512).map_err(|e| e.to_string())?;
            let entries = &bytes[offset..offset + 128 * 128];
            let expected_crc = u32::from_le_bytes(header[88..92].try_into().map_err(|e| format!("{e}"))?);
            assert_eq!(crc32(entries), expected_crc);
            assert_eq!(entries[31], 2);
            assert!(entries[128..].iter().all(|byte| *byte == 0));
        }
        let file = fs::File::open(&target).map_err(|e| e.to_string())?;
        let slice = fscommon::StreamSlice::new(file, PART_START, TARGET_END).map_err(|e| e.to_string())?;
        let volume = fatfs::FileSystem::new(slice, fatfs::FsOptions::new()).map_err(|e| e.to_string())?;
        let root = volume.root_dir();
        let mut sentinel = Vec::new();
        root.open_file("EFI/Microsoft/sentinel.bin")
            .map_err(|e| e.to_string())?
            .read_to_end(&mut sentinel)
            .map_err(|e| e.to_string())?;
        assert_eq!(sentinel, SENTINEL);
        assert!(root.open_file(FILES[0]).is_err());
        assert!(root.open_dir("EFI/SPACEOS").is_err());
        drop(root);
        drop(volume);
        fs::remove_file(source).map_err(|e| e.to_string())?;
        fs::remove_file(target).map_err(|e| e.to_string())?;
        fs::remove_dir(directory).map_err(|e| e.to_string())?;
        Ok(())
    }
}
