//! Negative tests for the syscall ABI: every misuse must be rejected with the right
//! error code and never harm the kernel. Exit code 0 = all checks passed.
#![no_std]
#![no_main]

use libspace::spaceabi::error::{Error, decode};
use libspace::spaceabi::handle::rights;
use libspace::spaceabi::syscall::{RecvArgs, nr};
use libspace::{Handle, handle, println, sys};

static mut FAILS: u32 = 0;

fn check(name: &str, got: Result<impl core::fmt::Debug, Error>, want: Error) {
    match got {
        Err(e) if e == want => println!("[abi]   ok   {name}: {want:?}"),
        other => {
            println!("[abi]   FAIL {name}: expected {want:?}, got {other:?}");
            // SAFETY: single-threaded.
            unsafe { FAILS += 1 };
        }
    }
}

fn check_ok<T: core::fmt::Debug>(name: &str, got: Result<T, Error>) -> Option<T> {
    match got {
        Ok(v) => {
            println!("[abi]   ok   {name}");
            Some(v)
        }
        Err(e) => {
            println!("[abi]   FAIL {name}: unexpected error {e:?}");
            // SAFETY: single-threaded.
            unsafe { FAILS += 1 };
            None
        }
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn space_main() -> i32 {
    println!("[abi] syscall negative tests");

    // Unknown syscall number.
    let r = decode(unsafe { sys::raw(999, 0, 0, 0, 0, 0, 0) });
    check("unknown syscall", r, Error::NoSys);

    // Bad handle index.
    check("send on bad handle", sys::send(12345, b"x", None), Error::BadHandle);
    check("wait on bad handle", sys::wait(4242), Error::BadHandle);

    // Wrong object kind: handle 0 is a channel here, not the root capability.
    check("kstats via channel handle", sys::kstats(handle::BOOTSTRAP), Error::Denied);
    check("spawn via channel handle", sys::spawn(handle::BOOTSTRAP, "bin/hello", 64, None), Error::Denied);
    check("wait on channel handle", sys::wait(handle::BOOTSTRAP), Error::Denied);

    // Bad pointers: kernel half, unmapped user page, wrap-around.
    let r = decode(unsafe { sys::raw(nr::LOG, 0xFFFF_8000_0000_0000, 8, 0, 0, 0, 0) });
    check("log from kernel address", r, Error::Fault);
    let r = decode(unsafe { sys::raw(nr::LOG, 0x10, 8, 0, 0, 0, 0) });
    check("log from unmapped user page", r, Error::Fault);
    let r = decode(unsafe { sys::raw(nr::LOG, 0x7FFF_FFFF_FFF0, 0x20, 0, 0, 0, 0) });
    check("log crossing end of user space", r, Error::Fault);
    let r = decode(unsafe { sys::raw(nr::RECV, handle::BOOTSTRAP as u64, 0xDEAD_0000, 0, 0, 0, 0) });
    check("recv with bad args pointer", r, Error::Fault);
    let bad_args = RecvArgs { buf: 0xFFFF_9000_0000_0000, buf_cap: 16, len: 0, handle: 0, flags: 0 };
    let r = decode(unsafe {
        sys::raw(nr::RECV, handle::BOOTSTRAP as u64, &bad_args as *const RecvArgs as u64, 0, 0, 0, 0)
    });
    check("recv into kernel buffer", r, Error::Fault);

    // Invalid arguments.
    check("unmap of unmapped range", sys::mem_unmap(0x5000_0000 as *mut u8, 4096), Error::Invalid);
    check("map of zero bytes", sys::mem_map(0), Error::Invalid);
    let r = decode(unsafe { sys::raw(nr::MEM_MAP, 4096, 0xFF, 0, 0, 0, 0) });
    check("map with unknown flags", r, Error::Invalid);
    let big = [0u8; 300];
    let (a, b) = sys::channel_create().expect("channel");
    check("oversized message", sys::send(a, &big, None), Error::MsgSize);
    check("transfer the sending handle itself", sys::send(a, b"x", Some(a)), Error::Invalid);
    check("transfer the peer endpoint through its own channel", sys::send(a, b"x", Some(b)), Error::Invalid);
    let a2 = sys::handle_dup(a, rights::CHANNEL_ALL).expect("dup a");
    check("transfer a duplicate of the sending endpoint", sys::send(a, b"x", Some(a2)), Error::Invalid);
    if sys::handle_info(a2).is_err() {
        println!("[abi]   FAIL rejected transfer must leave the handle in place");
        unsafe { FAILS += 1 };
    } else {
        println!("[abi]   ok   rejected transfer leaves the handle in place");
    }
    sys::handle_close(a2).expect("close a2");

    // Capability attenuation: a RECV-only duplicate cannot send and cannot be widened.
    let ro = check_ok("dup with RECV only", sys::handle_dup(a, rights::RECV)).unwrap_or(handle::INVALID);
    check("send on RECV-only dup", sys::send(ro, b"x", None), Error::Denied);
    check("dup of a handle without DUP right", sys::handle_dup(ro, rights::CHANNEL_ALL), Error::Denied);
    let info = sys::handle_info(ro).expect("handle_info");
    if info.rights != rights::RECV {
        println!("[abi]   FAIL rights of dup: {:#x}", info.rights);
        unsafe { FAILS += 1 };
    } else {
        println!("[abi]   ok   dup carries exactly RECV");
    }
    check("transfer a handle without TRANSFER right", sys::send(b, b"h", Some(ro)), Error::Denied);
    let mut buf = [0u8; 8];
    check("nonblocking recv on empty channel", sys::recv(ro, &mut buf, true), Error::WouldBlock);

    // Message that does not fit stays queued.
    sys::send(b, b"hello world", None).expect("send");
    let mut small = [0u8; 4];
    check("recv into too-small buffer", sys::recv(a, &mut small, true), Error::MsgSize);
    let mut ok = [0u8; 32];
    let (n, _) = sys::recv(a, &mut ok, true).expect("recv after MsgSize");
    if &ok[..n] != b"hello world" {
        println!("[abi]   FAIL message was lost");
        unsafe { FAILS += 1 };
    } else {
        println!("[abi]   ok   message survived the MsgSize rejection");
    }

    // Peer closed.
    sys::handle_close(b).expect("close b");
    check("recv after peer closed", sys::recv(a, &mut buf, false), Error::PeerClosed);
    check("send after peer closed", sys::send(a, b"x", None), Error::PeerClosed);
    sys::handle_close(a).expect("close a");
    sys::handle_close(ro).expect("close ro");
    check("handle_info on closed handle", sys::handle_info(a), Error::BadHandle);
    check("double close", sys::handle_close(a), Error::BadHandle);

    // Root-only calls with a random handle number.
    let none: Handle = 77;
    check("shutdown without root", sys::shutdown(none, 0), Error::BadHandle);
    check("debug without root", sys::debug(none, 1), Error::BadHandle);

    // SAFETY: single-threaded.
    let fails = unsafe { FAILS };
    println!("[abi] done: {} failure(s)", fails);
    if fails == 0 { 0 } else { 1 }
}
