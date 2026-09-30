//! `cargo xtask` – build, image, run and test driver for Space OS.
//!
//! Subcommands:
//!   build            cross-compile bootloader, kernel, user programs; build the ESP image
//!   run [--gui]      boot the image in QEMU (serial on stdio)
//!   test             boot scenarios and check the serial log + exit code
//!   soak [--boots N] N consecutive cold boots of the acceptance scenario (default 100)
//!   stress [--minutes N] the acceptance suite over and over in one boot, with random
//!                    kills between passes, for N minutes (default 480; ADR-0019)
//!   clippy | fmt | fmt-check | ci
//!
//! Environment: `SPACEOS_OVMF_CODE` / `SPACEOS_OVMF_VARS` override firmware discovery,
//! `SPACEOS_QEMU` overrides the QEMU binary, `SPACEOS_PCAP=<file>` records every
//! frame on the guest's network during `run` (test runs always record, next to the
//! serial log).

mod cloud;
mod lab;
mod nofpu;
mod pcap;
mod pki;
mod reference;

use std::fs;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const USER_PROGRAMS: &[&str] = &[
    "init",
    "hello",
    "fault",
    "abi_negative",
    "ipc_echo",
    "quota",
    "spin",
    "pingpong",
    "worker",
    "blocker",
    "spacecompute",
    "spaceai",
    "spaceshell",
    "uiworker",
    "spacebroker",
    "spaceagent",
    "spacelink",
    "spacepkg",
    "spaceterm",
    "spacenet",
    "tlsprobe",
    "spacecloud",
    "churn",
    "spacedesk",
    "deskapps",
];
const ESP_SIZE: u64 = 64 * 1024 * 1024;
/// Guest data disk (virtio-blk): holds the model and its manifest.
const DATA_SIZE: u64 = 64 * 1024 * 1024;
const MODEL_PATH: &str = "/spaceos/model.slm";
const PART_START: u64 = 1024 * 1024;
const BOOT_TIMEOUT: Duration = Duration::from_secs(240);

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap().to_path_buf()
}

fn sh(cmd: &mut Command) -> Result<(), String> {
    let desc = format!("{cmd:?}");
    let status = cmd.status().map_err(|e| format!("cannot run {desc}: {e}"))?;
    if status.success() { Ok(()) } else { Err(format!("command failed ({status}): {desc}")) }
}

fn cargo() -> Command {
    let mut c = Command::new(std::env::var("CARGO").unwrap_or_else(|_| "cargo".into()));
    c.current_dir(root());
    c
}

/// A `-p` argument naming one of our packages unambiguously: a dependency may share
/// the name (the `spin` test program and the `spin` crate under `rsa`), never the
/// workspace version.
fn package_spec(name: &str) -> String {
    format!("{name}@{}", env!("CARGO_PKG_VERSION"))
}

struct Built {
    boot_efi: PathBuf,
    kernel_elf: PathBuf,
    user_bins: Vec<(String, PathBuf)>,
}

fn build(profile_release: bool) -> Result<Built, String> {
    let r = root();
    let prof = if profile_release { "release" } else { "dev" };
    let dir = if profile_release { "release" } else { "debug" };
    println!("== building spaceboot (x86_64-unknown-uefi, {prof})");
    sh(cargo().args([
        "build",
        "--profile",
        prof,
        "-p",
        "spaceboot",
        "--target",
        "x86_64-unknown-uefi",
        "--target-dir",
        "target/boot",
    ]))?;
    println!("== building spacekernel (x86_64-unknown-none, {prof})");
    sh(cargo().args([
        "build",
        "--profile",
        prof,
        "-p",
        "spacekernel",
        "--target",
        "x86_64-unknown-none",
        "--target-dir",
        "target/kernel",
    ]))?;
    println!("== building user programs (x86_64-unknown-none, {prof})");
    let mut c = cargo();
    c.args(["build", "--profile", prof, "--target", "x86_64-unknown-none", "--target-dir", "target/user"]);
    for p in USER_PROGRAMS {
        c.args(["-p", &package_spec(p)]);
    }
    sh(&mut c)?;
    let built = Built {
        boot_efi: r.join(format!("target/boot/x86_64-unknown-uefi/{dir}/spaceboot.efi")),
        kernel_elf: r.join(format!("target/kernel/x86_64-unknown-none/{dir}/spacekernel")),
        user_bins: USER_PROGRAMS
            .iter()
            .map(|p| (p.to_string(), r.join(format!("target/user/x86_64-unknown-none/{dir}/{p}"))))
            .collect(),
    };
    // The kernel runs with the FPU and vector units off: an image that could reach
    // one of their instructions is not built at all.
    let mut total = 0;
    for (name, path) in [("spacekernel", &built.kernel_elf)]
        .into_iter()
        .chain(built.user_bins.iter().map(|(n, p)| (n.as_str(), p)))
    {
        let elf = fs::read(path).map_err(|e| format!("read {}: {e}", path.display()))?;
        total += nofpu::check(name, &elf)?;
    }
    println!(
        "== no x87/MMX/SSE/AVX instructions in the kernel and {} programs, besides the two `fault` runs on purpose \
         ({total} instructions decoded)",
        built.user_bins.len()
    );
    Ok(built)
}

/// `llvm-objcopy` from the pinned toolchain (`llvm-tools` component), if present.
fn llvm_objcopy() -> Option<PathBuf> {
    let out = Command::new("rustc").arg("--print").arg("sysroot").output().ok()?;
    let sysroot = String::from_utf8(out.stdout).ok()?;
    let host = std::env::var("HOST_TRIPLE").unwrap_or_else(|_| "x86_64-unknown-linux-gnu".into());
    let p = Path::new(sysroot.trim()).join("lib/rustlib").join(host).join("bin/llvm-objcopy");
    p.exists().then_some(p)
}

/// Strip symbols/debug info from an ELF for the boot image; the unstripped file stays
/// in `target/` for `addr2line`. Falls back to the original bytes without the tool.
fn stripped(path: &Path) -> Result<Vec<u8>, String> {
    if let Some(objcopy) = llvm_objcopy() {
        let out = root().join("build/strip").join(path.file_name().unwrap());
        fs::create_dir_all(out.parent().unwrap()).map_err(|e| e.to_string())?;
        if sh(Command::new(objcopy).arg("--strip-all").arg(path).arg(&out)).is_ok() {
            return fs::read(&out).map_err(|e| e.to_string());
        }
    }
    fs::read(path).map_err(|e| format!("read {}: {e}", path.display()))
}

/// Deliberately broken executables the kernel must reject without crashing.
fn bad_elf_fixtures(hello: &[u8]) -> Vec<(String, Vec<u8>)> {
    let mut bad_entry = hello.to_vec();
    // e_entry at offset 24: a non-canonical address outside every segment.
    bad_entry[24..32].copy_from_slice(&0x8000_0000_0000u64.to_le_bytes());
    let mut bad_magic = hello.to_vec();
    bad_magic[1] = b'X';
    let truncated = hello[..hello.len().min(600)].to_vec();
    let mut huge_segment = hello.to_vec();
    // First program header at e_phoff (offset 32); p_memsz at +40: claim 8 GiB.
    let phoff = u64::from_le_bytes(hello[32..40].try_into().unwrap()) as usize;
    huge_segment[phoff + 40..phoff + 48].copy_from_slice(&(8u64 << 30).to_le_bytes());
    vec![
        ("bad_entry".to_string(), bad_entry),
        ("bad_magic".to_string(), bad_magic),
        ("truncated".to_string(), truncated),
        ("huge_segment".to_string(), huge_segment),
    ]
}

fn make_initrd(built: &Built) -> Result<Vec<u8>, String> {
    let mut b = tar::Builder::new(Vec::new());
    let mut extra: Vec<(String, Vec<u8>)> = Vec::new();
    for (name, path) in &built.user_bins {
        let data = stripped(path)?;
        if name == "hello" {
            extra = bad_elf_fixtures(&data);
        }
        let mut h = tar::Header::new_ustar();
        h.set_size(data.len() as u64);
        h.set_mode(0o755);
        h.set_entry_type(tar::EntryType::Regular);
        h.set_mtime(0);
        h.set_cksum();
        b.append_data(&mut h, format!("bin/{name}"), &data[..]).map_err(|e| format!("tar {name}: {e}"))?;
    }
    for (name, data) in extra {
        let mut h = tar::Header::new_ustar();
        h.set_size(data.len() as u64);
        h.set_mode(0o644);
        h.set_entry_type(tar::EntryType::Regular);
        h.set_mtime(0);
        h.set_cksum();
        b.append_data(&mut h, format!("fixtures/{name}"), &data[..])
            .map_err(|e| format!("tar {name}: {e}"))?;
    }
    b.into_inner().map_err(|e| format!("tar finish: {e}"))
}

/// Write an MBR with one EFI System partition covering [PART_START, ESP_SIZE).
fn write_mbr(f: &mut fs::File) -> Result<(), String> {
    let mut mbr = [0u8; 512];
    let start_lba = (PART_START / 512) as u32;
    let sectors = ((ESP_SIZE - PART_START) / 512) as u32;
    let e = &mut mbr[446..462];
    e[0] = 0x80; // bootable
    e[1..4].copy_from_slice(&[0xFE, 0xFF, 0xFF]); // CHS start (LBA-only)
    e[4] = 0xEF; // EFI System Partition
    e[5..8].copy_from_slice(&[0xFE, 0xFF, 0xFF]);
    e[8..12].copy_from_slice(&start_lba.to_le_bytes());
    e[12..16].copy_from_slice(&sectors.to_le_bytes());
    mbr[510] = 0x55;
    mbr[511] = 0xAA;
    f.seek(SeekFrom::Start(0)).map_err(|e| e.to_string())?;
    f.write_all(&mbr).map_err(|e| e.to_string())
}

fn make_image(built: &Built, cmdline: &str, out: &Path) -> Result<(), String> {
    fs::create_dir_all(out.parent().unwrap()).map_err(|e| e.to_string())?;
    let initrd = make_initrd(built)?;
    let boot = fs::read(&built.boot_efi).map_err(|e| format!("read bootloader: {e}"))?;
    let kernel = stripped(&built.kernel_elf)?;
    let cfg = format!("# Space OS boot configuration (read by spaceboot)\ncmdline={cmdline}\n");

    let mut f = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .open(out)
        .map_err(|e| e.to_string())?;
    f.set_len(ESP_SIZE).map_err(|e| e.to_string())?;
    write_mbr(&mut f)?;
    let part = fscommon::StreamSlice::new(f, PART_START, ESP_SIZE).map_err(|e| e.to_string())?;
    let mut part = fscommon::BufStream::new(part);
    fatfs::format_volume(
        &mut part,
        fatfs::FormatVolumeOptions::new().fat_type(fatfs::FatType::Fat32).volume_label(*b"SPACEOS    "),
    )
    .map_err(|e| format!("format: {e}"))?;
    {
        let fs =
            fatfs::FileSystem::new(&mut part, fatfs::FsOptions::new()).map_err(|e| format!("mount: {e}"))?;
        let rootdir = fs.root_dir();
        let efi = rootdir.create_dir("EFI").map_err(|e| e.to_string())?;
        let bootdir = efi.create_dir("BOOT").map_err(|e| e.to_string())?;
        bootdir
            .create_file("BOOTX64.EFI")
            .map_err(|e| e.to_string())?
            .write_all(&boot)
            .map_err(|e| e.to_string())?;
        let sp = efi.create_dir("SPACEOS").map_err(|e| e.to_string())?;
        sp.create_file("spacekernel.elf")
            .map_err(|e| e.to_string())?
            .write_all(&kernel)
            .map_err(|e| e.to_string())?;
        sp.create_file("initrd.tar")
            .map_err(|e| e.to_string())?
            .write_all(&initrd)
            .map_err(|e| e.to_string())?;
        sp.create_file("spaceos.cfg")
            .map_err(|e| e.to_string())?
            .write_all(cfg.as_bytes())
            .map_err(|e| e.to_string())?;
    }
    part.flush().map_err(|e| e.to_string())?;
    println!(
        "== image {} ({} KiB bootloader, {} KiB kernel, {} KiB initrd, cmdline {cmdline:?})",
        out.display(),
        boot.len() / 1024,
        kernel.len() / 1024,
        initrd.len() / 1024
    );
    Ok(())
}

/// Build the reference model, its manifest and the pinned inference baseline.
///
/// The baseline is produced by the host reference implementation in
/// [`reference`], which shares its math and tensor layout with the guest, so the
/// guest must reproduce the token sequence exactly.
fn build_model() -> (Vec<u8>, String, String) {
    let header = reference::config();
    let weights = reference::weights(&header);
    let bytes = reference::serialise(&header, &weights);
    let start = Instant::now();
    let tokens = reference::generate(&header, &weights, reference::PROMPT, reference::GENERATE);
    let elapsed = start.elapsed();
    let distinct = {
        let mut seen: Vec<u32> = tokens.clone();
        seen.sort_unstable();
        seen.dedup();
        seen.len()
    };
    let prompt_hex: String = reference::PROMPT.iter().map(|b| format!("{b:02x}")).collect();
    let token_list: Vec<String> = tokens.iter().map(|t| t.to_string()).collect();
    let baseline = format!(
        "# Space OS inference baseline (host reference, pinned)\nmodel={MODEL_PATH}\nprompt_hex={prompt_hex}\ngenerate={}\ndistinct={distinct}\ntokens={}\n",
        reference::GENERATE,
        token_list.join(",")
    );
    println!(
        "== model: {} layers, d_model {}, {} params ({} KiB); baseline {} tokens, {distinct} distinct, host {:.0} ms",
        header.n_layers,
        header.d_model,
        header.weight_floats(),
        bytes.len() / 1024,
        tokens.len(),
        elapsed.as_secs_f64() * 1000.0
    );
    let digest = sha256_hex(&bytes);
    (bytes, baseline, digest)
}

/// Deliberately broken models the runtime must reject without crashing.
fn bad_models(good: &[u8]) -> Vec<(&'static str, Vec<u8>)> {
    let mut bad_magic = good.to_vec();
    bad_magic[1] = b'X';
    let mut bad_dims = good.to_vec();
    // d_model = 4096: beyond MAX_D_MODEL and inconsistent with heads * head_dim.
    bad_dims[16..20].copy_from_slice(&4096u32.to_le_bytes());
    let truncated = good[..good.len() / 2].to_vec();
    vec![("BADMAGIC.SLM", bad_magic), ("BADDIMS.SLM", bad_dims), ("TRUNC.SLM", truncated)]
}

