use alloc::{format, string::String, vec::Vec};
use uefi::proto::{
    loaded_image::LoadedImage,
    media::{
        file::{Directory, File, FileAttribute, FileInfo, FileMode, FileSystemInfo},
        fs::SimpleFileSystem,
        partition::{GptPartitionType, PartitionInfo},
    },
};
use uefi::{CStr16, Handle, Status, boot, cstr16, fs::FileSystem};

const BOOT: &CStr16 = cstr16!("\\EFI\\BOOT\\BOOTX64.EFI");
const SPACEOS: &CStr16 = cstr16!("\\EFI\\SPACEOS");
const EFI: &CStr16 = cstr16!("\\EFI");
const BOOT_DIR: &CStr16 = cstr16!("\\EFI\\BOOT");
const PATHS: [&CStr16; 4] = [crate::KERNEL_PATH, crate::INITRD_PATH, crate::CONFIG_PATH, BOOT];

pub struct Target {
    pub id: Handle,
    pub label: String,
}

pub struct Summary {
    pub files: usize,
    pub bytes: usize,
}

fn source_device() -> Result<Handle, &'static str> {
    boot::open_protocol_exclusive::<LoadedImage>(boot::image_handle())
        .map_err(|_| "Cannot identify the installer boot device.")?
        .device()
        .ok_or("The installer boot image has no filesystem device.")
}

fn target_label(id: Handle, source: Handle) -> Result<String, &'static str> {
    if id == source {
        return Err("The installer boot device cannot be the destination.");
    }
    let partition = boot::open_protocol_exclusive::<PartitionInfo>(id)
        .map_err(|_| "Destination has no EFI partition information.")?;
    if !partition.is_system() {
        return Err("Destination is not an EFI system partition.");
    }
    let entry = partition.gpt_partition_entry().ok_or("Destination is not a GPT EFI partition.")?;
    let partition_type = entry.partition_type_guid;
    if partition_type != GptPartitionType::EFI_SYSTEM_PARTITION {
        return Err("Destination GPT type is not EFI system partition.");
    }
    let guid = entry.unique_partition_guid;
    let label = format!("EFI partition {guid}");
    drop(partition);
    Ok(label)
}

pub fn targets() -> Result<Vec<Target>, &'static str> {
    let source = source_device()?;
    let handles = boot::find_handles::<SimpleFileSystem>()
        .map_err(|_| "Firmware could not enumerate filesystem destinations.")?;
    let mut targets = Vec::new();
    for id in handles {
        let Ok(label) = target_label(id, source) else { continue };
        let Ok(mut protocol) = boot::open_protocol_exclusive::<SimpleFileSystem>(id) else {
            continue;
        };
        let mut root =
            protocol.open_volume().map_err(|_| "An EFI destination filesystem could not be opened.")?;
        let info = root
            .get_boxed_info::<FileSystemInfo>()
            .map_err(|_| "An EFI destination filesystem could not be inspected.")?;
        if !info.read_only() {
            targets.push(Target { id, label });
        }
    }
    Ok(targets)
}

fn exists(root: &mut Directory, path: &CStr16) -> Result<bool, &'static str> {
    match root.open(path, FileMode::Read, FileAttribute::empty()) {
        Ok(_) => Ok(true),
        Err(error) if error.status() == Status::NOT_FOUND => Ok(false),
        Err(_) => Err("Destination paths could not be checked; installation stopped."),
    }
}

fn check_parent(root: &mut Directory, path: &CStr16) -> Result<(), &'static str> {
    if exists(root, path)? {
        let file = root
            .open(path, FileMode::Read, FileAttribute::empty())
            .map_err(|_| "Destination parent directory is unreadable.")?;
        if !file.is_directory().map_err(|_| "Cannot inspect destination parent directory.")? {
            return Err("A destination parent path is a file, not a directory.");
        }
    }
    Ok(())
}

fn create_directory(root: &mut Directory, path: &CStr16) -> Result<(), &'static str> {
    if !exists(root, path)? {
        let mut directory = root
            .open(path, FileMode::CreateReadWrite, FileAttribute::DIRECTORY)
            .map_err(|_| "Could not create destination directory; installation is incomplete.")?
            .into_directory()
            .ok_or("Destination directory creation returned a file.")?;
        directory.flush().map_err(|_| "Destination directory flush failed; installation is incomplete.")?;
    }
    Ok(())
}

