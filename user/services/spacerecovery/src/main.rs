//! `spacerecovery` – the recovery console (ADR-0027).
//!
//! What runs as pid 1 when the bootloader chose recovery: because the operator
//! pressed R, or because boots kept failing before the system came up. It needs
//! nothing that can fail on its own -- no model, no index, no desktop, no network,
//! no AI -- only the console and the data volume, and it lets a person find out
//! what is wrong and put the machine back in a state that boots:
//!
//! * `status`   what this machine has, and why recovery started
//! * `check`    the model against its manifest, the package store, the revocation
//!   list and the boot count, each with a verdict
//! * `repair packages` / `repair revocations`   empty a file that does not read
//!   back, so its service starts clean instead of failing on it
//! * `boot normal`   forget the failed boots: the next boot is a normal one
//! * `files [dir]`, `show FILE`   look around the volume
//! * `shell`    a full session (the same `spaceshell` as the terminal), then back
//! * `poweroff`
#![no_std]
#![no_main]

extern crate alloc;

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use libspace::shell::Session;
use libspace::spaceabi::error::Error;
use libspace::spaceabi::handle::rights;
use libspace::spaceabi::syscall::{DIR_ENTRIES_MAX, DirEntry};
use libspace::{Handle, boot, print, println, sha256, sys};

const ROOT: Handle = libspace::handle::BOOTSTRAP;
const MANIFEST_PATH: &str = "/spaceos/manifest.txt";
const STORE_PATH: &str = libspace::spaceabi::pkg::STORE_PATH;
const STORE_MAGIC: [u8; 8] = libspace::spaceabi::pkg::STORE_MAGIC;
const REVOKED_PATH: &str = libspace::spaceabi::link::REVOKED_PATH;
const SHELL_QUOTA: u64 = 256;
/// Longest command line kept; more is dropped as it is typed.
const LINE_MAX: usize = 96;

fn cmdline_value(key: &str) -> Option<String> {
    let mut buf = [0u8; 512];
    let n = sys::cmdline(ROOT, &mut buf).ok()?.min(buf.len());
    let line = core::str::from_utf8(&buf[..n]).ok()?;
    line.split_whitespace().find_map(|t| t.strip_prefix(key)).map(String::from)
}

/// Why this boot is a recovery boot, as the bootloader said on the command line.
fn why() -> &'static str {
    match cmdline_value("recovery=").as_deref() {
        Some("operator") => "the operator asked for it at the boot menu",
        Some("failed-boots") => "the boots before this one did not come up",
        _ => "the command line asked for it",
    }
}

fn read_all(path: &str, max: u64) -> Result<Vec<u8>, Error> {
    let f = sys::fs_open(ROOT, path)?;
    let r = (|| {
        let size = sys::fs_stat(f)?.size;
        if size > max {
            return Err(Error::MsgSize);
        }
        let mut out = alloc::vec![0u8; size as usize];
        let mut at = 0;
        while at < out.len() {
            let n = sys::fs_read(f, at as u64, &mut out[at..])?;
            if n == 0 {
                break;
            }
            at += n;
        }
        out.truncate(at);
        Ok(out)
    })();
    sys::handle_close(f).ok();
    r
}

fn status() {
    match sys::kstats(ROOT) {
        Ok(s) => {
            println!("[recovery] cpus: {}", s.cpus_online);
            println!("[recovery] memory: {} MiB free of {} MiB", s.frames_free / 256, s.frames_total / 256);
            if s.volume_sectors == 0 {
                println!("[recovery] data volume: none (no disk holds a volume labelled SPACEDATA)");
            } else {
                println!("[recovery] data volume: {} MiB", s.volume_sectors / 2048);
            }
        }
        Err(e) => println!("[recovery] kernel statistics unavailable: {e}"),
    }
    println!("[recovery] started because {}", why());
    match boot::count(ROOT) {
        Some(n) => println!("[recovery] boots since the system last came up: {n}"),
        None => println!("[recovery] the volume keeps no boot count"),
    }
}