fn sha256_hex(data: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(data);
    h.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

/// Build the guest data disk: a bare FAT32 volume (no partition table) with the
/// model and a manifest naming its size and SHA-256.
fn make_data_disk(out: &Path) -> Result<(), String> {
    let (model, baseline, digest) = build_model();
    let manifest =
        format!("# Space OS model manifest\npath={MODEL_PATH}\nsize={}\nsha256={digest}\n", model.len());
    fs::create_dir_all(out.parent().unwrap()).map_err(|e| e.to_string())?;
    let f = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .open(out)
        .map_err(|e| e.to_string())?;
    f.set_len(DATA_SIZE).map_err(|e| e.to_string())?;
    let mut disk = fscommon::BufStream::new(f);
    fatfs::format_volume(
        &mut disk,
        fatfs::FormatVolumeOptions::new().fat_type(fatfs::FatType::Fat32).volume_label(*b"SPACEDATA  "),
    )
    .map_err(|e| format!("format data disk: {e}"))?;
    {
        let fs = fatfs::FileSystem::new(&mut disk, fatfs::FsOptions::new()).map_err(|e| e.to_string())?;
        let dir = fs.root_dir().create_dir("SPACEOS").map_err(|e| e.to_string())?;
        dir.create_file("MODEL.SLM")
            .map_err(|e| e.to_string())?
            .write_all(&model)
            .map_err(|e| e.to_string())?;
        dir.create_file("MANIFEST.TXT")
            .map_err(|e| e.to_string())?
            .write_all(manifest.as_bytes())
            .map_err(|e| e.to_string())?;
        dir.create_file("BASELINE.TXT")
            .map_err(|e| e.to_string())?
            .write_all(baseline.as_bytes())
            .map_err(|e| e.to_string())?;
        for (name, data) in bad_models(&model) {
            dir.create_file(name).map_err(|e| e.to_string())?.write_all(&data).map_err(|e| e.to_string())?;
        }
        // Somewhere for the guest to write. Empty on purpose: everything in it was
        // put there by the guest itself, so a file found here on the second boot is
        // a file the volume remembered.
        dir.create_dir("VAR").map_err(|e| e.to_string())?;
        // Agent workspace (G01): the instruction, the input, and the file the patch
        // is checked against. Kept tiny and deterministic so the check is exact.
        let ws = dir.create_dir("WS").map_err(|e| e.to_string())?;
        for (name, body) in workspace_files() {
            ws.create_file(name)
                .map_err(|e| e.to_string())?
                .write_all(body.as_bytes())
                .map_err(|e| e.to_string())?;
        }
        // SpaceLink corpus (L01-L03): four short documents whose vocabulary is
        // disjoint enough that a query has one obvious answer.
        let docs = dir.create_dir("DOCS").map_err(|e| e.to_string())?;
        for (name, body) in corpus_files() {
            docs.create_file(name)
                .map_err(|e| e.to_string())?
                .write_all(body.as_bytes())
                .map_err(|e| e.to_string())?;
        }
        // The one certificate authority the guest's TLS trusts (ADR-0017), made
        // fresh with the leaves the lab's TLS services present.
        let tls = dir.create_dir("TLS").map_err(|e| e.to_string())?;
        tls.create_file("LABCA.DER")
            .map_err(|e| e.to_string())?
            .write_all(&pki::generate()?)
            .map_err(|e| e.to_string())?;
        // The credential store (ADR-0018): the key the lab's cloud provider accepts.
        // Outside the workspace, so no tool call can reach it.
        dir.create_dir("CRED")
            .map_err(|e| e.to_string())?
            .create_file("CLOUD.KEY")
            .map_err(|e| e.to_string())?
            .write_all(pki::api_key()?.as_bytes())
            .map_err(|e| e.to_string())?;
        // Package fixtures (P01): two good versions and three that must be refused,
        // each for a different reason.
        let pkgs = dir.create_dir("PKG").map_err(|e| e.to_string())?;
        for (name, image) in package_files()? {
            pkgs.create_file(name)
                .map_err(|e| e.to_string())?
                .write_all(&image)
                .map_err(|e| e.to_string())?;
        }
    }
    disk.flush().map_err(|e| e.to_string())?;
    println!(
        "== data disk {} ({} KiB model + baseline + 3 malformed fixtures + agent workspace + link corpus + lab CA + cloud credential + 5 packages, FAT32)",
        out.display(),
        model.len() / 1024
    );
    Ok(())
}

/// CRC-32 (IEEE 802.3), the checksum GPT headers and partition tables carry.
fn crc32(data: &[u8]) -> u32 {
    let mut crc = u32::MAX;
    for &b in data {
        crc ^= u32::from(b);
        for _ in 0..8 {
            crc = if crc & 1 != 0 { (crc >> 1) ^ 0xEDB8_8320 } else { crc >> 1 };
        }
    }
    !crc
}

/// A GPT data disk laid out the way an installed system's disk is: partition 1 an
/// EFI system partition with a FAT32 volume labelled `SPACEOS` (standing in for
/// the boot files), partition 2 a copy of the data volume in `volume`. Both GPT
/// copies, both checksums: firmware and host tools read it as a valid disk.
fn make_data_disk_gpt(volume: &Path, out: &Path) -> Result<(), String> {
    const SECTOR: u64 = 512;
    const ENTRIES: u64 = 128;
    const ENTRY_SIZE: u64 = 128;
    const TABLE_SECTORS: u64 = ENTRIES * ENTRY_SIZE / SECTOR;
    /// Partition types, in GPT's mixed-endian byte order.
    const EFI_SYSTEM: [u8; 16] =
        [0x28, 0x73, 0x2A, 0xC1, 0x1F, 0xF8, 0xD2, 0x11, 0xBA, 0x4B, 0x00, 0xA0, 0xC9, 0x3E, 0xC9, 0x3B];
    const BASIC_DATA: [u8; 16] =
        [0xA2, 0xA0, 0xD0, 0xEB, 0xE5, 0xB9, 0x33, 0x44, 0x87, 0xC0, 0x68, 0xB6, 0xB7, 0x26, 0x99, 0xC7];

    let data = fs::read(volume).map_err(|e| format!("read {}: {e}", volume.display()))?;
    let esp_sectors = ESP_SIZE / SECTOR;
    let data_sectors = (data.len() as u64).div_ceil(SECTOR);
    let esp_start = PART_START / SECTOR;
    let data_start = esp_start + esp_sectors;
    // A mebibyte after the last partition holds the backup table and header.
    let total = data_start + data_sectors + PART_START / SECTOR;
    let last = total - 1;
    let backup_table = last - TABLE_SECTORS;

    let mut f = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .open(out)
        .map_err(|e| e.to_string())?;
    f.set_len(total * SECTOR).map_err(|e| e.to_string())?;

    // Protective MBR: one partition of type 0xEE over the whole disk.
    let mut mbr = [0u8; 512];
    mbr[446 + 4] = 0xEE;
    mbr[446 + 8..446 + 12].copy_from_slice(&1u32.to_le_bytes());
    mbr[446 + 12..446 + 16].copy_from_slice(&u32::try_from(last).unwrap_or(u32::MAX).to_le_bytes());
    mbr[510] = 0x55;
    mbr[511] = 0xAA;

    let mut table = vec![0u8; (ENTRIES * ENTRY_SIZE) as usize];
    let parts = [
        (EFI_SYSTEM, esp_start, esp_start + esp_sectors - 1, "EFI system"),
        (BASIC_DATA, data_start, data_start + data_sectors - 1, "Space OS data"),
    ];
    for (i, (kind, first, end, name)) in parts.iter().enumerate() {
        let e = &mut table[i * ENTRY_SIZE as usize..(i + 1) * ENTRY_SIZE as usize];
        e[0..16].copy_from_slice(kind);
        // Unique partition GUID: fixed, so the image is the same on every build.
        let mut guid = *b"SPACEOS-PART-00\0";
        guid[15] = i as u8 + 1;
        e[16..32].copy_from_slice(&guid);
        e[32..40].copy_from_slice(&first.to_le_bytes());
        e[40..48].copy_from_slice(&end.to_le_bytes());
        for (j, unit) in name.encode_utf16().enumerate() {
            e[56 + j * 2..58 + j * 2].copy_from_slice(&unit.to_le_bytes());
        }
    }
    let table_crc = crc32(&table);
    let header = |mine: u64, other: u64, entries_at: u64| {
        let mut h = [0u8; 512];
        h[0..8].copy_from_slice(b"EFI PART");
        h[8..12].copy_from_slice(&0x0001_0000u32.to_le_bytes());
        h[12..16].copy_from_slice(&92u32.to_le_bytes());
        h[24..32].copy_from_slice(&mine.to_le_bytes());
        h[32..40].copy_from_slice(&other.to_le_bytes());
        h[40..48].copy_from_slice(&(2 + TABLE_SECTORS).to_le_bytes());
        h[48..56].copy_from_slice(&(backup_table - 1).to_le_bytes());
        h[56..72].copy_from_slice(b"SPACEOS-DATADISK");
        h[72..80].copy_from_slice(&entries_at.to_le_bytes());
        h[80..84].copy_from_slice(&(ENTRIES as u32).to_le_bytes());
        h[84..88].copy_from_slice(&(ENTRY_SIZE as u32).to_le_bytes());
        h[88..92].copy_from_slice(&table_crc.to_le_bytes());
        let crc = crc32(&h[0..92]);
        h[16..20].copy_from_slice(&crc.to_le_bytes());
        h
    };
    let mut put = |at: u64, bytes: &[u8]| -> Result<(), String> {
        f.seek(SeekFrom::Start(at * SECTOR)).map_err(|e| e.to_string())?;
        f.write_all(bytes).map_err(|e| e.to_string())
    };
    put(0, &mbr)?;
    put(1, &header(1, last, 2))?;
    put(2, &table)?;
    put(backup_table, &table)?;
    put(last, &header(last, 1, backup_table))?;
    put(data_start, &data)?;

    // The EFI system partition gets a FAT32 volume with a label of its own: were
    // the label not what picks the data volume, this one, first on the disk, would be.
    let part = fscommon::StreamSlice::new(f, esp_start * SECTOR, (esp_start + esp_sectors) * SECTOR)
        .map_err(|e| e.to_string())?;
    let mut part = fscommon::BufStream::new(part);
    fatfs::format_volume(
        &mut part,
        fatfs::FormatVolumeOptions::new().fat_type(fatfs::FatType::Fat32).volume_label(*b"SPACEOS    "),
    )
    .map_err(|e| format!("format the EFI system partition: {e}"))?;
    part.flush().map_err(|e| e.to_string())?;
    println!(
        "== data disk {} (GPT: EFI system partition, {} MiB; data volume, {} MiB)",
        out.display(),
        ESP_SIZE >> 20,
        (data_sectors * SECTOR) >> 20
    );
    Ok(())
}

/// Files the agent workspace starts with. `expect.txt` is `input.txt` with the rule
/// in `task.txt` applied, so the broker's check compares against something the host
/// computed, not something the guest produced.
fn workspace_files() -> [(&'static str, String); 3] {
    const RULE: &str = "TODO => DONE";
    const INPUT: &str = "space-os workspace\nline 1: TODO review the boot path\nline 2: ok\nline 3: TODO write the ADR\nline 4: TODO and TODO again\n";
    let expect = INPUT.replace("TODO", "DONE");
    [
        ("TASK.TXT", format!("# agent task\n# apply this rule to input.txt and write output.txt\n{RULE}\n")),
        ("INPUT.TXT", String::from(INPUT)),
        ("EXPECT.TXT", expect),
    ]
}

/// The SpaceLink corpus. `SECRET.TXT` exists to be revoked: `embargo` appears
/// nowhere else, so a query for it proves the document is really gone rather than
/// merely ranked lower.
fn corpus_files() -> [(&'static str, &'static str); 4] {
    [
        (
            "BOOT.TXT",
            "Space OS boot path\n\
             The bootloader spaceboot runs under UEFI, loads the kernel image and the initrd,\n\
             builds the page tables and exits boot services before jumping to the kernel.\n\
             The linear map covers RAM only; device MMIO is mapped uncached on demand.\n",
        ),
        (
            "IPC.TXT",
            "Space OS inter-process communication\n\
             A channel carries fixed-size messages and at most one handle per message.\n\
             Sending a handle over a channel transfers it: the sender loses the handle.\n\
             A channel endpoint closes when its last handle is closed, and the peer sees it.\n",
        ),
        (
            "MEMORY.TXT",
            "Space OS memory model\n\
             Each process has its own address space and a page quota it cannot exceed.\n\
             A memory object holds frames several processes can map at once.\n\
             Frames are freed when the object and every mapping of it are gone.\n",
        ),
        (
            "SECRET.TXT",
            "Space OS embargo notes\n\
             This document is under embargo and must not reach a context bundle.\n\
             It mentions a channel and a quota so that revoking it is visible in results.\n",
        ),
    ]
}

/// Package images for P01. The host signs them with the same shared implementation
/// the guest verifies with (`spaceabi::pkg`), which is what makes the three bad
/// packages meaningful: each one differs from a good package in exactly one way.
fn package_files() -> Result<Vec<(&'static str, Vec<u8>)>, String> {
    use spaceabi::pkg;

    fn build(name: &str, version: u32, payload: &[u8], key: &[u8]) -> Result<Vec<u8>, String> {
        let mut out = vec![0u8; core::mem::size_of::<pkg::Header>() + payload.len()];
        let n = pkg::build(name, version, payload, key, &mut out).ok_or("cannot build package")?;
        out.truncate(n);
        Ok(out)
    }

    let v1 = b"space-os demo package\nversion 1: the first release\n";
    let v2 = b"space-os demo package\nversion 2: adds a line nobody asked for\n";
    let good1 = build("demo", 1, v1, pkg::RELEASE_KEY)?;
    let good2 = build("demo", 2, v2, pkg::RELEASE_KEY)?;

    // Same header, one payload byte flipped: the MAC still checks out because it
    // covers the header, so this must be caught by the payload digest.
    let mut tampered = build("demo", 3, b"space-os demo package\nversion 3: tampered\n", pkg::RELEASE_KEY)?;
    let last = tampered.len() - 1;
    tampered[last] ^= 0x01;

    // Correctly built, but signed with a key this build does not trust.
    let forged = build("demo", 4, b"space-os demo package\nversion 4: forged\n", b"not-the-release-key")?;

    // Cut short after the header: the payload the header promises is not there.
    let mut truncated = build("demo", 5, b"space-os demo package\nversion 5: truncated\n", pkg::RELEASE_KEY)?;
    truncated.truncate(core::mem::size_of::<pkg::Header>() + 4);

    Ok(vec![
        ("DEMO1.SPK", good1),
        ("DEMO2.SPK", good2),
        ("BADPAY.SPK", tampered),
        ("FORGED.SPK", forged),
        ("TRUNC.SPK", truncated),
    ])
}

fn find_firmware() -> Result<(PathBuf, PathBuf), String> {
    if let (Ok(c), Ok(v)) = (std::env::var("SPACEOS_OVMF_CODE"), std::env::var("SPACEOS_OVMF_VARS")) {
        return Ok((c.into(), v.into()));
    }
    let candidates = [
        ("/usr/share/OVMF/OVMF_CODE_4M.fd", "/usr/share/OVMF/OVMF_VARS_4M.fd"),
        ("/usr/share/OVMF/OVMF_CODE.fd", "/usr/share/OVMF/OVMF_VARS.fd"),
        ("/usr/share/edk2/x64/OVMF_CODE.4m.fd", "/usr/share/edk2/x64/OVMF_VARS.4m.fd"),
        ("/usr/share/edk2/x64/OVMF_CODE.fd", "/usr/share/edk2/x64/OVMF_VARS.fd"),
        ("/usr/share/edk2-ovmf/x64/OVMF_CODE.fd", "/usr/share/edk2-ovmf/x64/OVMF_VARS.fd"),
        ("/usr/share/qemu/OVMF_CODE.fd", "/usr/share/qemu/OVMF_VARS.fd"),
        ("/opt/homebrew/share/qemu/edk2-x86_64-code.fd", "/opt/homebrew/share/qemu/edk2-i386-vars.fd"),
    ];
    for (c, v) in candidates {
        if Path::new(c).exists() && Path::new(v).exists() {
            return Ok((c.into(), v.into()));
        }
    }
    Err("OVMF firmware not found; install `ovmf` or set SPACEOS_OVMF_CODE/SPACEOS_OVMF_VARS".into())
}

struct QemuRun {
    log: String,
    exit_code: Option<i32>,
    elapsed: Duration,
    timed_out: bool,
    /// The typist's own failure, if it had one. Kept apart from the guest log: a
    /// harness that could not type is not a guest that stopped answering, and
    /// reporting the first as the second sends the reader looking in the wrong place.
    input_error: Option<String>,
    /// The guest's TCP as captured on the lab network, for machines with a card.
    tcp: Option<Result<pcap::TcpReport, String>>,
    /// What the lab services wrote about their connections (empty if nothing).
    lab_log: String,
    /// What the emulator itself used, when the boot was watched for it.
    host: Option<HostSamples>,
}

/// The emulator seen from the host, sampled through a long run: a guest that is
/// fine while the machine around it slowly runs out of descriptors is not fine.
#[derive(Default, Clone, Copy)]
struct HostSamples {
    samples: u32,
    fds_max: usize,
    fds_last: usize,
    rss_kib_max: u64,
    rss_kib_last: u64,
    /// Lab services alive at once (each connection is one).
    lab_max: usize,
    lab_last: usize,
}

impl HostSamples {
    fn take(&mut self, qemu_pid: u32, lab_exe: &Path) {
        let fds = fs::read_dir(format!("/proc/{qemu_pid}/fd")).map(|d| d.count()).unwrap_or(0);
        let rss = fs::read_to_string(format!("/proc/{qemu_pid}/status"))
            .ok()
            .and_then(|s| {
                s.lines()
                    .find_map(|l| l.strip_prefix("VmRSS:"))
                    .and_then(|v| v.trim().trim_end_matches("kB").trim().parse().ok())
            })
            .unwrap_or(0);
        let exe = lab_exe.as_os_str().as_encoded_bytes();
        let lab = fs::read_dir("/proc")
            .map(|d| {
                d.filter_map(Result::ok)
                    .filter(|e| e.file_name().to_string_lossy().bytes().all(|b| b.is_ascii_digit()))
                    .filter(|e| {
                        fs::read(e.path().join("cmdline")).is_ok_and(|c| {
                            let mut args = c.split(|&b| b == 0);
                            args.next() == Some(exe) && args.next() == Some(b"lab")
                        })
                    })
                    .count()
            })
            .unwrap_or(0);
        self.samples += 1;
        self.fds_max = self.fds_max.max(fds);
        self.fds_last = fds;
        self.rss_kib_max = self.rss_kib_max.max(rss);
        self.rss_kib_last = rss;
        self.lab_max = self.lab_max.max(lab);
        self.lab_last = lab;
    }
}

/// The serial log read as it grows: only what is new each time, whole lines only.
/// A long run's log reaches a hundred megabytes; reading all of it ten times a
/// second would be most of what the harness does.
#[derive(Default)]
struct LogTail {
    offset: u64,
    partial: Vec<u8>,
}

impl LogTail {
    /// Complete lines added since the last call.
    fn lines(&mut self, path: &Path) -> Vec<String> {
        let mut fresh = Vec::new();
        if let Ok(mut f) = fs::File::open(path)
            && f.seek(SeekFrom::Start(self.offset)).is_ok()
            && f.read_to_end(&mut fresh).is_ok()
        {
            self.offset += fresh.len() as u64;
            self.partial.extend_from_slice(&fresh);
        }
        let mut out = Vec::new();
        let mut start = 0;
        while let Some(i) = self.partial[start..].iter().position(|&b| b == b'\n') {
            let line = &self.partial[start..start + i];
            out.push(String::from_utf8_lossy(line).trim_end_matches('\r').to_string());
            start += i + 1;
        }
        self.partial.drain(..start);
        out
    }

    /// The line still being written, as far as it goes.
    fn unfinished(&self) -> String {
        String::from_utf8_lossy(&self.partial).into_owned()
    }
}

/// The guest's address on the lab network (QEMU's DHCP gives the first guest this).
const GUEST_IP: [u8; 4] = [10, 0, 2, 15];

/// What the capture says went wrong with the guest's TCP, if anything.
fn tcp_problems(run: &QemuRun) -> Vec<String> {
    match &run.tcp {
        None => Vec::new(),
        Some(Err(e)) => vec![format!("the network capture could not be read: {e}")],
        Some(Ok(r)) if !r.unacked_fins.is_empty() => vec![format!(
            "the guest never acknowledged the FIN of {} connection(s), leaving the peer retransmitting: {}",
            r.unacked_fins.len(),
            r.unacked_fins.join(", ")
        )],
        Some(Ok(_)) => Vec::new(),
    }
}

/// The connections a killed network service left behind, if there were any.
fn abandoned_note(r: &pcap::TcpReport) -> String {
    if r.abandoned.is_empty() {
        return String::new();
    }
    format!(
        ", {} left behind by a killed network service (the peer's FIN came after the guest had been silent for 20+ s, with nothing left to answer it: {})",
        r.abandoned.len(),
        r.abandoned.join(", ")
    )
}

/// One line about the guest's TCP for a passing run, or nothing if it used none.
fn tcp_summary(run: &QemuRun) -> String {
    match &run.tcp {
        Some(Ok(r)) if r.connections > 0 => format!(
            "; tcp: {} connections, {} closed cleanly, {} reset, {} segment(s) the peer sent again{}",
            r.connections,
            r.closed_cleanly,
            r.reset,
            r.peer_retransmits,
            abandoned_note(r)
        ),
        _ => String::new(),
    }
}

/// A machine the image is expected to boot on. The first entry is the pinned lab
/// profile of ADR-0003 (PRD §6); the rest are the variations the compatibility
/// matrix walks, so "it boots here" is a checked claim rather than an assumption.
struct Machine {
    name: &'static str,
    /// `-machine` value.
    machine: &'static str,
    cpu: &'static str,
    smp: &'static str,
    memory: &'static str,
    /// `-device` string for the block controller, empty for a machine with no disk.
    block_device: &'static str,
    /// `-device` string for the network card, empty for a machine with none. The
    /// card is always attached to the restricted lab network ([`lab::netdev`]): the
    /// virtual gateway, and the lab services through explicit forwarding rules --
    /// no other host service, no internet.
    net_device: &'static str,
    /// `-device` string for the entropy device, empty for a machine with none. A
    /// machine without one falls back to RDRAND, when its CPU has that.
    rng_device: &'static str,
    /// Appended verbatim (display and other knobs).
    extra: &'static [&'static str],
    /// Markers this configuration must produce on top of the common ones.
    must_contain: &'static [&'static str],
    /// How the data volume sits on the data disk.
    data_disk: DataDisk,
}

