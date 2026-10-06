//! Kernel console: every line goes to the serial port (COM1) and, when present, to the
//! framebuffer text console. Serial is the authoritative log for the test harness.

use core::fmt::{self, Write};

use crate::sync::SpinLock;

struct Console;

impl Write for Console {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        crate::arch::serial::write_str(s);
        crate::fb::write_str(s);
        Ok(())
    }
}

static CONSOLE: SpinLock<Console> = SpinLock::new(Console);

#[doc(hidden)]
pub fn _print(args: fmt::Arguments) {
    let mut c = CONSOLE.lock();
    let _ = c.write_fmt(args);
}

/// Print without taking the console lock (panic path only).
///
/// # Safety
/// Only from the panic handler, which has disabled interrupts and will never return
/// to whoever held the lock.
pub unsafe fn emergency_print(args: fmt::Arguments) {
    // SAFETY: see above.
    unsafe { CONSOLE.force_unlock() };
    let _ = Console.write_fmt(args);
}

#[macro_export]
macro_rules! print {
    ($($arg:tt)*) => { $crate::console::_print(format_args!($($arg)*)) };
}

#[macro_export]
macro_rules! println {
    () => { $crate::print!("\n") };
    ($($arg:tt)*) => { $crate::print!("{}\n", format_args!($($arg)*)) };
}
