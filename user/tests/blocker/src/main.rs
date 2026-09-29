//! Blocks forever inside a kernel wait so the parent can kill it mid-block.
//!
//! The kernel must release everything the blocked thread held (kernel stack,
//! address space, queue entries) at the moment of the kill, not when the event it
//! was waiting for finally happens - which, for these modes, is never.
//!
//! One mode turns the roles around: `net_send_later` makes the *parent* the one
//! that is blocked, by transmitting a frame only once the parent has gone to sleep
//! waiting for the answer.
#![no_std]
#![no_main]

use libspace::{handle, println, sys};

#[unsafe(no_mangle)]
pub extern "C" fn space_main() -> i32 {
    let mut buf = [0u8; 64];
    let (len, passed) = sys::recv(handle::BOOTSTRAP, &mut buf, false).expect("mode from parent");
    let mode = core::str::from_utf8(&buf[..len]).unwrap_or("?");
    sys::send(handle::BOOTSTRAP, b"ready", None).expect("ready");
    match mode {
        // Sleep far longer than the test runs: a kill must not wait for the timer.
        "sleep" => sys::sleep_ms(3_600_000),
        // Wait on a process that never exits.
        "wait" => {
            let h = passed.expect("process handle");
            let st = sys::wait(h).expect("wait");
            println!("[blocker] unexpected: target exited with {st:?}");
            return 2;
        }
        // Block in wait_any on two queues at once, with no timeout: the silent
        // bootstrap channel and a process that never exits.
        "wait_any" => {
            let h = passed.expect("process handle");
            let r = sys::wait_any(&[handle::BOOTSTRAP, h], libspace::spaceabi::syscall::WAIT_FOREVER);
            println!("[blocker] unexpected: wait_any returned {r:?}");
            return 5;
        }
        // Block in wait_any with a timeout far beyond the test: a kill must release
        // the timer entry at once, not when the hour is up.
        "wait_any_timeout" => {
            let r = sys::wait_any(&[handle::BOOTSTRAP], 3_600_000);
            println!("[blocker] unexpected: wait_any returned {r:?}");
            return 6;
        }
        // The parent is about to sleep in wait_any on its network lease. Transmit
        // the frame it handed over 150 ms from now, so the answer arrives while the
        // parent is blocked, then report when the frame left.
        "net_send_later" => {
            let nic = passed.expect("network lease");
            let mut frame = [0u8; 64];
            let (n, _) = sys::recv(handle::BOOTSTRAP, &mut frame, false).expect("frame from parent");
            sys::sleep_ms(150);
            let sent_at = sys::ticks_ms();
            if let Err(e) = sys::net_send(nic, &frame[..n]) {
                println!("[blocker] net_send: {e}");
                return 7;
            }
            sys::send(handle::BOOTSTRAP, &sent_at.to_le_bytes(), None).expect("report");
            return 0;
        }
        // Block in recv on a channel whose peer stays open and silent.
        "recv" => {
            let (n, _) = sys::recv(handle::BOOTSTRAP, &mut buf, false).expect("second message");
            println!("[blocker] unexpected: received {n} bytes");
            return 3;
        }
        _ => return 1,
    }
    println!("[blocker] unexpected: '{mode}' returned");
    4
}