/// The data disk's layout.
#[derive(Clone, Copy, PartialEq, Eq)]
enum DataDisk {
    /// The FAT32 volume fills the disk (what every scenario uses).
    Whole,
    /// A GPT disk the way an installed system has one: an EFI system partition
    /// first, the data volume in the second partition.
    Gpt,
}

const LAB: Machine = Machine {
    name: "lab",
    machine: "q35,accel=tcg",
    cpu: "qemu64",
    smp: "4",
    memory: "8G",
    block_device: "virtio-blk-pci,drive=spacedata,disable-legacy=on",
    net_device: VIRTIO_NET,
    rng_device: VIRTIO_RNG,
    extra: &[],
    must_contain: &[],
    data_disk: DataDisk::Whole,
};

/// The network card most machines get: modern-only virtio-net with no option ROM
/// (the firmware's own driver is enough, and nothing here boots from the network).
const VIRTIO_NET: &str = "virtio-net-pci,netdev=spacenet,disable-legacy=on,romfile=";

/// The entropy device most machines get: modern-only virtio-rng, fed by the host.
const VIRTIO_RNG: &str = "virtio-rng-pci,disable-legacy=on";

/// Configurations the acceptance run must survive unchanged.
const MACHINES: &[Machine] = &[
    LAB,
    Machine {
        name: "q35-1cpu-2g",
        machine: "q35,accel=tcg",
        cpu: "qemu64",
        smp: "1",
        memory: "2G",
        block_device: "virtio-blk-pci,drive=spacedata,disable-legacy=on",
        net_device: VIRTIO_NET,
        rng_device: VIRTIO_RNG,
        extra: &[],
        must_contain: &["[kernel] vfs: FAT32 mounted", "[init] ALL TESTS PASSED"],
        data_disk: DataDisk::Whole,
    },
    Machine {
        name: "i440fx",
        machine: "pc,accel=tcg",
        cpu: "qemu64",
        smp: "2",
        memory: "4G",
        block_device: "virtio-blk-pci,drive=spacedata,disable-legacy=on",
        net_device: VIRTIO_NET,
        // Neither an entropy device nor RDRAND (`qemu64`): nothing that needs keys can
        // run, and it must say so rather than use something predictable.
        rng_device: "",
        extra: &[],
        must_contain: &[
            "[kernel] vfs: FAT32 mounted",
            "[kernel] entropy: none",
            "[init] entropy: none",
            "[init] SKIP TLS: TLS 1.3 with ChaCha20-Poly1305: 16 KiB go both ways intact, and close_notify ends it (no entropy source)",
            "[init] ALL TESTS PASSED",
        ],
        data_disk: DataDisk::Whole,
    },
    Machine {
        name: "cpu-max",
        machine: "q35,accel=tcg",
        cpu: "max",
        smp: "4",
        memory: "8G",
        block_device: "virtio-blk-pci,drive=spacedata,disable-legacy=on",
        net_device: VIRTIO_NET,
        // No entropy device, but `-cpu max` has RDRAND: the fallback source.
        rng_device: "",
        extra: &[],
        // TLS keys come from RDRAND here.
        must_contain: &[
            "[kernel] entropy: RDRAND",
            "[init] entropy: available",
            "[init] PASS TLS: TLS 1.3 with ChaCha20-Poly1305: 16 KiB go both ways intact",
            "[init] ALL TESTS PASSED",
        ],
        data_disk: DataDisk::Whole,
    },
    Machine {
        name: "virtio-transitional",
        machine: "q35,accel=tcg",
        cpu: "qemu64",
        smp: "4",
        memory: "8G",
        // A transitional device answers to the legacy id (0x1001 block, 0x1000 net)
        // and still offers the modern capabilities; both drivers must negotiate
        // VIRTIO_F_VERSION_1 on it.
        block_device: "virtio-blk-pci,drive=spacedata,disable-legacy=off,disable-modern=off",
        net_device: "virtio-net-pci,netdev=spacenet,disable-legacy=off,disable-modern=off,romfile=",
        rng_device: "virtio-rng-pci,disable-legacy=off,disable-modern=off",
        extra: &[],
        must_contain: &[
            "[kernel] vfs: FAT32 mounted",
            "[init] PASS NET: ICMP echo to the gateway comes back intact",
            "[kernel] entropy: virtio-rng",
            "[init] ALL TESTS PASSED",
        ],
        data_disk: DataDisk::Whole,
    },
    Machine {
        name: "virtio-small-queue",
        machine: "q35,accel=tcg",
        cpu: "qemu64",
        smp: "4",
        memory: "8G",
        // A device that offers only four descriptors: the driver must take every
        // ring index modulo the negotiated size and chunk requests to fit it.
        block_device: "virtio-blk-pci,drive=spacedata,disable-legacy=on,queue-size=4",
        net_device: VIRTIO_NET,
        rng_device: VIRTIO_RNG,
        extra: &[],
        must_contain: &[
            "queue size 4 (max 4), 2 data pages/request",
            "[kernel] vfs: FAT32 mounted",
            "[init] ALL TESTS PASSED",
        ],
        data_disk: DataDisk::Whole,
    },
    Machine {
        name: "no-disk",
        machine: "q35,accel=tcg",
        cpu: "qemu64",
        smp: "4",
        memory: "8G",
        block_device: "",
        net_device: VIRTIO_NET,
        rng_device: VIRTIO_RNG,
        extra: &[],
        // No storage is a supported configuration: the kernel says so and the
        // disk-backed tests are skipped instead of failing.
        must_contain: &[
            "[kernel] virtio-blk: no device present",
            // The boot disk is on the AHCI controller; it must not pass for data.
            "[kernel] block: no disk holds a volume labelled SPACEDATA",
            "[kernel] vfs: no data volume",
            "[init] storage: none",
            "[init] SKIP A01",
            "[init] SKIP TLS: an expired certificate is refused (no disk, which holds the lab authority's certificate)",
            "[init] ALL TESTS PASSED",
        ],
        data_disk: DataDisk::Whole,
    },
    Machine {
        name: "sata-data",
        machine: "q35,accel=tcg",
        cpu: "qemu64",
        smp: "4",
        memory: "8G",
        // The data disk on the second port of the q35's own AHCI controller, next
        // to the boot disk on the first: how most PCs keep their disks. The boot
        // disk is found too, and must be left alone.
        block_device: "ide-hd,drive=spacedata,bus=ide.1",
        net_device: VIRTIO_NET,
        rng_device: VIRTIO_RNG,
        extra: &[],
        must_contain: &[
            "[kernel] block: AHCI 00:1f.2 port 0 (",
            "MBR with 1 partition(s), none labelled SPACEDATA; not used",
            "[kernel] block: AHCI 00:1f.2 port 1 (",
            "holds the data volume (the whole disk)",
            "[kernel] vfs: FAT32 mounted from AHCI 00:1f.2 port 1",
            "[init] ALL TESTS PASSED",
        ],
        data_disk: DataDisk::Whole,
    },
    Machine {
        name: "nvme-data",
        machine: "q35,accel=tcg",
        cpu: "qemu64",
        smp: "4",
        memory: "8G",
        // The data disk as namespace 1 of an NVMe controller.
        block_device: "nvme,drive=spacedata,serial=SPACEDATA1",
        net_device: VIRTIO_NET,
        rng_device: VIRTIO_RNG,
        extra: &[],
        must_contain: &[
            "1 usable namespace(s) among IDs 1-16",
            "namespace 1 (",
            "holds the data volume (the whole disk)",
            "[kernel] vfs: FAT32 mounted from NVMe",
            "[init] ALL TESTS PASSED",
        ],
        data_disk: DataDisk::Whole,
    },
    Machine {
        name: "sata-gpt",
        machine: "q35,accel=tcg",
        cpu: "qemu64",
        smp: "4",
        memory: "8G",
        // The data disk laid out like an installed system's: GPT, an EFI system
        // partition holding a FAT32 volume of its own first, the data volume second.
        // The label, not the order, picks the volume, and every request stays
        // inside the partition.
        block_device: "ide-hd,drive=spacedata,bus=ide.1",
        net_device: VIRTIO_NET,
        rng_device: VIRTIO_RNG,
        extra: &[],
        must_contain: &[
            "[kernel] block: AHCI 00:1f.2 port 1 (",
            "holds the data volume (GPT partition 2)",
            "[kernel] vfs: FAT32 mounted from AHCI 00:1f.2 port 1",
            "[init] ALL TESTS PASSED",
        ],
        data_disk: DataDisk::Gpt,
    },
    Machine {
        name: "e1000-only",
        machine: "q35,accel=tcg",
        cpu: "qemu64",
        smp: "4",
        memory: "8G",
        block_device: "virtio-blk-pci,drive=spacedata,disable-legacy=on",
        // A network card the kernel has no driver for. It must be left alone, said
        // so, and the network tests skipped -- not failed, not hung.
        net_device: "e1000,netdev=spacenet,romfile=",
        rng_device: VIRTIO_RNG,
        extra: &[],
        must_contain: &[
            "[kernel] virtio-net: no device present",
            "[init] network: none; network tests will be skipped",
            "[init] SKIP NET",
            "[init] ALL TESTS PASSED",
        ],
        data_disk: DataDisk::Whole,
    },
    Machine {
        name: "no-vga",
        machine: "q35,accel=tcg",
        cpu: "qemu64",
        smp: "4",
        memory: "8G",
        block_device: "virtio-blk-pci,drive=spacedata,disable-legacy=on",
        net_device: VIRTIO_NET,
        rng_device: VIRTIO_RNG,
        extra: &["-vga", "none"],
        // Without a GOP the console has to fall back to serial only.
        must_contain: &["[kernel] framebuffer: none usable; serial console only", "[init] ALL TESTS PASSED"],
        data_disk: DataDisk::Whole,
    },
    Machine {
        name: "vmware-vga",
        machine: "q35,accel=tcg",
        cpu: "qemu64",
        smp: "4",
        memory: "8G",
        block_device: "virtio-blk-pci,drive=spacedata,disable-legacy=on",
        net_device: VIRTIO_NET,
        rng_device: VIRTIO_RNG,
        extra: &["-vga", "vmware"],
        must_contain: &["[kernel] framebuffer:", "[init] ALL TESTS PASSED"],
        data_disk: DataDisk::Whole,
    },
];

