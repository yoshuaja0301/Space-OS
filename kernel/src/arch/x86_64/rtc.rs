//! The CMOS real-time clock, read once at boot.
//!
//! Wall-clock time is what certificate validity, audit timestamps and credential
//! expiry are measured against (PRD §4: a cloud adapter needs "waktu sistem"). The
//! RTC is read a single time; afterwards the time is that reading plus the
//! monotonic tick, so it never jumps backwards and never touches the CMOS again.
//! The RTC is expected to hold UTC (the QEMU profile runs `-rtc base=utc`).

use core::sync::atomic::{AtomicU64, Ordering};

use x86_64::instructions::port::Port;

const CMOS_INDEX: u16 = 0x70;
const CMOS_DATA: u16 = 0x71;

const REG_SECONDS: u8 = 0x00;
const REG_MINUTES: u8 = 0x02;
const REG_HOURS: u8 = 0x04;
const REG_DAY: u8 = 0x07;
const REG_MONTH: u8 = 0x08;
const REG_YEAR: u8 = 0x09;
const REG_STATUS_A: u8 = 0x0A;
const REG_STATUS_B: u8 = 0x0B;
/// Century register used by QEMU and most PC firmware (ACPI FADT names it).
const REG_CENTURY: u8 = 0x32;

/// Milliseconds since the Unix epoch at the moment of the boot reading; 0 when the
/// clock could not be read.
static EPOCH_MS_AT_READ: AtomicU64 = AtomicU64::new(0);
/// Uptime (ms) at the moment of the boot reading.
static UPTIME_AT_READ: AtomicU64 = AtomicU64::new(0);

fn cmos(reg: u8) -> u8 {
    // SAFETY: the CMOS index/data port pair. Bit 7 of the index stays clear, which
    // leaves NMIs enabled.
    unsafe {
        Port::<u8>::new(CMOS_INDEX).write(reg & 0x7F);
        Port::<u8>::new(CMOS_DATA).read()
    }
}

fn update_in_progress() -> bool {
    cmos(REG_STATUS_A) & 0x80 != 0
}

fn snapshot() -> Option<[u8; 7]> {
    // An update takes under 2 ms; a flag that never clears means there is no RTC.
    let mut spins = 0u32;
    while update_in_progress() {
        spins += 1;
        if spins > 1_000_000 {
            return None;
        }
        core::hint::spin_loop();
    }
    Some([
        cmos(REG_SECONDS),
        cmos(REG_MINUTES),
        cmos(REG_HOURS),
        cmos(REG_DAY),
        cmos(REG_MONTH),
        cmos(REG_YEAR),
        cmos(REG_CENTURY),
    ])
}

fn bcd(v: u8) -> u8 {
    (v >> 4) * 10 + (v & 0x0F)
}

/// Days from 1970-01-01 to `y-m-d` (proleptic Gregorian), H. Hinnant's algorithm.
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m as i64 + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Seconds since the Unix epoch, or `None` when the clock reads nonsense.
fn read_unix_seconds() -> Option<u64> {
    // Two identical snapshots in a row: the clock did not tick between reads.
    let mut last = snapshot()?;
    let mut stable = false;
    for _ in 0..8 {
        let now = snapshot()?;
        if now == last {
            stable = true;
            break;
        }
        last = now;
    }
    if !stable {
        return None;
    }
    let status_b = cmos(REG_STATUS_B);
    let binary = status_b & 0x04 != 0;
    let h24 = status_b & 0x02 != 0;
    let [mut sec, mut min, raw_hour, mut day, mut month, mut year, mut century] = last;
    let pm = raw_hour & 0x80 != 0;
    let mut hour = raw_hour & 0x7F;
    if !binary {
        sec = bcd(sec);
        min = bcd(min);
        hour = bcd(hour);
        day = bcd(day);
        month = bcd(month);
        year = bcd(year);
        century = bcd(century);
    }
    if !h24 {
        // 12-hour clock: 12 AM is 0, 12 PM is 12.
        hour %= 12;
        if pm {
            hour += 12;
        }
    }
    let full_year = if (19..=21).contains(&century) {
        century as i64 * 100 + year as i64
    } else if year < 70 {
        2000 + year as i64
    } else {
        1900 + year as i64
    };
    if sec > 59 || min > 59 || hour > 23 || !(1..=31).contains(&day) || !(1..=12).contains(&month) {
        return None;
    }
    let days = days_from_civil(full_year, month as u32, day as u32);
    if days < 0 {
        return None;
    }
    Some(days as u64 * 86_400 + hour as u64 * 3600 + min as u64 * 60 + sec as u64)
}

/// Read the RTC and anchor wall-clock time to the current uptime.
pub fn init(uptime_ms: u64) {
    match read_unix_seconds() {
        Some(secs) => {
            EPOCH_MS_AT_READ.store(secs * 1000, Ordering::Relaxed);
            UPTIME_AT_READ.store(uptime_ms, Ordering::Relaxed);
            println!("[kernel] rtc: {secs} s since the Unix epoch (UTC)");
        }
        None => println!("[kernel] rtc: unreadable; wall-clock time unavailable"),
    }
}

/// Milliseconds since the Unix epoch, or `None` when the RTC could not be read.
pub fn realtime_ms(uptime_ms: u64) -> Option<u64> {
    let base = EPOCH_MS_AT_READ.load(Ordering::Relaxed);
    if base == 0 {
        return None;
    }
    Some(base + uptime_ms.saturating_sub(UPTIME_AT_READ.load(Ordering::Relaxed)))
}