fn required_space(contents: &[Vec<u8>], allocation_unit: u32) -> Result<u64, &'static str> {
    let unit = u64::from(allocation_unit);
    if unit == 0 {
        return Err("Destination reported an invalid filesystem allocation size.");
    }
    let mut required = unit.checked_mul(8).ok_or("Destination allocation size is too large.")?;
    for content in contents {
        let size = u64::try_from(content.len()).map_err(|_| "Installer source size is too large.")?;
        let allocated =
            size.div_ceil(unit).checked_mul(unit).ok_or("Installer allocation size is too large.")?;
        required = required.checked_add(allocated).ok_or("Installer allocation size is too large.")?;
    }
    Ok(required)
}

/// Copy only after the caller obtains explicit destination confirmation.
/// A failed write may leave newly created files; no existing file is deleted.
pub fn install(target: Handle) -> Result<Summary, &'static str> {
    target_label(target, source_device()?)?;
    let source = boot::get_image_file_system(boot::image_handle())
        .map_err(|_| "Cannot open the installer source filesystem.")?;
    let mut source = FileSystem::new(source);
    let mut contents = Vec::with_capacity(PATHS.len());
    let mut bytes = 0usize;
    for path in PATHS {
        let content = source
            .read(path)
            .map_err(|_| "Required installer source file is missing or unreadable; nothing was copied.")?;
        if content.is_empty() {
            return Err("Required installer source file is empty; nothing was copied.");
        }
        bytes = bytes.checked_add(content.len()).ok_or("Installer source size is too large.")?;
        contents.push(content);
    }
    drop(source);
    let mut protocol = boot::open_protocol_exclusive::<SimpleFileSystem>(target)
        .map_err(|_| "Cannot open destination filesystem.")?;
    let mut root = protocol.open_volume().map_err(|_| "Cannot read destination filesystem.")?;
    let info = root
        .get_boxed_info::<FileSystemInfo>()
        .map_err(|_| "Cannot inspect destination filesystem capacity.")?;
    if info.read_only() {
        return Err("Destination filesystem is read-only; nothing was copied.");
    }
    let required = required_space(&contents, info.block_size())?;
    if info.free_space() < required {
        return Err("Destination has insufficient free space; nothing was copied.");
    }
    if exists(&mut root, SPACEOS)? || exists(&mut root, BOOT)? {
        return Err(
            "Destination already contains EFI\\SPACEOS or BOOTX64.EFI; existing files are protected.",
        );
    }
    check_parent(&mut root, EFI)?;
    check_parent(&mut root, BOOT_DIR)?;
    for path in [EFI, BOOT_DIR, SPACEOS] {
        create_directory(&mut root, path)?;
    }
    for (path, content) in PATHS.into_iter().zip(&contents) {
        if exists(&mut root, path)? {
            return Err("Destination file appeared during installation; existing files are protected.");
        }
        let mut file = root
            .open(path, FileMode::CreateReadWrite, FileAttribute::empty())
            .map_err(|_| "Destination file creation failed; installation is incomplete.")?
            .into_regular_file()
            .ok_or("Destination path is not a regular file.")?;
        file.write(content).map_err(|_| "Destination write failed; installation is incomplete.")?;
        let expected = u64::try_from(content.len()).map_err(|_| "Installer source size is too large.")?;
        let position = file
            .get_position()
            .map_err(|_| "Cannot verify destination write length; installation is incomplete.")?;
        let written = file
            .get_boxed_info::<FileInfo>()
            .map_err(|_| "Cannot verify destination file size; installation is incomplete.")?;
        if position != expected || written.file_size() != expected {
            return Err("Destination write was incomplete; installation is incomplete.");
        }
        file.flush().map_err(|_| "Destination file flush failed; installation is incomplete.")?;
    }
    root.flush().map_err(|_| "Destination directory flush failed; installation is incomplete.")?;
    drop(root);
    drop(protocol);
    Ok(Summary { files: PATHS.len(), bytes })
}