/// QEMU command line for `m`, booting `image` with `data_image` attached.
/// How the harness puts input into a running guest.
///
/// A socket chardev on a second serial port is deliberately *not* an option: OVMF
/// drives every serial port it finds as a console, and a socket chardev makes those
/// console writes fail, which panics the bootloader before the kernel ever runs.
/// Both mechanisms below leave the firmware's console alone.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Typing {
    /// Nobody types.
    None,
    /// Press keys on the emulated PS/2 keyboard through the QEMU monitor. This is
    /// the path a person at the machine uses: scan codes, IRQ 1, the kernel's
    /// decoder.
    Keyboard,
    /// Write bytes into the guest's second UART through a pty. This is the path a
    /// headless machine on a serial console uses: IRQ 3, COM2.
    Serial,
}

#[allow(clippy::too_many_arguments)]
fn qemu_args(
    m: &Machine,
    image: &Path,
    data_image: &Path,
    vars_copy: &Path,
    code: &Path,
    gui: bool,
    serial_path: Option<&Path>,
    typing: Typing,
    monitor: Option<&Path>,
    pcap: Option<&Path>,
) -> Result<Vec<String>, String> {
    let mut a: Vec<String> = vec![
        "-machine".into(),
        m.machine.into(),
        "-cpu".into(),
        m.cpu.into(),
        "-smp".into(),
        m.smp.into(),
        "-m".into(),
        m.memory.into(),
        "-drive".into(),
        format!("if=pflash,format=raw,readonly=on,file={}", code.display()),
        "-drive".into(),
        format!("if=pflash,format=raw,file={}", vars_copy.display()),
        "-drive".into(),
        format!("format=raw,file={}", image.display()),
    ];
    if !m.block_device.is_empty() {
        a.extend([
            "-drive".into(),
            format!("if=none,id=spacedata,format=raw,file={}", data_image.display()),
            "-device".into(),
            m.block_device.into(),
        ]);
    }
    if !m.rng_device.is_empty() {
        a.extend(["-device".into(), m.rng_device.into()]);
    }
    // Without an explicit choice QEMU adds a default card on an unrestricted user
    // network; every machine here says what it has instead.
    if m.net_device.is_empty() {
        a.extend(["-nic".into(), "none".into()]);
    } else {
        a.extend(["-netdev".into(), lab::netdev(), "-device".into(), m.net_device.into()]);
        // Every frame on the lab network: the harness checks the guest's TCP in it,
        // and it opens in Wireshark or `tcpdump -r`.
        if let Some(pcap) = pcap {
            a.extend([
                "-object".into(),
                format!("filter-dump,id=spacedump,netdev=spacenet,file={}", pcap.display()),
            ]);
        }
    }
    a.extend([
        "-device".into(),
        "isa-debug-exit,iobase=0xf4,iosize=0x04".into(),
        "-no-reboot".into(),
        "-rtc".into(),
        "base=utc".into(),
    ]);
    a.extend(m.extra.iter().map(|s| (*s).to_string()));
    // COM1 carries the log out.
    match serial_path {
        Some(p) => a.extend(["-serial".into(), format!("file:{}", p.display())]),
        None => a.extend(["-serial".into(), "mon:stdio".into()]),
    }
    // COM2 exists only where the harness types into it; a pty accepts firmware
    // console writes without backpressure, which a socket chardev does not.
    if typing == Typing::Serial {
        a.extend(["-chardev".into(), "pty,id=spacekbd".into(), "-serial".into(), "chardev:spacekbd".into()]);
    }
    if let Some(path) = monitor {
        a.extend(["-monitor".into(), format!("unix:{},server,nowait", path.display())]);
    }
    if !gui {
        a.extend(["-display".into(), "none".into()]);
    }
    Ok(a)
}

fn qemu_bin() -> String {
    std::env::var("SPACEOS_QEMU").unwrap_or_else(|_| "qemu-system-x86_64".into())
}

fn run_qemu_capture(
    image: &Path,
    data_image: &Path,
    log_path: &Path,
    done_markers: &[&str],
) -> Result<QemuRun, String> {
    run_qemu_capture_typing(&LAB, image, data_image, log_path, done_markers, Typing::None, &[], "")
}

fn run_qemu_capture_on(
    machine: &Machine,
    image: &Path,
    data_image: &Path,
    log_path: &Path,
    done_markers: &[&str],
) -> Result<QemuRun, String> {
    run_qemu_capture_typing(machine, image, data_image, log_path, done_markers, Typing::None, &[], "")
}

/// Boot, and optionally type `type_lines` on the guest console once `ready_marker`
/// appears in the log.
#[allow(clippy::too_many_arguments)]
fn run_qemu_capture_typing(
    machine: &Machine,
    image: &Path,
    data_image: &Path,
    log_path: &Path,
    done_markers: &[&str],
    typing: Typing,
    type_lines: &[&str],
    ready_marker: &str,
) -> Result<QemuRun, String> {
    boot_watched(
        machine,
        image,
        data_image,
        log_path,
        Watch {
            done_markers,
            typing,
            type_lines,
            ready_marker,
            timeout: BOOT_TIMEOUT,
            vars_copy: root().join("build/OVMF_VARS.fd"),
            progress: None,
            host: None,
        },
    )
}

/// How a boot is watched, beyond the machine and its disks.
struct Watch<'a> {
    /// Any of these in the log means the guest is done.
    done_markers: &'a [&'a str],
    typing: Typing,
    type_lines: &'a [&'a str],
    ready_marker: &'a str,
    timeout: Duration,
    /// Where this boot's copy of the firmware variables goes.
    vars_copy: PathBuf,
    /// Shown every complete line of the serial log as it appears.
    progress: Option<&'a mut dyn FnMut(&str)>,
    /// Sample the emulator's descriptors, memory and lab services this often; the
    /// path is the program the lab services run as.
    host: Option<(Duration, PathBuf)>,
}

fn boot_watched(
    machine: &Machine,
    image: &Path,
    data_image: &Path,
    log_path: &Path,
    mut w: Watch,
) -> Result<QemuRun, String> {
    let (code, vars) = find_firmware()?;
    fs::copy(&vars, &w.vars_copy).map_err(|e| format!("copy OVMF vars: {e}"))?;
    if log_path.exists() {
        fs::remove_file(log_path).ok();
    }
    // The monitor is how the harness reaches the keyboard and finds the pty; it is
    // only added for a scenario that types.
    let monitor = (w.typing != Typing::None).then(|| root().join("build/monitor.sock"));
    if let Some(path) = &monitor {
        fs::remove_file(path).ok();
    }
    let pcap = (!machine.net_device.is_empty()).then(|| log_path.with_extension("pcap"));
    if let Some(p) = &pcap {
        fs::remove_file(p).ok();
    }
    let args = qemu_args(
        machine,
        image,
        data_image,
        &w.vars_copy,
        &code,
        false,
        Some(log_path),
        w.typing,
        monitor.as_deref(),
        pcap.as_deref(),
    )?;
    let start = Instant::now();
    let lab_log = log_path.with_extension("lab.log");
    let _ = fs::remove_file(&lab_log);
    // A file rather than a pipe: over hours, a pipe nobody reads fills up, and then
    // QEMU stops at its next warning.
    let stderr_path = log_path.with_extension("stderr");
    let stderr_file =
        fs::File::create(&stderr_path).map_err(|e| format!("{}: {e}", stderr_path.display()))?;
    let mut child = Command::new(qemu_bin())
        .args(&args)
        .env("SPACEOS_LAB_LOG", &lab_log)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(stderr_file)
        .spawn()
        .map_err(|e| format!("cannot start {}: {e}", qemu_bin()))?;
    // The typist stops as soon as the guest is gone, so a boot that dies early fails
    // in seconds instead of waiting out the marker deadline.
    let guest_gone = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let typist = match &monitor {
        Some(path) if !w.type_lines.is_empty() => {
            let path = path.clone();
            let log = log_path.to_path_buf();
            let ready = w.ready_marker.to_string();
            let lines: Vec<String> = w.type_lines.iter().map(|l| (*l).to_string()).collect();
            let gone = guest_gone.clone();
            let typing = w.typing;
            Some(std::thread::spawn(move || type_on_guest(typing, &path, &log, &ready, &lines, &gone)))
        }
        _ => None,
    };
    let mut tail = LogTail::default();
    let mut host = w.host.as_ref().map(|_| HostSamples::default());
    let mut next_sample = Instant::now();
    let mut finished_at: Option<Instant> = None;
    let mut timed_out = false;
    let exit_code = loop {
        if let Some(st) = child.try_wait().map_err(|e| e.to_string())? {
            break st.code();
        }
        if start.elapsed() > w.timeout {
            timed_out = true;
            child.kill().ok();
            child.wait().ok();
            break None;
        }
        if let (Some(h), Some((every, exe))) = (host.as_mut(), w.host.as_ref())
            && Instant::now() >= next_sample
        {
            h.take(child.id(), exe);
            next_sample = Instant::now() + *every;
        }
        for line in tail.lines(log_path) {
            if let Some(p) = w.progress.as_mut() {
                p(&line);
            }
            if finished_at.is_none() && w.done_markers.iter().any(|m| line.contains(m)) {
                finished_at = Some(Instant::now());
            }
        }
        if finished_at.is_none() && w.done_markers.iter().any(|m| tail.unfinished().contains(m)) {
            finished_at = Some(Instant::now());
        }
        // Early exit once the kernel reported a terminal state but QEMU lingers:
        // give the debug-exit a moment, then stop it.
        if let Some(t) = finished_at
            && start.elapsed() > Duration::from_secs(5)
            && t.elapsed() > Duration::from_millis(3000)
        {
            child.kill().ok();
            child.wait().ok();
            break None;
        }
        std::thread::sleep(Duration::from_millis(100));
    };
    guest_gone.store(true, std::sync::atomic::Ordering::Relaxed);
    let typed = match typist.map(|t| t.join()) {
        None => Ok(()),
        Some(Ok(r)) => r,
        Some(Err(_)) => Err("the console typist thread panicked".to_string()),
    };
    let stderr = fs::read_to_string(&stderr_path).unwrap_or_default();
    let mut log = fs::read_to_string(log_path).unwrap_or_default();
    if !stderr.trim().is_empty() {
        log.push_str("\n[qemu stderr]\n");
        log.push_str(&stderr);
        fs::write(log_path, &log).ok();
    }
    let input_error = typed.err();
    if let Some(e) = &input_error {
        log.push_str(&format!("\n[harness] console input failed: {e}\n"));
        fs::write(log_path, &log).ok();
    }
    let tcp = pcap.map(|p| pcap::analyze(&p, GUEST_IP));
    let lab_log = fs::read_to_string(&lab_log).unwrap_or_default();
    Ok(QemuRun { log, exit_code, elapsed: start.elapsed(), timed_out, input_error, tcp, lab_log, host })
}

/// Wait for the guest to say it is ready, then type each line on its console.
///
/// One line at a time with a pause, the way a person types: the point of the
/// scenario is that the session keeps serving between keystrokes, which a burst
/// would not show.
fn type_on_guest(
    typing: Typing,
    monitor: &Path,
    log_path: &Path,
    ready_marker: &str,
    lines: &[String],
    guest_gone: &std::sync::atomic::AtomicBool,
) -> Result<(), String> {
    let mut mon = connect_monitor(monitor, guest_gone)?;
    wait_for_marker(log_path, ready_marker, guest_gone)?;
    match typing {
        Typing::None => Ok(()),
        Typing::Keyboard => {
            for line in lines {
                // A line starting with `!` is a step of a script rather than text:
                // `!key alt-tab` presses a combination, `!wait text` waits for the log
                // to show it, `!shot name` keeps a picture of the screen.
                if let Some(step) = line.strip_prefix('!') {
                    script_step(&mut mon, step, log_path, guest_gone)?;
                    continue;
                }
                for ch in line.chars() {
                    monitor_cmd(&mut mon, &format!("sendkey {}", key_name(ch)?), KEY_DRAIN)?;
                    std::thread::sleep(Duration::from_millis(30));
                }
                monitor_cmd(&mut mon, "sendkey ret", KEY_DRAIN)?;
                std::thread::sleep(Duration::from_millis(500));
            }
            Ok(())
        }
        Typing::Serial => {
            use std::io::Write;
            let pty = monitor_pty(&mut mon)?;
            let mut port = fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(&pty)
                .map_err(|e| format!("open {pty}: {e}"))?;
            for line in lines {
                port.write_all(line.as_bytes()).map_err(|e| format!("write: {e}"))?;
                port.write_all(b"\r").map_err(|e| format!("write: {e}"))?;
                port.flush().ok();
                std::thread::sleep(Duration::from_millis(500));
            }
            Ok(())
        }
    }
}

/// One scripted step (see `type_on_guest`).
fn script_step(
    mon: &mut std::os::unix::net::UnixStream,
    step: &str,
    log_path: &Path,
    guest_gone: &std::sync::atomic::AtomicBool,
) -> Result<(), String> {
    let (verb, arg) = step.split_once(' ').unwrap_or((step, ""));
    match verb {
        "key" => {
            monitor_cmd(mon, &format!("sendkey {arg}"), KEY_DRAIN)?;
            std::thread::sleep(Duration::from_millis(300));
            Ok(())
        }
        "wait" => wait_for_marker(log_path, arg, guest_gone),
        "sleep" => {
            std::thread::sleep(Duration::from_millis(arg.parse().map_err(|_| format!("sleep {arg:?}"))?));
            Ok(())
        }
        "shot" => screenshot(mon, log_path, arg),
        _ => Err(format!("unknown script step {step:?}")),
    }
}

