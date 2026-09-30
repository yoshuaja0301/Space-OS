//! `print!`/`println!` over `SYS_LOG`. Output is buffered per call so that one
//! `println!` reaches the kernel console as one atomic write.

use core::fmt::{self, Write};

const BUF: usize = 1024;

struct LineBuf {
    buf: [u8; BUF],
    len: usize,
}

impl LineBuf {
    fn flush(&mut self) {
        if self.len > 0 {
            if let Ok(s) = core::str::from_utf8(&self.buf[..self.len]) {
                crate::sys::log(s);
            }
            self.len = 0;
        }
    }
}

impl Write for LineBuf {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        let mut rest = s.as_bytes();
        while !rest.is_empty() {
            let space = BUF - self.len;
            if space == 0 {
                self.flush();
                continue;
            }
            let n = space.min(rest.len());
            // Keep UTF-8 boundaries intact when splitting.
            let mut cut = n;
            while cut < rest.len() && cut > 0 && (rest[cut] & 0xC0) == 0x80 {
                cut -= 1;
            }
            if cut == 0 {
                self.flush();
                continue;
            }
            self.buf[self.len..self.len + cut].copy_from_slice(&rest[..cut]);
            self.len += cut;
            rest = &rest[cut..];
        }
        Ok(())
    }
}

#[doc(hidden)]
pub fn _print(args: fmt::Arguments) {
    let mut b = LineBuf { buf: [0; BUF], len: 0 };
    let _ = b.write_fmt(args);
    b.flush();
}

#[macro_export]
macro_rules! print {
    ($($arg:tt)*) => { $crate::io::_print(format_args!($($arg)*)) };
}

#[macro_export]
macro_rules! println {
    () => { $crate::print!("\n") };
    ($($arg:tt)*) => { $crate::print!("{}\n", format_args!($($arg)*)) };
}
