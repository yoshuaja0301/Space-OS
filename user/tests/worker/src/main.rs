//! Short-lived worker for reclamation cycles: maps memory, allocates on the heap,
//! reports over IPC, exits.
#![no_std]
#![no_main]

extern crate alloc;

use alloc::vec::Vec;

use libspace::{handle, sys};

#[unsafe(no_mangle)]
pub extern "C" fn space_main() -> i32 {
    let region = sys::mem_map(16 * 4096).expect("mem_map");
    // SAFETY: 16 fresh pages.
    unsafe {
        for i in (0..16 * 4096).step_by(4096) {
            *region.add(i) = i as u8;
        }
    }
    let v: Vec<u32> = (0..2048).collect();
    let sum: u32 = v.iter().sum();
    let _ = sys::send(handle::BOOTSTRAP, b"done", None);
    if sum == 2_096_128 { 0 } else { 1 }
}
