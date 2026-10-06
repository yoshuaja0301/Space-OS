//! One side of a ping-pong through shared memory, with no system call in the loop.
//!
//! The parent writes odd numbers into a shared word and this program answers each
//! with the next even one. Nothing here ever gives up the CPU, so the two sides only
//! take turns this fast when they run at the same time on different CPUs; on one
//! CPU every turn waits for a preemption, a whole quantum (K02, ADR-0024).
#![no_std]
#![no_main]

use core::sync::atomic::{AtomicU32, Ordering};

use libspace::{handle, println, sys};

#[unsafe(no_mangle)]
pub extern "C" fn space_main() -> i32 {
    let mut buf = [0u8; 8];
    let (n, object) = match sys::recv(handle::BOOTSTRAP, &mut buf, false) {
        Ok(v) => v,
        Err(e) => {
            println!("[pingpong] no start message: {e}");
            return 1;
        }
    };
    let (Some(object), 4..) = (object, n) else {
        println!("[pingpong] the start message needs a round count and the shared memory");
        return 1;
    };
    let rounds = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]);
    let ptr = match sys::vmo_map(object, false) {
        Ok(p) => p,
        Err(e) => {
            println!("[pingpong] cannot map the shared memory: {e}");
            return 1;
        }
    };
    sys::handle_close(object).ok();
    // SAFETY: the object is at least a page, mapped read-write and page aligned; the
    // parent only ever touches the same word atomically.
    let word = unsafe { &*(ptr as *const AtomicU32) };
    loop {
        let v = word.load(Ordering::Acquire);
        if v >= rounds.saturating_mul(2) {
            return 0;
        }
        if v % 2 == 1 {
            word.store(v + 1, Ordering::Release);
        }
        core::hint::spin_loop();
    }
}
