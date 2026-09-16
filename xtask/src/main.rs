//! `cargo xtask` – build, image, run and test driver for Space OS.
//!
//! Subcommands:
//!   build            cross-compile bootloader, kernel, user programs; build the ESP image
//!   run [--gui]      boot the image in QEMU (serial on stdio)
//!   test             boot scenarios and check the serial log + exit code
//!   soak [--boots N] N consecutive cold boots of the acceptance scenario (default 100)
//!   clippy | fmt | fmt-check | ci
//!
//! Environment: `SPACEOS_OVMF_CODE` / `SPACEOS_OVMF_VARS` override firmware discovery,
//! `SPACEOS_QEMU` overrides the QEMU binary.

mod reference;

use std::fs;
use std::io::{Read, Seek, SeekFrom, Write};
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
    "worker",
    "blocker",
    "spacecompute",
    "spaceai",
    "spaceshell",
    "uiworker",
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
        c.args(["-p", p]);
    }
    sh(&mut c)?;
    Ok(Built {
        boot_efi: r.join(format!("target/boot/x86_64-unknown-uefi/{dir}/spaceboot.efi")),
        kernel_elf: r.join(format!("target/kernel/x86_64-unknown-none/{dir}/spacekernel")),
        user_bins: USER_PROGRAMS
            .iter()
            .map(|p| (p.to_string(), r.join(format!("target/user/x86_64-unknown-none/{dir}/{p}"))))
            .collect(),
    })
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
    }
    disk.flush().map_err(|e| e.to_string())?;
    println!(
        "== data disk {} ({} KiB model + baseline + 3 malformed fixtures, FAT32)",
        out.display(),
        model.len() / 1024
    );
    Ok(())
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
    /// Appended verbatim (display and other knobs).
    extra: &'static [&'static str],
    /// Markers this configuration must produce on top of the common ones.
    must_contain: &'static [&'static str],
}

const LAB: Machine = Machine {
    name: "lab",
    machine: "q35,accel=tcg",
    cpu: "qemu64",
    smp: "4",
    memory: "8G",
    block_device: "virtio-blk-pci,drive=spacedata,disable-legacy=on",
    extra: &[],
    must_contain: &[],
};

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
        extra: &[],
        must_contain: &["[kernel] vfs: FAT32 mounted", "[init] ALL TESTS PASSED"],
    },
    Machine {
        name: "i440fx",
        machine: "pc,accel=tcg",
        cpu: "qemu64",
        smp: "2",
        memory: "4G",
        block_device: "virtio-blk-pci,drive=spacedata,disable-legacy=on",
        extra: &[],
        must_contain: &["[kernel] vfs: FAT32 mounted", "[init] ALL TESTS PASSED"],
    },
    Machine {
        name: "cpu-max",
        machine: "q35,accel=tcg",
        cpu: "max",
        smp: "4",
        memory: "8G",
        block_device: "virtio-blk-pci,drive=spacedata,disable-legacy=on",
        extra: &[],
        must_contain: &["[init] ALL TESTS PASSED"],
    },
    Machine {
        name: "virtio-transitional",
        machine: "q35,accel=tcg",
        cpu: "qemu64",
        smp: "4",
        memory: "8G",
        // A transitional device answers to the legacy id 0x1001 and still offers the
        // modern capabilities; the driver must negotiate VIRTIO_F_VERSION_1 on it.
        block_device: "virtio-blk-pci,drive=spacedata,disable-legacy=off,disable-modern=off",
        extra: &[],
        must_contain: &["[kernel] vfs: FAT32 mounted", "[init] ALL TESTS PASSED"],
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
        extra: &[],
        must_contain: &[
            "[kernel] virtio-blk: pci 00:03.0 queue size 4 (max 4), 2 data pages/request",
            "[kernel] vfs: FAT32 mounted",
            "[init] ALL TESTS PASSED",
        ],
    },
    Machine {
        name: "no-disk",
        machine: "q35,accel=tcg",
        cpu: "qemu64",
        smp: "4",
        memory: "8G",
        block_device: "",
        extra: &[],
        // No storage is a supported configuration: the kernel says so and the
        // disk-backed tests are skipped instead of failing.
        must_contain: &[
            "[kernel] virtio-blk: no device present",
            "[kernel] vfs: no block device",
            "[init] storage: none",
            "[init] SKIP A01",
            "[init] ALL TESTS PASSED",
        ],
    },
    Machine {
        name: "no-vga",
        machine: "q35,accel=tcg",
        cpu: "qemu64",
        smp: "4",
        memory: "8G",
        block_device: "virtio-blk-pci,drive=spacedata,disable-legacy=on",
        extra: &["-vga", "none"],
        // Without a GOP the console has to fall back to serial only.
        must_contain: &["[kernel] framebuffer: none usable; serial console only", "[init] ALL TESTS PASSED"],
    },
    Machine {
        name: "vmware-vga",
        machine: "q35,accel=tcg",
        cpu: "qemu64",
        smp: "4",
        memory: "8G",
        block_device: "virtio-blk-pci,drive=spacedata,disable-legacy=on",
        extra: &["-vga", "vmware"],
        must_contain: &["[kernel] framebuffer:", "[init] ALL TESTS PASSED"],
    },
];