/// The model the manifest names, against the manifest's size and SHA-256.
fn check_model() -> Result<String, String> {
    let manifest = read_all(MANIFEST_PATH, 4096).map_err(|e| format!("{MANIFEST_PATH}: {e}"))?;
    let manifest = core::str::from_utf8(&manifest).map_err(|_| format!("{MANIFEST_PATH} is not text"))?;
    let field = |k: &str| manifest.lines().find_map(|l| l.strip_prefix(k)).map(str::trim);
    let path = field("path=").ok_or_else(|| String::from("the manifest names no model"))?;
    let want = field("sha256=").ok_or_else(|| String::from("the manifest has no sha256"))?;
    let f = sys::fs_open(ROOT, path).map_err(|e| format!("{path}: {e}"))?;
    let mut hasher = sha256::Sha256::new();
    let mut buf = alloc::vec![0u8; 16 * 1024];
    let mut at = 0u64;
    let read = loop {
        match sys::fs_read(f, at, &mut buf) {
            Ok(0) => break Ok(()),
            Ok(n) => {
                hasher.update(&buf[..n]);
                at += n as u64;
            }
            Err(e) => break Err(format!("{path}: read at {at}: {e}")),
        }
    };
    sys::handle_close(f).ok();
    read?;
    let got = sha256::to_hex(&hasher.finish());
    let got = core::str::from_utf8(&got).unwrap_or("");
    if got == want {
        Ok(format!("{path}: {at} bytes, sha256 matches the manifest"))
    } else {
        Err(format!("{path}: sha256 {got}, the manifest says {want}"))
    }
}

fn check_store() -> Result<String, String> {
    match read_all(STORE_PATH, 1 << 20) {
        Err(Error::NotFound) => Ok(String::from("no package store yet")),
        Err(e) => Err(format!("{STORE_PATH}: {e}")),
        Ok(b) if b.is_empty() => Ok(String::from("the package store is empty")),
        Ok(b) if b.len() >= STORE_MAGIC.len() && b[..STORE_MAGIC.len()] == STORE_MAGIC => {
            Ok(format!("the package store reads ({} bytes)", b.len()))
        }
        Ok(b) => Err(format!("{STORE_PATH}: {} bytes that are not a package store", b.len())),
    }
}

fn check_revocations() -> Result<String, String> {
    match read_all(REVOKED_PATH, 64 * 1024) {
        Err(Error::NotFound) => Ok(String::from("nothing revoked yet")),
        Err(e) => Err(format!("{REVOKED_PATH}: {e}")),
        Ok(b) => match core::str::from_utf8(&b) {
            Ok(s) => {
                Ok(format!("{} revoked document(s)", s.lines().filter(|l| !l.trim().is_empty()).count()))
            }
            Err(_) => Err(format!("{REVOKED_PATH} is not text")),
        },
    }
}

fn check() {
    let mut problems = 0;
    for (what, verdict) in
        [("model", check_model()), ("packages", check_store()), ("revocations", check_revocations())]
    {
        match verdict {
            Ok(m) => println!("[recovery] check {what}: ok: {m}"),
            Err(m) => {
                problems += 1;
                println!("[recovery] check {what}: PROBLEM: {m}");
            }
        }
    }
    match boot::count(ROOT) {
        None | Some(0) => println!("[recovery] check boots: ok"),
        Some(n) => {
            println!("[recovery] check boots: {n} boot(s) did not come up; 'boot normal' clears the count")
        }
    }
    println!("[recovery] check done: {problems} problem(s)");
}

fn repair(what: &str) {
    let (path, verdict) = match what {
        "packages" => (STORE_PATH, check_store()),
        "revocations" => (REVOKED_PATH, check_revocations()),
        _ => {
            println!("[recovery] repair what? 'repair packages' or 'repair revocations'");
            return;
        }
    };
    if verdict.is_ok() {
        println!("[recovery] {what}: nothing to repair");
        return;
    }
    // Creating a file that exists empties it: the service reads "nothing yet".
    match sys::fs_create(ROOT, path) {
        Ok(f) => {
            sys::handle_close(f).ok();
            println!("[recovery] {what}: {path} emptied; the service starts clean next time");
        }
        Err(e) => println!("[recovery] {what}: cannot empty {path}: {e}"),
    }
}

fn files(dir: &str) {
    let mut entries = [DirEntry::default(); DIR_ENTRIES_MAX];
    match sys::fs_list(ROOT, dir, &mut entries) {
        Ok(n) => {
            for e in &entries[..n] {
                if e.is_dir != 0 {
                    println!("[recovery]   {}/", e.name());
                } else {
                    println!("[recovery]   {} ({} bytes)", e.name(), e.size);
                }
            }
            println!("[recovery] {n} entries in {dir}");
        }
        Err(e) => println!("[recovery] {dir}: {e}"),
    }
}

