//! Choosing between a normal boot and recovery.
//!
//! Recovery must be reachable when the normal system is not (PRD, "rantai boot dan
//! pemulihan"): a desktop that crashes at start, a model that does not load, an
//! index that takes every service down with it. Two ways lead there, and both are
//! decided here, before the kernel runs:
//!
//! * **The operator asks.** For `recovery_wait_ms` the bootloader says it will take
//!   R for recovery, and waits for a key or the timer, whichever comes first.
//! * **The machine keeps failing.** With `boot_count=on`, every boot adds one to
//!   `\SPACEOS\VAR\BOOTS.TXT` on the data volume, through the firmware's own FAT
//!   driver, and the system sets it back to 0 once it is up. A count that has
//!   reached [`FAILED_BOOTS`] means that many boots in a row never got that far:
//!   the bootloader starts recovery instead, and says why.
//!
//! Nothing here can stop a boot: a data volume that cannot be found, read or
//! written, or a console without input, only means that way to recovery is not
//! offered this time, and the log says so.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use uefi::boot::{self, EventType, TimerTrigger, Tpl};
use uefi::proto::console::text::Key;
use uefi::proto::media::file::{File, FileSystemVolumeLabel};
use uefi::proto::media::fs::SimpleFileSystem;
use uefi::{CStr16, cstr16, println};

/// Boots in a row that never came up before recovery starts by itself.
pub const FAILED_BOOTS: u32 = 3;
const COUNT_PATH: &CStr16 = cstr16!("\\SPACEOS\\VAR\\BOOTS.TXT");
const DATA_LABEL: &str = "SPACEDATA";

/// What `spaceos.cfg` says.
pub struct Config {
    pub cmdline: Vec<u8>,
    pub recovery: Vec<u8>,
    pub wait_ms: u64,
    pub boot_count: bool,
}

pub fn parse_config(cfg: &[u8]) -> Config {
    let mut c = Config {
        cmdline: Vec::new(),
        recovery: b"init=bin/spacerecovery".to_vec(),
        wait_ms: 0,
        boot_count: false,
    };
    for line in cfg.split(|&b| b == b'\n') {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        if let Some(v) = line.strip_prefix(b"cmdline=") {
            c.cmdline = v.to_vec();
        } else if let Some(v) = line.strip_prefix(b"recovery=") {
            c.recovery = v.to_vec();
        } else if let Some(v) = line.strip_prefix(b"recovery_wait_ms=") {
            // At most ten seconds: a typo must not hold every boot for minutes.
            c.wait_ms =
                core::str::from_utf8(v).ok().and_then(|s| s.trim().parse().ok()).unwrap_or(0).min(10_000);
        } else if line == b"boot_count=on" {
            c.boot_count = true;
        }
    }
    c
}

/// Offer the R key for `ms` milliseconds. True if it was pressed.
fn operator_asks(ms: u64) -> bool {
    // A console without input cannot be asked; the timer alone would just delay.
    let has_input = uefi::table::system_table_raw().is_some_and(|st| {
        // SAFETY: the firmware's system table, valid while boot services run.
        !unsafe { st.as_ref() }.stdin.is_null()
    });
    if !has_input {
        println!("spaceboot: no console input; the recovery key is not offered");
        return false;
    }
    println!("spaceboot: press R within {} s for recovery", ms.div_ceil(1000));
    // Keys pressed before the question are not answers to it.
    uefi::system::with_stdin(|stdin| while let Ok(Some(_)) = stdin.read_key() {});
    // SAFETY: a plain timer event without a notification function.
    let Ok(timer) = (unsafe { boot::create_event(EventType::TIMER, Tpl::APPLICATION, None, None) }) else {
        return false;
    };
    // 100 ns units.
    if boot::set_timer(&timer, TimerTrigger::Relative(ms * 10_000)).is_err() {
        let _ = boot::close_event(timer);
        return false;
    }
    let mut asked = false;
    while let Some(key_event) = uefi::system::with_stdin(|stdin| stdin.wait_for_key_event()) {
        // SAFETY: both events stay valid for the call; the timer is ours.
        let mut events = [key_event, unsafe { timer.unsafe_clone() }];
        match boot::wait_for_event(&mut events) {
            Ok(0) => match uefi::system::with_stdin(|stdin| stdin.read_key()) {
                Ok(Some(Key::Printable(c))) if matches!(char::from(c), 'r' | 'R') => {
                    asked = true;
                    break;
                }
                // Any other key: keep waiting for the rest of the time.
                _ => continue,
            },
            _ => break, // the timer, or an error: boot normally
        }
    }
    let _ = boot::close_event(timer);
    asked
}

/// The data volume, found by its label among the file systems the firmware knows.
fn data_volume() -> Option<uefi::fs::FileSystem> {
    for handle in boot::find_handles::<SimpleFileSystem>().ok()? {
        let Ok(mut sfs) = boot::open_protocol_exclusive::<SimpleFileSystem>(handle) else { continue };
        let Ok(mut root) = sfs.open_volume() else { continue };
        let Ok(label) = root.get_boxed_info::<FileSystemVolumeLabel>() else { continue };
        let name = String::from(label.volume_label());
        drop(root);
        if name.trim_end() == DATA_LABEL {
            return Some(uefi::fs::FileSystem::new(sfs));
        }
    }
    None
}

/// Add this boot to the count on the data volume; the count before it, or `None`
/// when there is nowhere to keep one.
fn count_boot() -> Option<u32> {
    let Some(mut fs) = data_volume() else {
        println!("spaceboot: no data volume labelled {DATA_LABEL}; boots are not counted");
        return None;
    };
    let before = fs
        .read(COUNT_PATH)
        .ok()
        .and_then(|b| {
            let s = core::str::from_utf8(&b).ok()?;
            s.lines().find_map(|l| l.trim().strip_prefix("tries=")).and_then(|v| v.trim().parse::<u32>().ok())
        })
        .unwrap_or(0);
    println!("spaceboot: {before} boot(s) since the system last came up");
    let now = before.saturating_add(1);
    if fs.write(COUNT_PATH, format!("tries={now}\n").as_bytes()).is_err() {
        println!("spaceboot: cannot write the boot count; boots are not counted");
        return None;
    }
    Some(before)
}

/// The command line to boot with: the normal one, or recovery's with the reason
/// appended (`recovery=operator` or `recovery=failed-boots`).
pub fn choose(cfg: &Config) -> Vec<u8> {
    let asked = cfg.wait_ms > 0 && operator_asks(cfg.wait_ms);
    let failed = if cfg.boot_count { count_boot() } else { None };
    let reason = if asked {
        Some("operator")
    } else if let Some(n) = failed.filter(|&n| n >= FAILED_BOOTS) {
        println!("spaceboot: {n} boots in a row did not come up; starting recovery");
        Some("failed-boots")
    } else {
        None
    };
    match reason {
        None => cfg.cmdline.clone(),
        Some(r) => {
            println!("spaceboot: recovery ({r})");
            let mut line = cfg.recovery.clone();
            line.extend_from_slice(format!(" recovery={r}").as_bytes());
            line
        }
    }
}