/// QEMU command line for `m`, booting `image` with `data_image` attached.
fn qemu_args(
    m: &Machine,
    image: &Path,
    data_image: &Path,
    vars_copy: &Path,
    code: &Path,
    gui: bool,
    serial_path: Option<&Path>,
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
    a.extend([
        "-device".into(),
        "isa-debug-exit,iobase=0xf4,iosize=0x04".into(),
        "-no-reboot".into(),
        "-rtc".into(),
        "base=utc".into(),
    ]);
    a.extend(m.extra.iter().map(|s| (*s).to_string()));
    match serial_path {
        Some(p) => a.extend(["-serial".into(), format!("file:{}", p.display())]),
        None => a.extend(["-serial".into(), "mon:stdio".into()]),
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
    run_qemu_capture_on(&LAB, image, data_image, log_path, done_markers)
}

fn run_qemu_capture_on(
    machine: &Machine,
    image: &Path,
    data_image: &Path,
    log_path: &Path,
    done_markers: &[&str],
) -> Result<QemuRun, String> {
    let (code, vars) = find_firmware()?;
    let vars_copy = root().join("build/OVMF_VARS.fd");
    fs::copy(&vars, &vars_copy).map_err(|e| format!("copy OVMF vars: {e}"))?;
    if log_path.exists() {
        fs::remove_file(log_path).ok();
    }
    let args = qemu_args(machine, image, data_image, &vars_copy, &code, false, Some(log_path))?;
    let start = Instant::now();
    let mut child = Command::new(qemu_bin())
        .args(&args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("cannot start {}: {e}", qemu_bin()))?;
    let mut timed_out = false;
    let exit_code = loop {
        if let Some(st) = child.try_wait().map_err(|e| e.to_string())? {
            break st.code();
        }
        if start.elapsed() > BOOT_TIMEOUT {
            timed_out = true;
            child.kill().ok();
            child.wait().ok();
            break None;
        }
        // Early exit once the kernel reported a terminal state but QEMU lingers.
        let finished =
            fs::read_to_string(log_path).map(|s| done_markers.iter().any(|m| s.contains(m))).unwrap_or(false);
        if finished && start.elapsed() > Duration::from_secs(5) {
            // give the debug-exit a moment, then stop
            std::thread::sleep(Duration::from_millis(3000));
            if child.try_wait().map_err(|e| e.to_string())?.is_none() {
                child.kill().ok();
                child.wait().ok();
                break None;
            }
        }
        std::thread::sleep(Duration::from_millis(100));
    };
    let mut stderr = String::new();
    if let Some(mut e) = child.stderr.take() {
        e.read_to_string(&mut stderr).ok();
    }
    let mut log = fs::read_to_string(log_path).unwrap_or_default();
    if !stderr.trim().is_empty() {
        log.push_str("\n[qemu stderr]\n");
        log.push_str(&stderr);
        fs::write(log_path, &log).ok();
    }
    Ok(QemuRun { log, exit_code, elapsed: start.elapsed(), timed_out })
}

struct Scenario {
    name: &'static str,
    cmdline: &'static str,
    expect_exit: i32,
    must_contain: &'static [&'static str],
    must_not_contain: &'static [&'static str],
    /// Consecutive boots of the same image that must all pass (D01 asks for the
    /// checksum to hold after a reboot).
    runs: u32,
}

