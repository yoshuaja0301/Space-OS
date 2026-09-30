//! Waiting on a device that does not interrupt.
//!
//! The storage drivers poll. They start before the timer does, and their commands
//! run under a spinlock with interrupts off, so a wait for a device cannot be
//! counted in timer ticks: it is measured with the CPU's time-stamp counter. How
//! fast that counts is not known this early. Taking it to count at 5 GHz, faster
//! than any x86 TSC runs, makes every wait at least as long as asked -- a few times
//! longer on a slow counter -- so a timeout never fires early; it only has to fire
//! at all, and well before a CPU waiting for the driver's lock would take the wait
//! for a deadlock.

use crate::arch::cpu::timestamp;

/// Counts per millisecond of a counter running at 5 GHz.
const COUNTS_PER_MS: u64 = 5_000_000;

/// Poll `ready` until it holds or `ms` milliseconds have passed. True if it held.
pub fn until(ms: u64, mut ready: impl FnMut() -> bool) -> bool {
    let start = timestamp();
    let span = ms.saturating_mul(COUNTS_PER_MS);
    loop {
        if ready() {
            return true;
        }
        if timestamp().wrapping_sub(start) > span {
            // One last look, so a device that finished just now is not failed.
            return ready();
        }
        core::hint::spin_loop();
    }
}
