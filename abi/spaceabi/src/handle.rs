//! Capability handles.
//!
//! A handle is a per-process index into the kernel handle table. Every entry
//! pairs a kernel object with a set of *rights*; rights can only be reduced
//! (see `SYS_HANDLE_DUP`), never widened, and the kernel checks them on every use.

pub type Handle = u32;

/// Never a valid handle; used for "no handle" arguments.
pub const INVALID: Handle = u32::MAX;

/// Handle 0 is the bootstrap handle installed by the spawner
/// (for `init` this is the root capability, for children the channel passed by the parent).
pub const BOOTSTRAP: Handle = 0;

/// Maximum number of handles per process.
pub const MAX_HANDLES: usize = 256;

/// Rights bits carried by a handle.
pub mod rights {
    /// Send messages on a channel endpoint.
    pub const SEND: u32 = 1 << 0;
    /// Receive messages on a channel endpoint.
    pub const RECV: u32 = 1 << 1;
    /// Move the handle to another process (via message or spawn).
    pub const TRANSFER: u32 = 1 << 2;
    /// Duplicate the handle (with equal or fewer rights).
    pub const DUP: u32 = 1 << 3;
    /// Wait for a process to exit.
    pub const WAIT: u32 = 1 << 4;
    /// Kill a process.
    pub const KILL: u32 = 1 << 5;
    /// Root capability: spawn programs from the initrd.
    pub const SPAWN: u32 = 1 << 6;
    /// Root capability: read kernel statistics.
    pub const STATS: u32 = 1 << 7;
    /// Root capability: shut the machine down.
    pub const SHUTDOWN: u32 = 1 << 8;
    /// Root capability: kernel debug hooks (fault injection).
    pub const DEBUG: u32 = 1 << 9;
    /// Root capability: open files on mounted filesystems.
    pub const FS: u32 = 1 << 10;
    /// Read from a file handle, or from a mapped memory object.
    pub const READ: u32 = 1 << 11;
    /// Map a memory object into an address space.
    pub const MAP: u32 = 1 << 12;
    /// Map a memory object writable.
    pub const WRITE: u32 = 1 << 13;
    /// Root capability: read what a person types on the console.
    ///
    /// Separate from every other root right on purpose: a session service needs the
    /// keystrokes and nothing else, and nothing that does not own the session should
    /// be able to read them.
    pub const CONSOLE: u32 = 1 << 14;

    pub const CHANNEL_ALL: u32 = SEND | RECV | TRANSFER | DUP;
    pub const PROCESS_ALL: u32 = WAIT | KILL | TRANSFER | DUP;
    pub const FILE_ALL: u32 = READ | TRANSFER | DUP;
    pub const MEMORY_ALL: u32 = READ | WRITE | MAP | TRANSFER | DUP;
    pub const ROOT_ALL: u32 = SPAWN | STATS | SHUTDOWN | DEBUG | FS | CONSOLE | TRANSFER | DUP;
}

/// Kind of kernel object behind a handle (returned by `SYS_HANDLE_INFO`).
pub mod kind {
    pub const CHANNEL: u32 = 1;
    pub const PROCESS: u32 = 2;
    pub const ROOT: u32 = 3;
    pub const FILE: u32 = 4;
    pub const MEMORY: u32 = 5;
}