/// QEMU exit status = (value << 1) | 1 for isa-debug-exit.
const EXIT_SUCCESS: i32 = (0x10 << 1) | 1; // 33
#[allow(dead_code)]
const EXIT_FAILURE: i32 = (0x11 << 1) | 1; // 35
const EXIT_PANIC: i32 = (0x3f << 1) | 1; // 127

const SCENARIOS: &[Scenario] = &[
    Scenario {
        name: "acceptance",
        cmdline: "",
        expect_exit: EXIT_SUCCESS,
        must_contain: &[
            "[kernel] selftest: heap ok",
            "[kernel] virtio-blk: ready",
            "[kernel] vfs: FAT32 mounted",
            "[init] Space OS init running",
            "[ai] model verified: sha256",
            "[ai] generated 128 tokens offline, all matching the pinned baseline",
            "[init] ALL TESTS PASSED",
        ],
        must_not_contain: &["KERNEL PANIC", "[init] FAIL", "TESTS FAILED"],
        runs: 1,
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
        must_not_contain: &["[init] Space OS init running"],
        runs: 1,
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
        must_not_contain: &["[init] Space OS init running"],
        runs: 1,
    },
    Scenario {
        name: "kernel-stack-overflow-diagnosis",
        cmdline: "selftest=stack",
        expect_exit: EXIT_PANIC,
        must_contain: &["!!! CPU EXCEPTION IN KERNEL MODE: double fault !!!", "!!! KERNEL PANIC !!!"],
        must_not_contain: &["[init] Space OS init running"],
        runs: 1,
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
        must_not_contain: &["KERNEL PANIC", "[init] FAIL"],
        runs: 2,
    },
];

fn check_run(s: &Scenario, run: &QemuRun) -> Vec<String> {
    let mut problems = Vec::new();
    if run.timed_out {
        problems.push(format!("timed out after {:?}", run.elapsed));
    }
    if run.exit_code != Some(s.expect_exit) {
        problems.push(format!("QEMU exit code {:?}, expected {}", run.exit_code, s.expect_exit));
    }
    for m in s.must_contain {
        if !run.log.contains(m) {
            problems.push(format!("missing marker {m:?}"));
        }
    }
    for m in s.must_not_contain {
        if run.log.contains(m) {
            problems.push(format!("unexpected marker {m:?}"));
        }
    }
    problems
}

