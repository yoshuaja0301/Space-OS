//! The job `spaceshell` supervises.
//!
//! It reads the job name from its bootstrap channel and then reaches exactly one of
//! the end states a real inference worker can reach, so U01 can be tested against
//! every one of them instead of only the tidy case.
#![no_std]
#![no_main]

use libspace::spaceabi::shell::job;
use libspace::{handle, println, sys};

#[unsafe(no_mangle)]
pub extern "C" fn space_main() -> i32 {
    let mut buf = [0u8; 32];
    let (n, _) = match sys::recv(handle::BOOTSTRAP, &mut buf, false) {
        Ok(v) => v,
        Err(e) => {
            println!("[worker] no job on the bootstrap channel: {e}");
            return 2;
        }
    };
    let mode = core::str::from_utf8(&buf[..n]).unwrap_or("");
    println!("[worker] job '{mode}' starting");
    match mode {
        job::OK => {
            // Stand in for a short inference: a little arithmetic and a yield, so the
            // shell really does observe a RUNNING state before the exit.
            let mut acc = 1u64;
            for i in 1..200_000u64 {
                acc = acc.wrapping_mul(6364136223846793005).wrapping_add(i);
            }
            sys::sleep_ms(5);
            println!("[worker] job '{mode}' finished ({acc:#x})");
            0
        }
        job::CRASH => {
            println!("[worker] job '{mode}' is about to fault");
            // SAFETY: deliberately invalid. The kernel must kill this process and
            // nothing else; the shell has to stay alive and say what happened.
            unsafe { core::ptr::read_volatile(core::ptr::null::<u64>()) };
            1
        }
        job::HANG => {
            println!("[worker] job '{mode}' is wedged and never calls the kernel again");
            loop {
                core::hint::spin_loop();
            }
        }
        job::SLOW => {
            println!("[worker] job '{mode}' is sleeping");
            for _ in 0..600 {
                sys::sleep_ms(1000);
            }
            0
        }
        _ => {
            println!("[worker] unknown job '{mode}'");
            4
        }
    }
}
