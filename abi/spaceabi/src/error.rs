//! Syscall error codes.
//!
//! A syscall returns a `isize`: values `>= 0` are results, values in
//! `[-4095, -1]` are negated error codes from this module.

macro_rules! errors {
    ($($(#[$doc:meta])* $name:ident = $val:expr, $msg:expr;)*) => {
        /// Error returned by a syscall.
        #[repr(u32)]
        #[derive(Clone, Copy, Debug, PartialEq, Eq)]
        pub enum Error {
            $($(#[$doc])* $name = $val,)*
        }

        impl Error {
            pub const fn from_code(code: u32) -> Option<Error> {
                match code {
                    $($val => Some(Error::$name),)*
                    _ => None,
                }
            }

            pub const fn message(self) -> &'static str {
                match self {
                    $(Error::$name => $msg,)*
                }
            }
        }
    };
}

errors! {
    /// Invalid argument (bad flags, length, alignment, address range).
    Invalid = 1, "invalid argument";
    /// Handle index does not name an open handle.
    BadHandle = 2, "bad handle";
    /// Handle exists but lacks the required right, or names an object of the wrong kind.
    Denied = 3, "permission denied";
    /// Kernel is out of memory.
    NoMemory = 4, "out of memory";
    /// The process memory quota would be exceeded.
    Quota = 5, "quota exceeded";
    /// Operation would block (queue full / no message and NONBLOCK set).
    WouldBlock = 6, "would block";
    /// Peer endpoint of the channel is closed.
    PeerClosed = 7, "peer closed";
    /// Named program not found in the initrd.
    NotFound = 8, "not found";
    /// A user pointer was not readable/writable at the required size.
    Fault = 9, "bad address";
    /// Unknown syscall number.
    NoSys = 10, "no such syscall";
    /// Executable image is malformed.
    NoExec = 11, "invalid executable";
    /// Message larger than the buffer or the maximum message size.
    MsgSize = 12, "message too large";
    /// Handle table is full.
    TooManyHandles = 13, "too many handles";
    /// Target process has already exited.
    Exited = 14, "process exited";
    /// The blocking call was interrupted because the caller is being terminated.
    /// User code never observes it: the process exits when the syscall returns.
    Interrupted = 15, "interrupted";
}

impl core::fmt::Display for Error {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(self.message())
    }
}

/// Highest negated error value; anything below `-MAX_ERRNO` is a normal result.
pub const MAX_ERRNO: isize = 4095;

/// Encode a syscall result into the `rax` return convention.
pub const fn encode(r: Result<usize, Error>) -> isize {
    match r {
        Ok(v) => v as isize,
        Err(e) => -(e as u32 as isize),
    }
}

/// Decode the `rax` return convention.
pub const fn decode(raw: isize) -> Result<usize, Error> {
    if raw < 0 && raw >= -MAX_ERRNO {
        match Error::from_code((-raw) as u32) {
            Some(e) => Err(e),
            None => Err(Error::Invalid),
        }
    } else {
        Ok(raw as usize)
    }
}
