//! `libspace` – the Space OS user-space runtime (ABI v0).
//!
//! Provides the process entry point (`_start` → `space_main`), typed syscall
//! wrappers ([`sys`]), a small heap ([`heap`]) and `print!`/`println!` ([`io`]).
//! Programs implement `#[unsafe(no_mangle)] pub extern "C" fn space_main() -> i32`.

#![no_std]
#![deny(unsafe_op_in_unsafe_fn)]

extern crate alloc;

pub mod heap;
pub mod io;
pub mod sha256;
pub mod sys;

pub use spaceabi;
pub use spaceabi::error::Error;
pub use spaceabi::handle::{self, Handle};
pub use spaceabi::syscall::{ExitStatus, FileStat, KernelStats, SelfInfo, exit_kind, kill_reason};

use core::arch::naked_asm;

unsafe extern "C" {
    fn space_main() -> i32;
}

/// Process entry point: the kernel starts every program here with a clean register
/// file and `rsp` at the top of the initial stack.
#[unsafe(naked)]
#[unsafe(no_mangle)]
pub extern "C" fn _start() -> ! {
    naked_asm!(
        "and rsp, -16",
        "xor rbp, rbp",
        "call {start}",
        "ud2",
        start = sym rust_start,
    )
}

extern "C" fn rust_start() -> ! {
    // SAFETY: `space_main` is provided by the program crate.
    let code = unsafe { space_main() };
    sys::exit(code)
}

#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    println!("[user] panic: {}", info);
    sys::exit(101)
}