fn show(path: &str) {
    match read_all(path, 64 * 1024) {
        Ok(b) => {
            let text = String::from_utf8_lossy(&b[..b.len().min(2048)]);
            for line in text.lines().take(24) {
                println!("[recovery] | {line}");
            }
        }
        Err(e) => println!("[recovery] {path}: {e}"),
    }
}

/// A full session on this console, as `spaceterm` gives one; back here when it ends.
fn shell() {
    let run = || -> Result<(), String> {
        let (mine, theirs) = sys::channel_create().map_err(|e| format!("channel: {e}"))?;
        let child = sys::spawn(ROOT, "bin/spaceshell", SHELL_QUOTA, Some(theirs))
            .map_err(|e| format!("cannot start the session service: {e}"))?;
        let root = sys::handle_dup(
            ROOT,
            rights::SPAWN | rights::FS | rights::CONSOLE | rights::TRANSFER | rights::DUP,
        )
        .map_err(|e| format!("cannot narrow the root capability: {e}"));
        let opened =
            root.and_then(|r| Session::open(mine, r).map_err(|e| format!("cannot open the session: {e}")));
        if let Err(e) = opened {
            sys::kill(child).ok();
            return Err(e);
        }
        let status = sys::wait(child).map_err(|e| format!("wait: {e}"))?;
        sys::handle_close(mine).ok();
        println!("[recovery] session ended: {status:?}");
        Ok(())
    };
    if let Err(e) = run() {
        println!("[recovery] shell: {e}");
    }
}

/// Run one command; false to power off.
fn run(line: &str) -> bool {
    let mut words = line.split_whitespace();
    match (words.next(), words.next()) {
        (None, _) => {}
        (Some("help"), _) => println!(
            "[recovery] commands: status, check, repair packages|revocations, boot normal, files [dir], show FILE, shell, poweroff"
        ),
        (Some("status"), _) => status(),
        (Some("check"), _) => check(),
        (Some("repair"), what) => repair(what.unwrap_or("")),
        (Some("boot"), Some("normal")) => match boot::clear(ROOT) {
            Ok(()) => println!("[recovery] boot count cleared; the next boot is a normal one"),
            Err(e) => println!("[recovery] cannot clear the boot count: {e}"),
        },
        (Some("files"), dir) => files(dir.unwrap_or("/spaceos")),
        (Some("show"), Some(path)) => show(path),
        (Some("shell"), _) => shell(),
        (Some("poweroff" | "quit"), _) => return false,
        (Some(other), _) => println!("[recovery] unknown command '{other}'; type 'help'"),
    }
    true
}

fn prompt() {
    print!("recovery> ");
}

#[unsafe(no_mangle)]
pub extern "C" fn space_main() -> i32 {
    println!("[recovery] Space OS recovery console: no model, no network and no AI needed");
    println!("[recovery] started because {}", why());
    println!("[recovery] ready; type 'help'");
    prompt();
    let mut line = String::new();
    let mut last_cr = false;
    let mut typed = [0u8; 64];
    loop {
        let n = match sys::console_read(ROOT, &mut typed) {
            Ok(0) => {
                sys::sleep_ms(20);
                continue;
            }
            Ok(n) => n,
            // Bytes were dropped before anyone read them: the half-line has a hole.
            Err(Error::DataLoss) => {
                line.clear();
                println!();
                println!("[recovery] input was lost; the line was discarded");
                prompt();
                continue;
            }
            Err(e) => {
                println!("[recovery] no console ({e}); powering off");
                let _ = sys::shutdown(ROOT, 1);
                return 1;
            }
        };
        for &b in &typed[..n] {
            let was_cr = core::mem::replace(&mut last_cr, b == b'\r');
            match b {
                b'\n' if was_cr => {}
                b'\r' | b'\n' => {
                    println!();
                    let command = core::mem::take(&mut line);
                    if !run(command.trim()) {
                        println!("[recovery] powering off");
                        let _ = sys::shutdown(ROOT, 0);
                        return 0;
                    }
                    prompt();
                }
                0x08 | 0x7F => {
                    if line.pop().is_some() {
                        print!("\u{8} \u{8}");
                    }
                }
                b if (0x20..0x7F).contains(&b) && line.len() < LINE_MAX => {
                    line.push(b as char);
                    print!("{}", b as char);
                }
                _ => {}
            }
        }
    }
}