fn cmd_test(release: bool) -> Result<(), String> {
    let built = build(release)?;
    let logs = root().join("build/logs");
    fs::create_dir_all(&logs).map_err(|e| e.to_string())?;
    let data_image = root().join("build/data.img");
    make_data_disk(&data_image)?;
    let mut failures = 0;
    for s in SCENARIOS {
        let image = root().join(format!("build/esp-{}.img", s.name));
        make_image(&built, s.cmdline, &image)?;
        println!("== scenario {} (cmdline {:?}, {} boot(s))", s.name, s.cmdline, s.runs);
        for run_index in 1..=s.runs {
            let log_path = if s.runs == 1 {
                logs.join(format!("{}.log", s.name))
            } else {
                logs.join(format!("{}-boot{run_index}.log", s.name))
            };
            let run = run_qemu_capture(
                &image,
                &data_image,
                &log_path,
                &["[kernel] shutdown requested", "spacekernel: halted after panic", "[init] TESTS FAILED"],
            )?;
            let problems = check_run(s, &run);
            if problems.is_empty() {
                println!(
                    "   PASS boot {run_index}/{} in {:.1}s (exit {:?}), log: {}",
                    s.runs,
                    run.elapsed.as_secs_f64(),
                    run.exit_code,
                    log_path.display()
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
        println!("== all {} scenarios passed", SCENARIOS.len());
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
fn cmd_compat(release: bool) -> Result<(), String> {
    const COMMON: &[&str] =
        &["[kernel] selftest: heap ok", "[init] Space OS init running", "[init] ALL TESTS PASSED"];
    const FORBIDDEN: &[&str] = &["KERNEL PANIC", "[init] FAIL", "TESTS FAILED"];

    let built = build(release)?;
    let logs = root().join("build/logs/compat");
    fs::create_dir_all(&logs).map_err(|e| e.to_string())?;
    let image = root().join("build/esp-compat.img");
    make_image(&built, "", &image)?;
    let data_image = root().join("build/data.img");
    make_data_disk(&data_image)?;

    let mut failures = 0;
    for m in MACHINES {
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
            &data_image,
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
        if problems.is_empty() {
            println!(
                "   PASS in {:.1}s (exit {:?}), log: {}",
                run.elapsed.as_secs_f64(),
                run.exit_code,
                log_path.display()
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
        println!("== all {} machine configurations booted and passed", MACHINES.len());
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
        let problems = check_run(s, &run);
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

fn cmd_run(gui: bool, cmdline: &str, release: bool) -> Result<(), String> {
    let built = build(release)?;
    let image = root().join("build/esp-run.img");
    make_image(&built, cmdline, &image)?;
    let data_image = root().join("build/data.img");
    make_data_disk(&data_image)?;
    let (code, vars) = find_firmware()?;
    let vars_copy = root().join("build/OVMF_VARS.fd");
    fs::copy(&vars, &vars_copy).map_err(|e| e.to_string())?;
    let args = qemu_args(&LAB, &image, &data_image, &vars_copy, &code, gui, None)?;
    println!("== {} {}", qemu_bin(), args.join(" "));
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
        c.args(["-p", p]);
    }
    c.args(["-p", "libspace", "--", "-D", "warnings"]);
    sh(&mut c)?;
    sh(cargo().args(["clippy", "-p", "xtask", "--", "-D", "warnings"]))
}

fn usage() -> ! {
    eprintln!(
        "usage: cargo xtask <build|run [--gui] [--cmdline S]|test|compat|soak [--boots N]|clippy|fmt|fmt-check|ci> [--debug]"
    );
    std::process::exit(2)
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let release = !args.iter().any(|a| a == "--debug");
    let cmd = args.first().map(String::as_str).unwrap_or("");
    let res = match cmd {
        "build" | "image" => build(release)
            .and_then(|b| make_image(&b, "", &root().join("build/esp.img")))
            .and_then(|()| make_data_disk(&root().join("build/data.img"))),
        "run" => {
            let gui = args.iter().any(|a| a == "--gui");
            let cmdline = args
                .iter()
                .position(|a| a == "--cmdline")
                .and_then(|i| args.get(i + 1))
                .cloned()
                .unwrap_or_default();
            cmd_run(gui, &cmdline, release)
        }
        "test" => cmd_test(release),
        "compat" => cmd_compat(release),
        "soak" => {
            let boots = args
                .iter()
                .position(|a| a == "--boots")
                .and_then(|i| args.get(i + 1))
                .and_then(|s| s.parse().ok())
                .unwrap_or(100);
            cmd_soak(boots, release)
        }
        "model" => make_data_disk(&root().join("build/data.img")),
        "seedsearch" => cmd_seedsearch(),
        "clippy" => cmd_clippy(),
        "fmt" => sh(cargo().args(["fmt", "--all"])),
        "fmt-check" => sh(cargo().args(["fmt", "--all", "--", "--check"])),
        "ci" => sh(cargo().args(["fmt", "--all", "--", "--check"]))
            .and_then(|_| cmd_clippy())
            .and_then(|_| cmd_test(true))
            .and_then(|_| cmd_compat(true)),
        _ => usage(),
    };
    if let Err(e) = res {
        eprintln!("xtask: {e}");
        std::process::exit(1);
    }
}
