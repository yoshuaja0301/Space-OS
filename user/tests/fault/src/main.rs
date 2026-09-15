//! Deliberately misbehaves in the way the parent asks for. The kernel must kill this
//! process (and only this process) with the matching reason.
#![no_std]
#![no_main]

use libspace::{handle, println, sys};

#[unsafe(no_mangle)]
pub extern "C" fn space_main() -> i32 {
    let mut buf = [0u8; 64];
    let (len, _) = sys::recv(handle::BOOTSTRAP, &mut buf, false).expect("mode from parent");
    let mode = core::str::from_utf8(&buf[..len]).unwrap_or("?");
    println!("[fault] pid {} performing '{}'", sys::self_info().map(|i| i.pid).unwrap_or(0), mode);
    match mode {
        "kernel_write" => {
            let p = 0xFFFF_8000_0000_0000u64 as *mut u64;
            // SAFETY: intentionally invalid: writing into the kernel half must fault.
            unsafe { core::ptr::write_volatile(p, 1) };
        }
        "null_read" => {
            let p = core::ptr::null::<u64>();
            // SAFETY: intentionally invalid.
            let v = unsafe { core::ptr::read_volatile(p) };
            println!("[fault] read {v}");
        }
        "nx_exec" => {
            // The stack is mapped NX: executing from it must fault.
            let code = [0xC3u8; 16]; // ret
            let f: extern "C" fn() = unsafe { core::mem::transmute(code.as_ptr()) };
            f();
        }
        "div_zero" => {
            // Rust's `/` inserts a software check that panics; use the instruction
            // directly so the CPU raises #DE.
            // SAFETY: intentionally raises a divide error.
            unsafe {
                core::arch::asm!("xor edx, edx", "mov eax, 1", "xor ecx, ecx", "div ecx",
                    out("eax") _, out("edx") _, out("ecx") _)
            };
        }
        "ud2" => {
            // SAFETY: undefined instruction; the kernel must report INVALID_OPCODE.
            unsafe { core::arch::asm!("ud2") };
        }
        "cli" => {
            // SAFETY: privileged instruction in ring 3 -> #GP.
            unsafe { core::arch::asm!("cli") };
        }
        "kernel_syscall_ptr" => {
            // Not a fault: the kernel must reject the pointer with Error::Fault instead
            // of touching kernel memory. Exit 0 if it does.
            let r = unsafe { sys::raw(spaceabi::syscall::nr::LOG, 0xFFFF_FFFF_8000_0000, 16, 0, 0, 0, 0) };
            return if spaceabi::error::decode(r) == Err(spaceabi::error::Error::Fault) { 0 } else { 2 };
        }
        _ => {}
    }
    println!("[fault] survived '{}' - this is a test failure", mode);
    3
}