/// Keep a picture of the screen as `<log dir>/<log stem>-<name>.png`: QEMU writes
/// what the display shows as PPM, and the harness turns it into a PNG half the size
/// -- small enough to keep as evidence, large enough to read.
fn screenshot(mon: &mut std::os::unix::net::UnixStream, log_path: &Path, name: &str) -> Result<(), String> {
    let stem = log_path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    let ppm = log_path.with_file_name(format!("{stem}-{name}.ppm"));
    let png_path = log_path.with_file_name(format!("{stem}-{name}.png"));
    fs::remove_file(&ppm).ok();
    monitor_cmd(mon, &format!("screendump {}", ppm.display()), Duration::from_millis(500))?;
    let deadline = Instant::now() + Duration::from_secs(10);
    let raw = loop {
        if let Ok(b) = fs::read(&ppm)
            && let Some(img) = parse_ppm(&b)
        {
            break img;
        }
        if Instant::now() > deadline {
            return Err(format!("QEMU never wrote the screenshot {}", ppm.display()));
        }
        std::thread::sleep(Duration::from_millis(100));
    };
    let (w, h, rgb) = raw;
    let (hw, hh) = (w / 2, h / 2);
    let mut half = Vec::with_capacity(hw * hh * 3);
    for y in 0..hh {
        for x in 0..hw {
            for c in 0..3 {
                let at = |xx: usize, yy: usize| rgb[(yy * w + xx) * 3 + c] as u32;
                let v = (at(2 * x, 2 * y)
                    + at(2 * x + 1, 2 * y)
                    + at(2 * x, 2 * y + 1)
                    + at(2 * x + 1, 2 * y + 1))
                    / 4;
                half.push(v as u8);
            }
        }
    }
    let f = fs::File::create(&png_path).map_err(|e| format!("{}: {e}", png_path.display()))?;
    let mut enc = png::Encoder::new(io::BufWriter::new(f), hw as u32, hh as u32);
    enc.set_color(png::ColorType::Rgb);
    enc.set_depth(png::BitDepth::Eight);
    enc.set_compression(png::Compression::Best);
    let mut writer = enc.write_header().map_err(|e| format!("png: {e}"))?;
    writer.write_image_data(&half).map_err(|e| format!("png: {e}"))?;
    fs::remove_file(&ppm).ok();
    Ok(())
}

/// Width, height and RGB bytes of a binary PPM (`P6`, maxval 255).
fn parse_ppm(b: &[u8]) -> Option<(usize, usize, Vec<u8>)> {
    let mut fields = Vec::new();
    let mut i = 0;
    while fields.len() < 4 && i < b.len() {
        if b[i] == b'#' {
            while i < b.len() && b[i] != b'\n' {
                i += 1;
            }
        } else if b[i].is_ascii_whitespace() {
            i += 1;
        } else {
            let start = i;
            while i < b.len() && !b[i].is_ascii_whitespace() {
                i += 1;
            }
            fields.push(String::from_utf8_lossy(&b[start..i]).into_owned());
        }
    }
    if fields.len() < 4 || fields[0] != "P6" || fields[3] != "255" {
        return None;
    }
    let (w, h): (usize, usize) = (fields[1].parse().ok()?, fields[2].parse().ok()?);
    let data = b.get(i + 1..i + 1 + w * h * 3)?;
    Some((w, h, data.to_vec()))
}

/// How long to drain the monitor after a keystroke: long enough that its echo never
/// backs up in the socket, short enough that typing stays fast.
const KEY_DRAIN: Duration = Duration::from_millis(5);

fn connect_monitor(
    path: &Path,
    guest_gone: &std::sync::atomic::AtomicBool,
) -> Result<std::os::unix::net::UnixStream, String> {
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        if let Ok(s) = std::os::unix::net::UnixStream::connect(path) {
            // Short, because a keystroke needs no answer; `monitor_cmd` keeps polling
            // until its own deadline when it does want one.
            s.set_read_timeout(Some(Duration::from_millis(5))).ok();
            return Ok(s);
        }
        if guest_gone.load(std::sync::atomic::Ordering::Relaxed) {
            return Err("the guest exited before the monitor socket appeared".into());
        }
        if Instant::now() > deadline {
            return Err("the QEMU monitor socket never appeared".into());
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn wait_for_marker(
    log_path: &Path,
    marker: &str,
    guest_gone: &std::sync::atomic::AtomicBool,
) -> Result<(), String> {
    let deadline = Instant::now() + Duration::from_secs(150);
    loop {
        if fs::read_to_string(log_path).map(|s| s.contains(marker)).unwrap_or(false) {
            return Ok(());
        }
        if guest_gone.load(std::sync::atomic::Ordering::Relaxed) {
            return Err(format!("the guest exited without printing {marker:?}"));
        }
        if Instant::now() > deadline {
            return Err(format!("the guest never printed {marker:?}"));
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Send a monitor command and drain whatever it echoes back.
///
/// `wait` is how long to keep reading. Keystrokes do not need an answer, so they
/// pass a few milliseconds - just enough to keep the socket from filling - while a
/// query that needs its reply passes longer. Waiting for a reply on every keystroke
/// would add a third of a second per character.
fn monitor_cmd(
    mon: &mut std::os::unix::net::UnixStream,
    cmd: &str,
    wait: Duration,
) -> Result<String, String> {
    use std::io::{Read, Write};
    mon.write_all(format!("{cmd}\n").as_bytes()).map_err(|e| format!("monitor write: {e}"))?;
    mon.flush().ok();
    let mut out = String::new();
    let mut buf = [0u8; 4096];
    let deadline = Instant::now() + wait;
    while Instant::now() < deadline {
        match mon.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => out.push_str(&String::from_utf8_lossy(&buf[..n])),
            // The socket read timeout is short so a keystroke drains quickly; a
            // timeout is "nothing yet", not "nothing coming", so keep waiting until
            // the caller's deadline.
            Err(e) if matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut) => {}
            Err(_) => break,
        }
    }
    Ok(out)
}

/// Ask the monitor where the second UART's pty landed.
fn monitor_pty(mon: &mut std::os::unix::net::UnixStream) -> Result<String, String> {
    for _ in 0..5 {
        let out = monitor_cmd(mon, "info chardev", Duration::from_millis(400))?;
        if let Some(rest) = out.split("/dev/pts/").nth(1) {
            let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
            if !digits.is_empty() {
                return Ok(format!("/dev/pts/{digits}"));
            }
        }
    }
    Err("the monitor did not report a pty for the second serial port".into())
}

/// QEMU key name for a character the scenarios type. An unmapped character is a
/// harness error rather than a silently dropped keystroke.
fn key_name(ch: char) -> Result<String, String> {
    match ch {
        'a'..='z' | '0'..='9' => Ok(ch.to_string()),
        ' ' => Ok("spc".into()),
        '/' => Ok("slash".into()),
        '-' => Ok("minus".into()),
        '.' => Ok("dot".into()),
        // A correction is part of typing, so the harness must be able to make one.
        '\u{8}' => Ok("backspace".into()),
        _ => Err(format!("no QEMU key name for {ch:?}")),
    }
}

struct Scenario {
    name: &'static str,
    cmdline: &'static str,
    expect_exit: i32,
    must_contain: &'static [&'static str],
    /// Markers this scenario alone must produce, on top of `must_contain`.
    must_contain_extra: &'static [&'static str],
    must_not_contain: &'static [&'static str],
    /// Consecutive boots of the same image that must all pass (D01 asks for the
    /// checksum to hold after a reboot).
    runs: u32,
    /// How the harness types, if at all.
    typing: Typing,
    /// Markers required only on the final boot of a multi-boot scenario. This is
    /// where "the volume remembered what the last boot wrote" belongs: it cannot be
    /// true on the first boot of a fresh disk, and demanding it there would be
    /// demanding a lie.
    final_boot_markers: &'static [&'static str],
    /// Lines typed on the guest console once `ready_marker` appears. Empty for a
    /// scenario nobody types into.
    type_lines: &'static [&'static str],
    ready_marker: &'static str,
    /// Lines the lab services must have written: the server's own account of what
    /// the guest did. A refusal the guest reports is half the story; the server
    /// being told why (a TLS alert) is the other half.
    lab_must_contain: &'static [&'static str],
    /// Lines the lab services must never have written: a request that should not
    /// have left the guest, a stream nobody stopped, a secret that got out.
    lab_must_not_contain: &'static [&'static str],
}

/// QEMU exit status = (value << 1) | 1 for isa-debug-exit.
const EXIT_SUCCESS: i32 = (0x10 << 1) | 1; // 33
const EXIT_FAILURE: i32 = (0x11 << 1) | 1; // 35
const EXIT_PANIC: i32 = (0x3f << 1) | 1; // 127

const TERMINAL_READY: &str = "[shell] terminal ready on the console";

/// What a typed session must produce, whichever way the keystrokes arrive.
const TERMINAL_MARKERS: &[&str] = &[
    "[term] Space OS interactive session",
    TERMINAL_READY,
    // Each typed command is answered. `help` is typed with a correction in it
    // (`helpp` then backspace), so this line also proves the erase worked: the
    // command only resolves if the extra character really went away.
    "[shell] commands: help, status, ls",
    "MODEL.SLM",
    // `status` answers for itself, before and after Stop -- not just the line the
    // session paints after every command.
    "[shell] worker idle (-), last exit code",
    "[shell] started worker 'hang'",
    "[shell] worker running (hang), last exit code",
    // Stop reaches a worker that never calls the kernel again...
    "[shell] worker 'hang' ended: stopped",
    // ...and the session is still serving afterwards, and says so when asked.
    "session: worker stopped (hang)",
    "[shell] worker stopped (hang), last exit code -1",
    "[shell] closing the session",
    "[term] session ended",
];

/// The desktop driven from the keyboard (ADR-0020): every step a person would take,
/// with a picture of the screen at the points worth seeing. `!wait` waits for the
/// desktop's own account of a step; keys typed after it are handled in order.
const DESKTOP_SCRIPT: &[&str] = &[
    "!wait [desk] window 1 'Terminal' (terminal) opened",
    "!sleep 600",
    "!shot 1-terminal",
    "ls /spaceos",
    "!wait [deskterm] /spaceos:",
    "!key meta_l-e",
    "!wait [desk] window 2 'Files' (files) opened",
    "!key down",
    "!key down",
    "!key meta_l-a",
    "!wait [desk] window 3 'Agent Center' (agent) opened",
    // The inference worker crashes; everything else must carry on.
    "!key 2",
    "!wait [deskagent] worker 'crash' crashed (page fault)",
    "!sleep 600",
    "!shot 2-worker-crashed",
    "!key alt-tab",
    "status",
    "!wait the session has served",
    "!key alt-tab",
    "!key down",
    "!key alt-tab",
    // A worker that never returns, and the Stop that ends it.
    "!key 3",
    "!wait [deskagent] worker 'hang' running",
    "!key s",
    "!wait [deskagent] Stop: worker 'hang' stopped",
    // The real worker (PRD §9, first end-to-end scenario): the model on the guest
    // disk writes text until Stop, which it honours between two compute steps, and
    // then the terminal still answers.
    "!key 5",
    "!wait [deskagent] worker 'infer' wrote its first token",
    "!sleep 700",
    "!key s",
    "!wait [deskagent] worker 'infer' stopped between two steps",
    "!sleep 600",
    "!shot 3-inference-stopped",
    "!key alt-tab",
    "ls /spaceos/docs",
    "!wait [deskterm] /spaceos/docs:",
    "!key alt-tab",
    "!key alt-tab",
    "!key alt-shift-right",
    "!wait [desk] resized 'Agent Center'",
    "!key alt-down",
    "!wait [desk] moved 'Agent Center'",
    "!key meta_l-m",
    "!wait [desk] minimized 'Agent Center'",
    "!key ctrl-alt-right",
    "!wait [desk] workspace 2",
    "!key ctrl-alt-left",
    "!wait [desk] workspace 1",
    "!key meta_l-h",
    "!wait [desk] high contrast on",
    "!sleep 600",
    "!shot 4-high-contrast",
    "!key meta_l-h",
    "!wait [desk] high contrast off",
    "!key alt-f4",
    "!wait closed: the app",
    "!sleep 600",
    "!shot 5-after-close",
    // The Command Center: search the index, a context bundle, and show the best
    // result in the file manager.
    "!key meta_l-spc",
    "!wait [desk] window 4 'Command Center' (command) opened",
    "channel",
    "!wait results for 'channel'; first /spaceos/docs/IPC.TXT",
    "!key ctrl-b",
    "!wait [deskcmd] context bundle for 'channel'",
    "!key ctrl-o",
    "!wait [deskfiles] selected IPC.TXT",
    "!sleep 600",
    "!shot 6-command-center",
    "!key ctrl-alt-delete",
];

/// What the lab services must never write, in one pass or in a thousand.
const LAB_FORBIDDEN: &[&str] = &[
    "nobody stopped it",
    "THE CREDENTIAL LEAKED",
    "attempt 4 answered",
    // Refused in the guest: neither may ever reach the provider.
    "lab-budget",
    "lab-local",
];

