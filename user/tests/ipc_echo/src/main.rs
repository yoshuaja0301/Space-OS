//! Echo service: uppercases every message from the parent. A message carrying a
//! handle is answered on that handle instead (exercises handle transfer).
#![no_std]
#![no_main]

use libspace::spaceabi::error::Error;
use libspace::{handle, println, sys};

#[unsafe(no_mangle)]
pub extern "C" fn space_main() -> i32 {
    let mut buf = [0u8; 256];
    let mut served = 0u32;
    loop {
        match sys::recv(handle::BOOTSTRAP, &mut buf, false) {
            Ok((len, Some(h))) => {
                let text = core::str::from_utf8(&buf[..len]).unwrap_or("");
                println!("[echo] received handle {h} with '{text}', replying on it");
                sys::send(h, b"via transferred handle", None).expect("send on transferred handle");
                sys::handle_close(h).expect("close transferred handle");
            }
            Ok((len, None)) => {
                for b in &mut buf[..len] {
                    b.make_ascii_uppercase();
                }
                sys::send(handle::BOOTSTRAP, &buf[..len], None).expect("reply");
                served += 1;
            }
            Err(Error::PeerClosed) => {
                println!("[echo] parent closed the channel after {served} messages; exiting");
                return 0;
            }
            Err(e) => {
                println!("[echo] unexpected error {e:?}");
                return 1;
            }
        }
    }
}
