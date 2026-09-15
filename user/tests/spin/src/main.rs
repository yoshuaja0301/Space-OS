//! A runaway process: never yields, never makes a syscall. The scheduler must keep
//! preempting it and the parent must be able to kill it.
#![no_std]
#![no_main]

use libspace::println;

#[unsafe(no_mangle)]
pub extern "C" fn space_main() -> i32 {
    println!("[spin] spinning forever without syscalls");
    let mut x = 0u64;
    loop {
        x = x.wrapping_mul(6364136223846793005).wrapping_add(1);
        core::hint::black_box(x);
    }
}