const SCENARIOS: &[Scenario] = &[
    Scenario {
        name: "acceptance",
        // The default, said out loud: the K02 test reads the line back and checks
        // that the program it names is the one running.
        cmdline: "init=bin/init",
        expect_exit: EXIT_SUCCESS,
        must_contain: &[
            "[kernel] selftest: heap ok",
            "[kernel] cmdline: \"init=bin/init\"",
            "[init] kernel command line: \"init=bin/init\"",
            "input decoding ok",
            "[kernel] virtio-blk: ready",
            "[kernel] virtio-net: pci",
            "[kernel] virtio-rng: pci",
            "[kernel] entropy: virtio-rng",
            "[init] entropy: available",
            "[kernel] rtc:",
            "[init] PASS NET: ARP: the gateway answers who-has 10.0.2.2",
            "[init] PASS NET: ICMP echo to the gateway comes back intact",
            "[init] PASS NET: a frame arriving while the receiver sleeps wakes it",
            // TLS ran here rather than being skipped: every case is required.
            "[init] PASS TLS: TLS 1.3 with ChaCha20-Poly1305: 16 KiB go both ways intact",
            "[init] PASS TLS: TLS 1.3 with AES-128-GCM: 16 KiB go both ways intact",
            "[init] PASS TLS: TLS 1.3 with AES-256-GCM: 16 KiB go both ways intact",
            "[init] PASS TLS: an expired certificate is refused",
            "[init] PASS TLS: a certificate for another name is refused",
            "[init] PASS TLS: a certificate from an unknown authority is refused",
            "[init] PASS TLS: a record altered on the way is caught",
            "[init] PASS TLS: a connection cut short without close_notify is reported as truncated",
            "[init] PASS NET: closing a connection the peer is still sending on resets it, so the peer stops",
            // I01, against the lab's mock provider: every case is required.
            "[init] PASS I01: the adapter holds the credential, and the provider refuses a wrong one",
            "[init] PASS I01: answers stream back as the provider writes them, and cost what it reports",
            "[init] PASS I01: the model's tool calls go through the Tool Broker, inside the workspace",
            "[init] PASS I01: a tool call for the credential is refused, and the provider hears only that",
            "[init] PASS I01: a stalled answer ends at the deadline, and the adapter keeps serving",
            "[init] PASS I01: an overloaded provider is tried three times, and no more",
            "[init] PASS I01: an answer nested deep enough to exhaust a stack is refused, and the adapter lives on",
            "[init] PASS I01: a provider streaming past what was reserved is cut off, and charged what it used",
            "[init] PASS I01: an ask that could exceed the budget is refused before a frame leaves",
            "[init] PASS I01: local-only work is refused before a frame leaves",
            "[init] PASS I01: the adapter quits cleanly, and the broker's audit holds every tool call",
            "[kernel] vfs: FAT32 mounted",
            "[init] Space OS init running",
            "[ai] model verified: sha256",
            "[ai] generated 128 tokens offline, all matching the pinned baseline",
            "[init] ALL TESTS PASSED",
            // The session was told its input was lost and kept its terminal, instead
            // of running the half-line that survived.
            "[shell] input was lost; the line was discarded",
        ],
        must_contain_extra: &[],
        must_not_contain: &["KERNEL PANIC", "[init] FAIL", "TESTS FAILED", "[shell] unknown command"],
        runs: 1,
        typing: Typing::None,
        final_boot_markers: &[],
        type_lines: &[],
        ready_marker: "",
        // The servers' side of the TLS cases: the echo went through and ended with
        // close_notify, and every refusal reached the server as the alert that names
        // it -- including bad_record_mac, which RFC 8446 requires.
        lab_must_contain: &[
            "tls-echo: handshake complete (TLS13_CHACHA20_POLY1305_SHA256)",
            "tls-echo: 16384 bytes echoed, closed with close_notify both ways",
            "tls-aes128: handshake complete (TLS13_AES_128_GCM_SHA256)",
            "tls-aes128: 16384 bytes echoed, closed with close_notify both ways",
            "tls-aes256: handshake complete (TLS13_AES_256_GCM_SHA384)",
            "tls-aes256: 16384 bytes echoed, closed with close_notify both ways",
            "tls-expired: the handshake ended: received fatal alert: CertificateExpired",
            "tls-wrongname: the handshake ended: received fatal alert: BadCertificate",
            "tls-untrusted: the handshake ended: received fatal alert: UnknownCA",
            "tls-tamper: after 23 bytes: received fatal alert: BadRecordMac",
            "tls-truncate: answered, and left without close_notify",
            // A stream the guest walked away from was reset, so it stopped.
            "chargen: the guest went away after",
            // The cloud provider's side of I01 (a mock, ADR-0018).
            "cloud: 401: lab-echo: the key presented is not the lab's",
            "cloud: lab-echo: streamed 4 pieces",
            "cloud: lab-tool: tool_result for toolu_lab_1: 119 bytes",
            "cloud: lab-exfil: the tool was refused: refused by the Tool Broker",
            "cloud: lab-stall: the client closed the connection after",
            "cloud: lab-overloaded: attempt 3 answered 529",
            "cloud: lab-runaway: the client closed the connection after",
            "cloud: lab-hostile: the client closed the connection",
        ],
        lab_must_not_contain: LAB_FORBIDDEN,
    },
    Scenario {
        name: "stress",
        // The stability run in short (ADR-0019): the whole suite twice in one boot, a
        // round of random kills after each pass -- the network service among them,
        // mid-transfer -- and the machine's memory back where the first pass left it,
        // to the frame and the byte. `cargo xtask stress` is the long version.
        cmdline: "stress=2",
        expect_exit: EXIT_SUCCESS,
        must_contain: &[
            "[stress] stability run: 2 passes of the acceptance suite",
            "[stress] baseline after pass 1:",
            "[stress] pass 1: ok;",
            "[stress] pass 2: ok;",
            "spacenet at",
            "network back",
            "[stress] STRESS PASSED",
        ],
        must_contain_extra: &[],
        must_not_contain: &["KERNEL PANIC", "[init] FAIL", "LEAK", "STRESS FAILED", "[churn]"],
        runs: 1,
        typing: Typing::None,
        final_boot_markers: &[],
        type_lines: &[],
        ready_marker: "",
        lab_must_contain: &[],
        lab_must_not_contain: LAB_FORBIDDEN,
    },
    Scenario {
        name: "panic-diagnosis",
        cmdline: "selftest=panic",
        expect_exit: EXIT_PANIC,
        must_contain: &[
            "!!! KERNEL PANIC !!!",
            "deliberate kernel panic",
            "backtrace (frame pointers):",
            "spacekernel: halted after panic",
        ],
        must_contain_extra: &[],
        must_not_contain: &["[init] Space OS init running"],
        runs: 1,
        typing: Typing::None,
        final_boot_markers: &[],
        type_lines: &[],
        ready_marker: "",
        lab_must_contain: &[],
        lab_must_not_contain: &[],
    },
    Scenario {
        name: "kernel-fault-diagnosis",
        cmdline: "selftest=kfault",
        expect_exit: EXIT_PANIC,
        must_contain: &[
            "!!! CPU EXCEPTION IN KERNEL MODE: page fault !!!",
            "cr2=0xfffff000dead0000",
            "!!! KERNEL PANIC !!!",
        ],
        must_contain_extra: &[],
        must_not_contain: &["[init] Space OS init running"],
        runs: 1,
        typing: Typing::None,
        final_boot_markers: &[],
        type_lines: &[],
        ready_marker: "",
        lab_must_contain: &[],
        lab_must_not_contain: &[],
    },
    Scenario {
        name: "kernel-stack-overflow-diagnosis",
        cmdline: "selftest=stack",
        expect_exit: EXIT_PANIC,
        must_contain: &["!!! CPU EXCEPTION IN KERNEL MODE: double fault !!!", "!!! KERNEL PANIC !!!"],
        must_contain_extra: &[],
        must_not_contain: &["[init] Space OS init running"],
        runs: 1,
        typing: Typing::None,
        final_boot_markers: &[],
        type_lines: &[],
        ready_marker: "",
        lab_must_contain: &[],
        lab_must_not_contain: &[],
    },
    Scenario {
        name: "storage-reboot",
        cmdline: "",
        expect_exit: EXIT_SUCCESS,
        must_contain: &[
            "[kernel] virtio-blk: ready",
            "[kernel] vfs: FAT32 mounted",
            "[init] PASS D01",
            "[init] ALL TESTS PASSED",
        ],
        must_contain_extra: &[],
        must_not_contain: &["KERNEL PANIC", "[init] FAIL"],
        runs: 2,
        typing: Typing::None,
        // The volume is shared by every scenario, so the generation number depends on
        // what ran before; what must be true here is that this boot found the one the
        // previous boot left. `init` checks the arithmetic itself and fails if it is
        // off by anything.
        final_boot_markers: &["survived the reboot, wrote"],
        type_lines: &[],
        ready_marker: "",
        lab_must_contain: &[],
        lab_must_not_contain: &[],
    },
    Scenario {
        name: "init-exit-diagnosis",
        // A first process that ends without asking for shutdown leaves nothing to
        // schedule. That must read as a diagnosis, not as a hang.
        cmdline: "init=bin/hello",
        expect_exit: EXIT_FAILURE,
        must_contain: &[
            "[kernel] init spawned as pid 1 from bin/hello",
            "[hello] hello from user space",
            "ended without requesting shutdown; nothing left to run",
        ],
        must_contain_extra: &[],
        must_not_contain: &["KERNEL PANIC"],
        runs: 1,
        typing: Typing::None,
        final_boot_markers: &[],
        type_lines: &[],
        ready_marker: "",
        lab_must_contain: &[],
        lab_must_not_contain: &[],
    },
    Scenario {
        name: "init-missing-diagnosis",
        // A misspelled init names the program that actually failed.
        cmdline: "init=bin/not_a_program",
        expect_exit: EXIT_PANIC,
        must_contain: &["cannot start \"bin/not_a_program\" from initrd: not found", "!!! KERNEL PANIC !!!"],
        must_contain_extra: &[],
        must_not_contain: &["[init] Space OS init running"],
        runs: 1,
        typing: Typing::None,
        final_boot_markers: &[],
        type_lines: &[],
        ready_marker: "",
        lab_must_contain: &[],
        lab_must_not_contain: &[],
    },
    Scenario {
        name: "terminal",
        // The same image, booted into a session instead of the acceptance run, and
        // driven from the emulated keyboard: scan codes, IRQ 1, the kernel decoder.
        cmdline: "init=bin/spaceterm",
        expect_exit: EXIT_SUCCESS,
        must_contain: TERMINAL_MARKERS,
        // No second UART exists in this scenario, so every keystroke came from the
        // emulated keyboard.
        must_contain_extra: &["[kernel] console input: keyboard (IRQ1); no COM2 UART"],
        must_not_contain: &["KERNEL PANIC", "unknown command"],
        runs: 1,
        typing: Typing::Keyboard,
        final_boot_markers: &[],
        type_lines: &["helpp\u{8}", "status", "ls /spaceos", "run hang", "status", "stop", "status", "quit"],
        ready_marker: TERMINAL_READY,
        lab_must_contain: &[],
        lab_must_not_contain: &[],
    },
    Scenario {
        name: "desktop",
        // The image booted into the desktop (ADR-0020) and driven from the emulated
        // keyboard: windows opened, used, moved, resized, minimized, moved between
        // workspaces and closed, an inference worker crashing and another stopped --
        // with screenshots kept as `build/logs/desktop-*.png`.
        cmdline: "init=bin/spacedesk",
        expect_exit: EXIT_SUCCESS,
        must_contain: &[
            "[kernel] display: leased to pid 1 'bin/spacedesk'",
            "[desk] Space OS desktop on a 1280x800 screen",
            "[desk] window 1 'Terminal' (terminal) opened",
            "[deskterm] /spaceos:",
            "[desk] window 2 'Files' (files) opened",
            "[deskfiles] /spaceos:",
            "[desk] window 3 'Agent Center' (agent) opened",
            "[deskagent] worker 'crash' crashed (page fault)",
            // Typed into the terminal after the crash, and answered.
            "the session has served",
            "[deskagent] Stop: worker 'hang' stopped",
            // The model wrote text, Stop ended it between two steps, and the
            // terminal answered afterwards.
            "[deskagent] worker 'infer' wrote its first token",
            "[ai] stopped on request after",
            "[shell] Stop: worker 'infer' ended between two steps",
            "[deskagent] worker 'infer' stopped between two steps",
            "[deskterm] /spaceos/docs:",
            "[desk] shortcut Alt+Tab",
            "[desk] resized 'Agent Center'",
            "[desk] moved 'Agent Center'",
            "[desk] minimized 'Agent Center'",
            "[desk] workspace 2",
            "[desk] workspace 1",
            "[desk] high contrast on",
            "closed: the app",
            "[deskcmd] index: ",
            "results for 'channel'; first /spaceos/docs/IPC.TXT",
            "[deskcmd] context bundle for 'channel'",
            "[desk] 'Command Center' asked to show /spaceos/docs/IPC.TXT in the file manager",
            "[deskfiles] selected IPC.TXT",
            "[desk] shutting down at the person's request",
        ],
        must_contain_extra: &[],
        must_not_contain: &["KERNEL PANIC", "unknown command", "did not start", "no window", "cannot"],
        runs: 1,
        typing: Typing::Keyboard,
        final_boot_markers: &[],
        type_lines: DESKTOP_SCRIPT,
        ready_marker: "[desk] desktop ready",
        lab_must_contain: &[],
        lab_must_not_contain: &[],
    },
    Scenario {
        name: "terminal-serial",
        // The same session over a serial console: COM2, IRQ 3. A headless machine
        // has no keyboard, and this is the path it uses.
        cmdline: "init=bin/spaceterm",
        expect_exit: EXIT_SUCCESS,
        must_contain: TERMINAL_MARKERS,
        must_contain_extra: &["[kernel] console input: keyboard (IRQ1) and COM2 serial (IRQ3)"],
        must_not_contain: &["KERNEL PANIC", "unknown command"],
        runs: 1,
        typing: Typing::Serial,
        final_boot_markers: &[],
        type_lines: &["helpp\u{8}", "status", "ls /spaceos", "run hang", "status", "stop", "status", "quit"],
        ready_marker: TERMINAL_READY,
        lab_must_contain: &[],
        lab_must_not_contain: &[],
    },
];

fn check_run(s: &Scenario, run: &QemuRun, final_boot: bool) -> Vec<String> {
    let mut problems = Vec::new();
    if final_boot {
        for m in s.final_boot_markers {
            if !run.log.contains(m) {
                problems.push(format!("missing marker {m:?} on the last boot"));
            }
        }
    }
    // First, because it explains every marker that follows it: if the harness never
    // managed to type, the missing answers are the harness's fault, not the guest's.
    if let Some(e) = &run.input_error {
        problems.push(format!("the harness could not type on the guest console: {e}"));
    }
    if run.timed_out {
        problems.push(format!("timed out after {:?}", run.elapsed));
    }
    if run.exit_code != Some(s.expect_exit) {
        problems.push(format!("QEMU exit code {:?}, expected {}", run.exit_code, s.expect_exit));
    }
    for m in s.must_contain.iter().chain(s.must_contain_extra.iter()) {
        if !run.log.contains(m) {
            problems.push(format!("missing marker {m:?}"));
        }
    }
    for m in s.must_not_contain {
        if run.log.contains(m) {
            problems.push(format!("unexpected marker {m:?}"));
        }
    }
    problems.extend(tcp_problems(run));
    for m in s.lab_must_contain {
        if !run.lab_log.contains(m) {
            problems.push(format!("the lab services never wrote {m:?}"));
        }
    }
    for m in s.lab_must_not_contain {
        if let Some(line) = run.lab_log.lines().find(|l| l.contains(m)) {
            problems.push(format!("the lab services wrote {line:?}"));
        }
    }
    problems
}

fn cmd_test(release: bool, only: Option<&str>) -> Result<(), String> {
    if let Some(name) = only
        && !SCENARIOS.iter().any(|s| s.name == name)
    {
        return Err(format!("no scenario named {name:?}"));
    }
    let built = build(release)?;
    let logs = root().join("build/logs");
    fs::create_dir_all(&logs).map_err(|e| e.to_string())?;
    let data_image = root().join("build/data.img");
    make_data_disk(&data_image)?;
    let mut failures = 0;
    let mut ran = 0;
    for s in SCENARIOS.iter().filter(|s| only.is_none_or(|n| n == s.name)) {
        ran += 1;
        let image = root().join(format!("build/esp-{}.img", s.name));
        make_image(&built, s.cmdline, &image)?;
        println!("== scenario {} (cmdline {:?}, {} boot(s))", s.name, s.cmdline, s.runs);
        for run_index in 1..=s.runs {
            let log_path = if s.runs == 1 {
                logs.join(format!("{}.log", s.name))
            } else {
                logs.join(format!("{}-boot{run_index}.log", s.name))
            };
            let run = run_qemu_capture_typing(
                &LAB,
                &image,
                &data_image,
                &log_path,
                &["[kernel] shutdown requested", "spacekernel: halted after panic", "[init] TESTS FAILED"],
                s.typing,
                s.type_lines,
                s.ready_marker,
            )?;
            let problems = check_run(s, &run, run_index == s.runs);
            if problems.is_empty() {
                println!(
                    "   PASS boot {run_index}/{} in {:.1}s (exit {:?}), log: {}{}",
                    s.runs,
                    run.elapsed.as_secs_f64(),
                    run.exit_code,
                    log_path.display(),
                    tcp_summary(&run)
                );
                continue;
            }
            failures += 1;
            println!(
                "   FAIL boot {run_index}/{} in {:.1}s, log: {}",
                s.runs,
                run.elapsed.as_secs_f64(),
                log_path.display()
            );
            for p in problems {
                println!("     - {p}");
            }
            println!("----- last 60 log lines -----");
            for l in run.log.lines().rev().take(60).collect::<Vec<_>>().into_iter().rev() {
                println!("{l}");
            }
            println!("-----------------------------");
            break;
        }
    }
    if failures == 0 {
        println!("== all {ran} scenarios passed");
        Ok(())
    } else {
        Err(format!("{failures} scenario(s) failed"))
    }
}

