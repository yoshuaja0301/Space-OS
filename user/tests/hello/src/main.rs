#![no_std]
#![no_main]

use libspace::{println, sys};

#[unsafe(no_mangle)]
pub extern "C" fn space_main() -> i32 {
    let me = sys::self_info().expect("self_info");
    println!(
        "[hello] hello from user space, pid {} ({} of {} pages used)",
        me.pid, me.used_pages, me.quota_pages
    );
    0
}
