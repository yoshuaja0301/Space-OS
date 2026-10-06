//! Wall-clock time: the firmware's clock as the bootloader read it (UEFI `GetTime`),
//! carried forward with the uptime. AArch64 machines have no one RTC device the
//! kernel could rely on; the firmware always has a clock.

use core::sync::atomic::{AtomicU64, Ordering};

static BOOT_TIME: AtomicU64 = AtomicU64::new(0);
static EPOCH_MS_AT_READ: AtomicU64 = AtomicU64::new(0);
static UPTIME_AT_READ: AtomicU64 = AtomicU64::new(0);

pub fn set_boot_time(secs: u64) {
    BOOT_TIME.store(secs, Ordering::Relaxed);
}

/// Anchor wall-clock time to the current uptime.
pub fn init(uptime_ms: u64) {
    let secs = BOOT_TIME.load(Ordering::Relaxed);
    if secs == 0 {
        println!("[kernel] rtc: the firmware gave no time; wall-clock time unavailable");
        return;
    }
    EPOCH_MS_AT_READ.store(secs * 1000, Ordering::Relaxed);
    UPTIME_AT_READ.store(uptime_ms, Ordering::Relaxed);
    println!("[kernel] rtc: {secs} s since the Unix epoch (UTC)");
}

/// Milliseconds since the Unix epoch, or `None` without a clock.
pub fn realtime_ms(uptime_ms: u64) -> Option<u64> {
    let base = EPOCH_MS_AT_READ.load(Ordering::Relaxed);
    if base == 0 {
        return None;
    }
    Some(base + uptime_ms.saturating_sub(UPTIME_AT_READ.load(Ordering::Relaxed)))
}