/// Boot the acceptance image on every machine of the compatibility matrix.
///
/// The lab profile is what the acceptance run uses; this walks the variations a
/// user is likely to have (another chipset, fewer cores, less memory, a
/// transitional virtio device, no disk, no display) and checks that the same image
/// still reaches the end - degrading where hardware is missing, never crashing.
fn cmd_compat(release: bool, only: Option<&str>) -> Result<(), String> {
    const COMMON: &[&str] =
        &["[kernel] selftest: heap ok", "[init] Space OS init running", "[init] ALL TESTS PASSED"];
    const FORBIDDEN: &[&str] = &["KERNEL PANIC", "[init] FAIL", "TESTS FAILED"];

    if let Some(name) = only
        && !MACHINES.iter().any(|m| m.name == name)
    {
        return Err(format!("no machine named {name:?}"));
    }
    let built = build(release)?;
    let logs = root().join("build/logs/compat");
    fs::create_dir_all(&logs).map_err(|e| e.to_string())?;
    let image = root().join("build/esp-compat.img");
    make_image(&built, "", &image)?;
    let data_image = root().join("build/data.img");
    make_data_disk(&data_image)?;
    let gpt_image = root().join("build/data-gpt.img");
    let selected = || MACHINES.iter().filter(|m| only.is_none_or(|n| n == m.name));
    if selected().any(|m| m.data_disk == DataDisk::Gpt) {
        make_data_disk_gpt(&data_image, &gpt_image)?;
    }

    let mut failures = 0;
    let mut ran = 0;
    for m in selected() {
        ran += 1;
        println!(
            "== machine {}: -machine {} -cpu {} -smp {} -m {} ({}){}",
            m.name,
            m.machine,
            m.cpu,
            m.smp,
            m.memory,
            if m.block_device.is_empty() { "no block device" } else { m.block_device },
            if m.extra.is_empty() { String::new() } else { format!(" {}", m.extra.join(" ")) }
        );
        let log_path = logs.join(format!("{}.log", m.name));
        let run = run_qemu_capture_on(
            m,
            &image,
            if m.data_disk == DataDisk::Gpt { &gpt_image } else { &data_image },
            &log_path,
            &["[kernel] shutdown requested", "spacekernel: halted after panic", "[init] TESTS FAILED"],
        )?;
        let mut problems = Vec::new();
        if run.timed_out {
            problems.push(format!("timed out after {:?}", run.elapsed));
        }
        if run.exit_code != Some(EXIT_SUCCESS) {
            problems.push(format!("QEMU exit code {:?}, expected {EXIT_SUCCESS}", run.exit_code));
        }
        for marker in COMMON.iter().chain(m.must_contain.iter()) {
            if !run.log.contains(marker) {
                problems.push(format!("missing marker {marker:?}"));
            }
        }
        for marker in FORBIDDEN {
            if run.log.contains(marker) {
                problems.push(format!("unexpected marker {marker:?}"));
            }
        }
        problems.extend(tcp_problems(&run));
        if problems.is_empty() {
            println!(
                "   PASS in {:.1}s (exit {:?}), log: {}{}",
                run.elapsed.as_secs_f64(),
                run.exit_code,
                log_path.display(),
                tcp_summary(&run)
            );
            continue;
        }
        failures += 1;
        println!("   FAIL in {:.1}s, log: {}", run.elapsed.as_secs_f64(), log_path.display());
        for p in problems {
            println!("     - {p}");
        }
        println!("----- last 40 log lines -----");
        for l in run.log.lines().rev().take(40).collect::<Vec<_>>().into_iter().rev() {
            println!("{l}");
        }
        println!("-----------------------------");
    }
    if failures == 0 {
        println!("== all {ran} machine configurations booted and passed");
        Ok(())
    } else {
        Err(format!("{failures} machine configuration(s) failed"))
    }
}

fn cmd_soak(boots: u32, release: bool) -> Result<(), String> {
    let built = build(release)?;
    let s = &SCENARIOS[0];
    let image = root().join("build/esp-soak.img");
    make_image(&built, s.cmdline, &image)?;
    let data_image = root().join("build/data.img");
    make_data_disk(&data_image)?;
    let logs = root().join("build/logs/soak");
    fs::create_dir_all(&logs).map_err(|e| e.to_string())?;
    let mut times = Vec::new();
    let start = Instant::now();
    for i in 1..=boots {
        let log_path = logs.join(format!("boot-{i:03}.log"));
        let run = run_qemu_capture(
            &image,
            &data_image,
            &log_path,
            &["[kernel] shutdown requested", "spacekernel: halted after panic"],
        )?;
        let problems = check_run(s, &run, true);
        if !problems.is_empty() {
            println!(
                "boot {i}/{boots}: FAIL after {:.1}s: {}",
                run.elapsed.as_secs_f64(),
                problems.join("; ")
            );
            return Err(format!("soak failed at boot {i}; see {}", log_path.display()));
        }
        times.push(run.elapsed.as_secs_f64());
        println!("boot {i}/{boots}: ok in {:.1}s", run.elapsed.as_secs_f64());
    }
    let n = times.len() as f64;
    let mean = times.iter().sum::<f64>() / n;
    let max = times.iter().cloned().fold(0.0, f64::max);
    let min = times.iter().cloned().fold(f64::MAX, f64::min);
    let summary = format!(
        "soak: {boots}/{boots} consecutive cold boots passed; per boot min {min:.1}s mean {mean:.1}s max {max:.1}s; total {:.0}s",
        start.elapsed().as_secs_f64()
    );
    println!("{summary}");
    fs::write(logs.join("summary.txt"), format!("{summary}\n")).ok();
    Ok(())
}

/// `cargo xtask stress --minutes N`: the stability run of PRD §9 (ADR-0019).
///
/// One boot, the acceptance suite again and again for N minutes with random kills
/// between passes, and the machine's memory held to where the first pass left it.
/// Everything a run of hours depends on is its own: a copy of this program (every
/// lab connection starts it again, and a rebuild must not change what answers), a
/// lab authority, a data disk and firmware variables under `build/stress/`, so
/// development can go on beside it.
fn cmd_stress(minutes: u64, release: bool) -> Result<(), String> {
    let dir = root().join("build/stress");
    if std::env::var_os("SPACEOS_STRESS_COPY").is_none() {
        fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
        let me = std::env::current_exe().map_err(|e| format!("where am I: {e}"))?;
        let copy = dir.join("xtask");
        fs::copy(&me, &copy).map_err(|e| format!("copy {}: {e}", me.display()))?;
        // Ahead of whatever else the host is doing: a vCPU that waits behind a compile
        // loses tens of milliseconds at a time, and the suite's timing checks would
        // count that against the guest. Raising priority needs root; without it `nice`
        // says so and runs the copy as it is.
        let run = |cmd: &mut Command| {
            cmd.args(std::env::args_os().skip(1)).env("SPACEOS_STRESS_COPY", "1").status()
        };
        let st = match run(Command::new("nice").args(["-n", "-10"]).arg(&copy)) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => run(&mut Command::new(&copy)),
            r => r,
        }
        .map_err(|e| format!("cannot start {}: {e}", copy.display()))?;
        return if st.success() { Ok(()) } else { Err(format!("the stability run failed ({st})")) };
    }
    // SAFETY: nothing else runs in this process yet; the lab services QEMU starts
    // inherit it, and so find the authority this run's disk was made with.
    unsafe { std::env::set_var("SPACEOS_LAB_PKI", dir.join("lab-pki")) };
    let built = build(release)?;
    let image = dir.join("esp.img");
    make_image(&built, &format!("stress={minutes}m"), &image)?;
    let data_image = dir.join("data.img");
    make_data_disk(&data_image)?;
    let log_path = dir.join("stress.log");
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    println!(
        "== stability run: {minutes} min on the lab machine; log {}, lab log {}",
        log_path.display(),
        log_path.with_extension("lab.log").display()
    );
    let t0 = Instant::now();
    let mut progress = |line: &str| {
        let t = t0.elapsed().as_secs();
        let stamp = format!("{:02}:{:02}:{:02}", t / 3600, t / 60 % 60, t % 60);
        if let Some(rest) = line.strip_prefix("[stress] ") {
            // The pass line ends in its figures; the harness keeps those for the CSV.
            println!("{stamp} {}", rest.split(" frames_free=").next().unwrap_or(rest));
        } else if line.contains("KERNEL PANIC") || line.contains("[init] FAIL") || line.contains("[churn]") {
            println!("{stamp} !! {line}");
        }
    };
    let run = boot_watched(
        &LAB,
        &image,
        &data_image,
        &log_path,
        Watch {
            done_markers: &[
                "[stress] STRESS PASSED",
                "[stress] STRESS FAILED",
                "spacekernel: halted after panic",
                "[kernel] shutdown requested",
            ],
            typing: Typing::None,
            type_lines: &[],
            ready_marker: "",
            // The last pass starts before the deadline and runs to its end.
            timeout: Duration::from_secs(minutes * 60 + 20 * 60),
            vars_copy: dir.join("OVMF_VARS.fd"),
            progress: Some(&mut progress),
            host: Some((Duration::from_secs(60), exe)),
        },
    )?;
    let (summary, csv, problems) = stress_report(&run, minutes);
    fs::write(dir.join("summary.txt"), &summary).map_err(|e| e.to_string())?;
    fs::write(dir.join("memory.csv"), &csv).map_err(|e| e.to_string())?;
    println!("{summary}");
    println!(
        "== summary {}, memory per pass {}",
        dir.join("summary.txt").display(),
        dir.join("memory.csv").display()
    );
    if problems.is_empty() {
        println!("== stability run passed");
        Ok(())
    } else {
        for p in &problems {
            println!("   - {p}");
        }
        Err(format!("the stability run failed: {} problem(s)", problems.len()))
    }
}

/// One pass as its closing line reports it.
struct PassLine {
    pass: u64,
    ok: bool,
    passed: u64,
    total: u64,
    seconds: f64,
    /// frames_free, frames_free_min, heap_used, heap_used_peak, processes,
    /// threads, init_heap, handles, uptime_ms -- in that order.
    figures: [u64; 9],
}

const PASS_FIGURES: [&str; 9] = [
    "frames_free",
    "frames_free_min",
    "heap_used",
    "heap_used_peak",
    "processes",
    "threads",
    "init_heap",
    "handles",
    "uptime_ms",
];

fn parse_pass_line(line: &str) -> Option<PassLine> {
    let rest = line.strip_prefix("[stress] pass ")?;
    let (num, rest) = rest.split_once(": ")?;
    let pass = num.parse().ok()?;
    let ok = rest.starts_with("ok;");
    let tests = rest.split("; ").find(|p| p.contains(" tests"))?;
    let (counts, time) = tests.split_once(" tests")?;
    let (passed, total) = counts.trim().split_once('/')?;
    let seconds = time.rsplit_once(" in ")?.1.trim_end_matches(" s").parse().ok()?;
    let mut figures = [0u64; 9];
    for (i, key) in PASS_FIGURES.iter().enumerate() {
        let at = line.find(&format!(" {key}="))? + key.len() + 2;
        figures[i] = line[at..].split_whitespace().next()?.parse().ok()?;
    }
    Some(PassLine { pass, ok, passed: passed.parse().ok()?, total: total.parse().ok()?, seconds, figures })
}

