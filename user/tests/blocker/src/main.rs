//! Blocks forever inside a kernel wait so the parent can kill it mid-block.
//!
//! The kernel must release everything the blocked thread held (kernel stack,
//! address space, queue entries) at the moment of the kill, not when the event it
//! was waiting for finally happens - which, for these modes, is never.
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
