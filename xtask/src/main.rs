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
        "== data disk {} ({} KiB model + baseline + 3 malformed fixtures + agent workspace + link corpus + 5 packages, FAT32)",
        out.display(),
        model.len() / 1024
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
    let (code, vars) = find_firmware()?;
    let vars_copy = root().join("build/OVMF_VARS.fd");
    fs::copy(&vars, &vars_copy).map_err(|e| format!("copy OVMF vars: {e}"))?;
    if log_path.exists() {
        fs::remove_file(log_path).ok();
    }
    // The monitor is how the harness reaches the keyboard and finds the pty; it is
    // only added for a scenario that types.
    let monitor = (typing != Typing::None).then(|| root().join("build/monitor.sock"));
    if let Some(path) = &monitor {
        fs::remove_file(path).ok();
    }
    let args = qemu_args(
        machine,
        image,
        data_image,
        &vars_copy,
        &code,
        false,
        Some(log_path),
        typing,
        monitor.as_deref(),
    )?;
    let start = Instant::now();
    let mut child = Command::new(qemu_bin())
        .args(&args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("cannot start {}: {e}", qemu_bin()))?;
    // The typist stops as soon as the guest is gone, so a boot that dies early fails
    // in seconds instead of waiting out the marker deadline.
    let guest_gone = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let typist = match &monitor {
        Some(path) if !type_lines.is_empty() => {
            let path = path.clone();
            let log = log_path.to_path_buf();
            let ready = ready_marker.to_string();
            let lines: Vec<String> = type_lines.iter().map(|l| (*l).to_string()).collect();
            let gone = guest_gone.clone();
            Some(std::thread::spawn(move || type_on_guest(typing, &path, &log, &ready, &lines, &gone)))
        }
        _ => None,
    };
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
    guest_gone.store(true, std::sync::atomic::Ordering::Relaxed);
    let typed = match typist.map(|t| t.join()) {
        None => Ok(()),
        Some(Ok(r)) => r,
        Some(Err(_)) => Err("the console typist thread panicked".to_string()),
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
    let input_error = typed.err();
    if let Some(e) = &input_error {
        log.push_str(&format!("\n[harness] console input failed: {e}\n"));
        fs::write(log_path, &log).ok();
    }
    Ok(QemuRun { log, exit_code, elapsed: start.elapsed(), timed_out, input_error })
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

const SCENARIOS: &[Scenario] = &[
    Scenario {
        name: "acceptance",
        cmdline: "",
        expect_exit: EXIT_SUCCESS,
        must_contain: &[
            "[kernel] selftest: heap ok",
            "input decoding ok",
            "[kernel] virtio-blk: ready",
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
    let args = qemu_args(&LAB, &image, &data_image, &vars_copy, &code, gui, None, typing, None)?;
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
            let serial_input = args.iter().any(|a| a == "--serial-input");
            let cmdline = args
                .iter()
                .position(|a| a == "--cmdline")
                .and_then(|i| args.get(i + 1))
                .cloned()
                .unwrap_or_default();
            cmd_run(gui, serial_input, &cmdline, release)
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