/// The run's record: a summary for people, one CSV row per pass, and what failed.
fn stress_report(run: &QemuRun, minutes: u64) -> (String, String, Vec<String>) {
    let mut problems = Vec::new();
    if run.timed_out {
        problems.push(format!("timed out after {:?}", run.elapsed));
    }
    if run.exit_code != Some(EXIT_SUCCESS) {
        problems.push(format!("QEMU exit code {:?}, expected {EXIT_SUCCESS}", run.exit_code));
    }
    if !run.log.contains("[stress] STRESS PASSED") {
        problems.push(String::from("the guest never said STRESS PASSED"));
    }
    for bad in ["KERNEL PANIC", "[init] FAIL", "STRESS FAILED", "LEAK", "[churn]", "[qemu stderr]"] {
        if let Some(line) = run.log.lines().find(|l| l.contains(bad)) {
            problems.push(format!("the log has {line:?}"));
        }
    }
    for bad in LAB_FORBIDDEN {
        let n = run.lab_log.lines().filter(|l| l.contains(bad)).count();
        if n > 0 {
            problems.push(format!("the lab services wrote {bad:?} {n} time(s)"));
        }
    }
    // A live connection's FIN left unanswered is the guest's TCP, however long the run.
    problems.extend(tcp_problems(run));
    let passes: Vec<PassLine> = run.log.lines().filter_map(parse_pass_line).collect();
    let reported =
        run.log.lines().filter(|l| l.starts_with("[stress] pass ") && l.contains(" frames_free=")).count();
    if reported != passes.len() {
        problems.push(format!("{} of {reported} pass lines could not be read", reported - passes.len()));
    }
    let mut csv = String::from("pass,ok,tests_passed,tests,seconds");
    for key in PASS_FIGURES {
        csv.push(',');
        csv.push_str(key);
    }
    csv.push('\n');
    for p in &passes {
        csv.push_str(&format!("{},{},{},{},{:.1}", p.pass, p.ok as u8, p.passed, p.total, p.seconds));
        for f in p.figures {
            csv.push_str(&format!(",{f}"));
        }
        csv.push('\n');
    }
    // Held to pass 1 independently of the guest's own check: free frames, kernel
    // heap, processes, threads, init's heap and handles.
    let first = passes.first();
    let drift: Vec<String> = passes
        .iter()
        .filter(|p| first.is_some_and(|f| [0, 2, 4, 5, 6, 7].iter().any(|&i| p.figures[i] != f.figures[i])))
        .map(|p| format!("pass {}", p.pass))
        .collect();
    if !drift.is_empty() {
        problems.push(format!("memory differs from pass 1 after {}", drift.join(", ")));
    }
    if passes.iter().any(|p| !p.ok || p.passed != p.total) {
        problems.push(String::from("a pass did not end ok"));
    }
    let guest_minutes = passes.last().map_or(0.0, |p| p.figures[8] as f64 / 60_000.0);
    if guest_minutes < minutes as f64 {
        problems.push(format!("the guest ran {guest_minutes:.1} of {minutes} minutes"));
    }
    let mut out = String::new();
    let done =
        run.log.lines().find(|l| l.starts_with("[stress] done:")).unwrap_or("[stress] done: (missing)");
    out.push_str(&format!("stability run, {minutes} min requested; the guest's own account:\n  {done}\n"));
    if let (Some(f), Some(l)) = (passes.first(), passes.last()) {
        let tests: u64 = passes.iter().map(|p| p.passed).sum();
        let (mut min_s, mut max_s, mut sum_s) = (f64::MAX, 0.0f64, 0.0);
        for p in &passes {
            min_s = min_s.min(p.seconds);
            max_s = max_s.max(p.seconds);
            sum_s += p.seconds;
        }
        let lowest = passes.iter().map(|p| p.figures[1]).min().unwrap_or(0);
        let peak = passes.iter().map(|p| p.figures[3]).max().unwrap_or(0);
        out.push_str(&format!(
            "passes: {} ({} ok), {tests} tests passed; {:.1} h of guest time; a pass took {min_s:.1}-{max_s:.1} s, {:.1} s on average\n",
            passes.len(),
            passes.iter().filter(|p| p.ok).count(),
            l.figures[8] as f64 / 3_600_000.0,
            sum_s / passes.len() as f64
        ));
        out.push_str(&format!(
            "memory after every pass, {}: {} free frames ({} MiB), {} kernel heap bytes, {} process(es), {} thread(s), init heap {} bytes, {} init handles\n",
            if drift.is_empty() { "identical to pass 1" } else { "NOT constant" },
            f.figures[0],
            f.figures[0] * 4 / 1024,
            f.figures[2],
            f.figures[4],
            f.figures[5],
            f.figures[6],
            f.figures[7]
        ));
        out.push_str(&format!(
            "memory during the run: at least {lowest} frames free ({} MiB, {} MiB below the end-of-pass level); kernel heap at most {peak} bytes\n",
            lowest * 4 / 1024,
            f.figures[0].saturating_sub(lowest) * 4 / 1024
        ));
    }
    let killed = run
        .log
        .lines()
        .filter(|l| l.starts_with("[stress] pass ") && l.contains(" chaos: ") && l.contains("network back"))
        .count();
    out.push_str(&format!(
        "chaos: the network service was killed mid-transfer and came back {killed} time(s)\n"
    ));
    match &run.tcp {
        Some(Ok(r)) => out.push_str(&format!(
            "tcp: {} connections, {} closed cleanly, {} reset, {} segment(s) the peer sent again; {} peer FIN(s) never answered on a live connection, {} connection(s) left behind by a killed network service and never answered\n",
            r.connections,
            r.closed_cleanly,
            r.reset,
            r.peer_retransmits,
            r.unacked_fins.len(),
            r.abandoned.len()
        )),
        Some(Err(e)) => out.push_str(&format!("tcp: the capture could not be read: {e}\n")),
        None => {}
    }
    let lab_lines = run.lab_log.lines().count();
    let quiet = run.lab_log.lines().filter(|l| l.contains("nothing from the guest for")).count();
    out.push_str(&format!(
        "lab: {lab_lines} lines from the services; {quiet} echo connection(s) left open by a killed network service, closed after 30 s idle; none of {LAB_FORBIDDEN:?}\n"
    ));
    if let Some(h) = run.host {
        out.push_str(&format!(
            "host: QEMU held at most {} file descriptors ({} at the last of {} samples) and {} MiB resident ({} MiB last); at most {} lab services alive at once\n",
            h.fds_max,
            h.fds_last,
            h.samples,
            h.rss_kib_max / 1024,
            h.rss_kib_last / 1024,
            h.lab_max
        ));
    }
    if problems.is_empty() {
        out.push_str("verdict: passed\n");
    } else {
        out.push_str("verdict: FAILED\n");
        for p in &problems {
            out.push_str(&format!("  - {p}\n"));
        }
    }
    (out, csv, problems)
}

/// Boot the image the way a person would.
///
/// A session that can be typed at needs a way in, and there are exactly two:
/// `--gui` opens a window whose keyboard is the emulated PS/2 controller, and
/// `--serial-input` adds COM2 as a pty for a headless machine. Without either, the
/// guest has no input path at all -- COM1 carries the log out and nothing comes back
/// in -- so a session would sit at its prompt for ever.
fn cmd_run(gui: bool, serial_input: bool, cmdline: &str, release: bool) -> Result<(), String> {
    let built = build(release)?;
    let image = root().join("build/esp-run.img");
    make_image(&built, cmdline, &image)?;
    let data_image = root().join("build/data.img");
    make_data_disk(&data_image)?;
    let (code, vars) = find_firmware()?;
    let vars_copy = root().join("build/OVMF_VARS.fd");
    fs::copy(&vars, &vars_copy).map_err(|e| e.to_string())?;
    let typing = if serial_input { Typing::Serial } else { Typing::None };
    let pcap = std::env::var_os("SPACEOS_PCAP").map(PathBuf::from);
    let args =
        qemu_args(&LAB, &image, &data_image, &vars_copy, &code, gui, None, typing, None, pcap.as_deref())?;
    println!("== {} {}", qemu_bin(), args.join(" "));
    if serial_input {
        println!("== COM2 is a pty; QEMU prints its path below. Type into it with e.g. `screen <pty>`");
    } else if !gui && cmdline.contains("init=bin/space") {
        println!(
            "== note: this guest has no input path. Add --gui for a keyboard, or --serial-input for COM2"
        );
    }
    let st = Command::new(qemu_bin()).args(&args).status().map_err(|e| e.to_string())?;
    println!(
        "== qemu exited with {:?} (33 = clean shutdown, 35 = tests failed, 127 = kernel panic)",
        st.code()
    );
    Ok(())
}

/// Score candidate seeds for the reference model: an untrained model can collapse
/// into one repeated token, which would make the pinned baseline a weak check.
fn cmd_seedsearch() -> Result<(), String> {
    let header = reference::config();
    let mut best: Vec<(usize, usize, u64)> = Vec::new();
    for seed in 1..=120u64 {
        let seed = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ 0x5061_6365_4F53_0000;
        let w = reference::weights_seeded(&header, seed);
        let tokens = reference::generate(&header, &w, reference::PROMPT, reference::GENERATE);
        let mut sorted = tokens.clone();
        sorted.sort_unstable();
        sorted.dedup();
        let mut longest = 1usize;
        let mut run = 1usize;
        for i in 1..tokens.len() {
            if tokens[i] == tokens[i - 1] {
                run += 1;
                longest = longest.max(run);
            } else {
                run = 1;
            }
        }
        best.push((sorted.len(), longest, seed));
    }
    // Most distinct tokens first, then the shortest longest-run.
    best.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    for (distinct, longest, seed) in best.iter().take(8) {
        println!("seed {seed:#018x}: {distinct} distinct, longest run {longest}");
    }
    Ok(())
}

fn cmd_clippy() -> Result<(), String> {
    sh(cargo().args([
        "clippy",
        "-p",
        "spaceabi",
        "-p",
        "spaceboot",
        "--target",
        "x86_64-unknown-uefi",
        "--target-dir",
        "target/boot",
        "--",
        "-D",
        "warnings",
    ]))?;
    sh(cargo().args([
        "clippy",
        "-p",
        "spacekernel",
        "--target",
        "x86_64-unknown-none",
        "--target-dir",
        "target/kernel",
        "--",
        "-D",
        "warnings",
    ]))?;
    let mut c = cargo();
    c.args(["clippy", "--target", "x86_64-unknown-none", "--target-dir", "target/user"]);
    for p in USER_PROGRAMS {
        c.args(["-p", &package_spec(p)]);
    }
    c.args(["-p", "libspace", "-p", "spacetls", "--", "-D", "warnings"]);
    sh(&mut c)?;
    sh(cargo().args(["clippy", "-p", "xtask", "--", "-D", "warnings"]))
}

/// Host-side unit tests: the shared ABI crate (message layouts, the DNS codec) and
/// the build tool itself (the lab DNS server among them).
fn cmd_unit() -> Result<(), String> {
    sh(cargo().args(["test", "-p", "spaceabi", "-p", "xtask"]))
}

fn usage() -> ! {
    eprintln!(
        "usage: cargo xtask <build|run [--gui] [--cmdline S]|test|compat|soak [--boots N]|stress [--minutes N]|unit|tcpcheck FILE|clippy|fmt|fmt-check|ci> [--debug]"
    );
    std::process::exit(2)
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let cmd = args.first().map(String::as_str).unwrap_or("");
    // Started by QEMU, once per guest connection to a lab service (see `lab`).
    if cmd == "lab" {
        std::process::exit(lab::main(args.get(1).map(String::as_str)));
    }
    let release = !args.iter().any(|a| a == "--debug");
    // `--only NAME`: one scenario (test) or one machine (compat), for iterating.
    let only = args.iter().position(|a| a == "--only").and_then(|i| args.get(i + 1)).cloned();
    let res = match cmd {
        "build" | "image" => build(release)
            .and_then(|b| make_image(&b, "", &root().join("build/esp.img")))
            .and_then(|()| make_data_disk(&root().join("build/data.img"))),
        "run" => {
            let gui = args.iter().any(|a| a == "--gui");
            let serial_input = args.iter().any(|a| a == "--serial-input");
            let cmdline = args
                .iter()
                .position(|a| a == "--cmdline")
                .and_then(|i| args.get(i + 1))
                .cloned()
                .unwrap_or_default();
            cmd_run(gui, serial_input, &cmdline, release)
        }
        "test" => cmd_test(release, only.as_deref()),
        "compat" => cmd_compat(release, only.as_deref()),
        "soak" => {
            let boots = args
                .iter()
                .position(|a| a == "--boots")
                .and_then(|i| args.get(i + 1))
                .and_then(|s| s.parse().ok())
                .unwrap_or(100);
            cmd_soak(boots, release)
        }
        "stress" => {
            let minutes = args
                .iter()
                .position(|a| a == "--minutes")
                .and_then(|i| args.get(i + 1))
                .and_then(|s| s.parse().ok())
                .unwrap_or(480);
            cmd_stress(minutes, release)
        }
        "model" => make_data_disk(&root().join("build/data.img")),
        "seedsearch" => cmd_seedsearch(),
        "clippy" => cmd_clippy(),
        "fmt" => sh(cargo().args(["fmt", "--all"])),
        "fmt-check" => sh(cargo().args(["fmt", "--all", "--", "--check"])),
        "unit" => cmd_unit(),
        // Check a capture made earlier (e.g. with SPACEOS_PCAP during `run`).
        "tcpcheck" => match args.get(1) {
            Some(path) => pcap::analyze(Path::new(path), GUEST_IP).and_then(|r| {
                println!(
                    "{} connections, {} closed cleanly, {} reset, {} segment(s) the peer sent again{}",
                    r.connections,
                    r.closed_cleanly,
                    r.reset,
                    r.peer_retransmits,
                    abandoned_note(&r)
                );
                if r.unacked_fins.is_empty() {
                    Ok(())
                } else {
                    Err(format!("FINs never acknowledged by the guest: {}", r.unacked_fins.join(", ")))
                }
            }),
            None => usage(),
        },
        "ci" => sh(cargo().args(["fmt", "--all", "--", "--check"]))
            .and_then(|_| cmd_clippy())
            .and_then(|_| cmd_unit())
            .and_then(|_| cmd_test(true, None))
            .and_then(|_| cmd_compat(true, None)),
        _ => usage(),
    };
    if let Err(e) = res {
        eprintln!("xtask: {e}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The GPT data disk reads back as what it claims: both headers and the table
    /// check out, partition 2 holds the volume byte for byte, and partition 1 is a
    /// FAT32 volume with another label.
    #[test]
    fn gpt_data_disk_is_valid() {
        let dir = std::env::temp_dir().join(format!("spaceos-gpt-test-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let volume: Vec<u8> = (0..3 * 1024 * 1024 + 512).map(|i: u32| (i % 251) as u8).collect();
        let (vol_path, disk_path) = (dir.join("volume.img"), dir.join("disk.img"));
        fs::write(&vol_path, &volume).unwrap();
        make_data_disk_gpt(&vol_path, &disk_path).unwrap();
        let disk = fs::read(&disk_path).unwrap();
        fs::remove_dir_all(&dir).unwrap();

        let sector = |lba: u64| &disk[(lba * 512) as usize..(lba * 512 + 512) as usize];
        let le32 = |b: &[u8], at: usize| u32::from_le_bytes(b[at..at + 4].try_into().unwrap());
        let le64 = |b: &[u8], at: usize| u64::from_le_bytes(b[at..at + 8].try_into().unwrap());
        assert_eq!((disk[510], disk[511], disk[446 + 4]), (0x55, 0xAA, 0xEE), "protective MBR");
        let last = disk.len() as u64 / 512 - 1;
        for (at, other) in [(1, last), (last, 1)] {
            let h = sector(at);
            assert_eq!(&h[0..8], b"EFI PART");
            let mut copy = h[0..92].to_vec();
            copy[16..20].fill(0);
            assert_eq!(le32(h, 16), crc32(&copy), "header CRC at LBA {at}");
            assert_eq!((le64(h, 24), le64(h, 32)), (at, other));
            let table = le64(h, 72);
            let bytes = (le32(h, 80) * le32(h, 84)) as usize;
            let entries = &disk[(table * 512) as usize..(table * 512) as usize + bytes];
            assert_eq!(le32(h, 88), crc32(entries), "table CRC at LBA {at}");
            let (first, end) = (le64(entries, 128 + 32), le64(entries, 128 + 40));
            assert_eq!(&disk[(first * 512) as usize..(first * 512) as usize + volume.len()], &volume[..]);
            assert_eq!(end - first + 1, (volume.len() as u64).div_ceil(512));
            assert!(end < le64(h, 48) + 1, "partition 2 ends inside the usable area");
        }
        // Partition 1's entry is the first in the table at LBA 2.
        let esp = sector(le64(&disk[2 * 512..], 32));
        assert_eq!(&esp[82..90], b"FAT32   ");
        assert_eq!(&esp[71..82], b"SPACEOS    ");
    }

    /// The pass line as `init` prints it (user/init/src/stress.rs) reads back into
    /// the figures the memory record is made of.
    #[test]
    fn pass_lines_read_back() {
        let line = "[stress] pass 12: ok; 108/108 tests in 27.7 s; frames_free=2090105 \
                    frames_free_min=2089227 heap_used=9448 heap_used_peak=32240 processes=1 threads=1 \
                    init_heap=0 handles=1 uptime_ms=27664";
        let p = parse_pass_line(line).expect("a pass line");
        assert_eq!((p.pass, p.ok, p.passed, p.total), (12, true, 108, 108));
        assert!((p.seconds - 27.7).abs() < 1e-9);
        assert_eq!(p.figures, [2090105, 2089227, 9448, 32240, 1, 1, 0, 1, 27664]);

        let failed = "[stress] pass 3: 1 test(s) failed; LEAK against pass 1: free frames -1; 107/108 tests \
                      (1 skipped) in 30.0 s; frames_free=1 frames_free_min=1 heap_used=1 heap_used_peak=1 \
                      processes=1 threads=1 init_heap=0 handles=1 uptime_ms=90000";
        let p = parse_pass_line(failed).expect("a failed pass line");
        assert_eq!((p.ok, p.passed, p.total), (false, 107, 108));

        assert!(parse_pass_line("[stress] pass 3 chaos: 6 killed after 357 ms").is_none());
        assert!(parse_pass_line("[stress] pass 3 starting, 60 s into the run").is_none());
    }
}
