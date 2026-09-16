//! Syscall numbers and argument blocks (ABI version 0).
//!
//! Calling convention (x86-64): `syscall` instruction, number in `rax`, arguments
//! in `rdi, rsi, rdx, r10, r8, r9`, result in `rax` (see [`crate::error::decode`]).
//! `rcx` and `r11` are clobbered by the hardware.
//!
//! Every pointer argument must point into the caller's own address space and be
//! mapped for the required access, otherwise the call fails with `Error::Fault`
//! — the kernel never touches unchecked user memory.

/// Syscall numbers.
pub mod nr {
    /// `exit(code: i32) -> !`
    pub const EXIT: usize = 0;
    /// `log(ptr, len) -> 0` – write UTF-8 text to the kernel console.
    pub const LOG: usize = 1;
    /// `yield() -> 0`
    pub const YIELD: usize = 2;
    /// `sleep(ms) -> 0`
    pub const SLEEP: usize = 3;
    /// `ticks() -> milliseconds since boot`
    pub const TICKS: usize = 4;
    /// `mem_map(len, flags) -> addr` – map zero-filled anonymous pages (quota checked).
    pub const MEM_MAP: usize = 5;
    /// `mem_unmap(addr, len) -> 0`
    pub const MEM_UNMAP: usize = 6;
    /// `channel_create(out: *mut [Handle; 2]) -> 0`
    pub const CHANNEL_CREATE: usize = 7;
    /// `send(handle, ptr, len, transfer_handle) -> 0`
    pub const SEND: usize = 8;
    /// `recv(handle, args: *mut RecvArgs) -> 0`
    pub const RECV: usize = 9;
    /// `handle_close(handle) -> 0`
    pub const HANDLE_CLOSE: usize = 10;
    /// `handle_dup(handle, rights_mask) -> new_handle`
    pub const HANDLE_DUP: usize = 11;
    /// `spawn(root_handle, args: *const SpawnArgs) -> process_handle`
    pub const SPAWN: usize = 12;
    /// `wait(process_handle, out: *mut ExitStatus) -> 0`
    pub const WAIT: usize = 13;
    /// `kill(process_handle) -> 0`
    pub const KILL: usize = 14;
    /// `self_info(out: *mut SelfInfo) -> 0`
    pub const SELF_INFO: usize = 15;
    /// `kstats(root_handle, out: *mut KernelStats) -> 0`
    pub const KSTATS: usize = 16;
    /// `shutdown(root_handle, code) -> !`
    pub const SHUTDOWN: usize = 17;
    /// `debug(root_handle, op) -> 0` – kernel fault injection, see [`debug_op`].
    pub const DEBUG: usize = 18;
    /// `handle_info(handle, out: *mut HandleInfo) -> 0`
    pub const HANDLE_INFO: usize = 19;
    /// `fs_open(root_handle, path_ptr, path_len) -> file_handle`
    pub const FS_OPEN: usize = 20;
    /// `fs_read(file_handle, offset, buf_ptr, len) -> bytes_read`
    pub const FS_READ: usize = 21;
    /// `fs_stat(file_handle, out: *mut FileStat) -> 0`
    pub const FS_STAT: usize = 22;
    /// `vmo_create(len) -> memory_handle` – shareable zeroed memory, charged to the creator.
    pub const VMO_CREATE: usize = 23;
    /// `vmo_map(memory_handle, flags) -> addr`
    pub const VMO_MAP: usize = 24;
    /// `vmo_size(memory_handle) -> len`
    pub const VMO_SIZE: usize = 25;

    pub const COUNT: usize = 26;
}

/// Maximum inline message payload in bytes.
pub const MSG_MAX: usize = 256;

/// Flags for `SYS_RECV`.
pub mod recv_flags {
    /// Return `WouldBlock` instead of blocking when no message is queued.
    pub const NONBLOCK: u32 = 1;
}

/// Flags for `SYS_MEM_MAP` and `SYS_VMO_MAP`.
pub mod map_flags {
    pub const NONE: u32 = 0;
    /// Map a memory object read-only even when the handle carries `WRITE`.
    pub const READ_ONLY: u32 = 1;
}

/// Argument block for `SYS_RECV`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct RecvArgs {
    /// Buffer to receive the payload into.
    pub buf: u64,
    pub buf_cap: u64,
    /// Out: payload length.
    pub len: u64,
    /// Out: received handle or [`crate::handle::INVALID`].
    pub handle: u32,
    pub flags: u32,
}

/// Argument block for `SYS_SPAWN`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct SpawnArgs {
    /// Program name inside the initrd (e.g. `bin/hello`).
    pub name: u64,
    pub name_len: u64,
    /// Memory quota for the new process, in pages (code + stack + heap).
    pub quota_pages: u64,
    /// Handle moved into the child as its bootstrap handle 0, or INVALID.
    pub pass_handle: u32,
    pub _pad: u32,
}

/// How a process ended.
pub mod exit_kind {
    /// Called `exit`; `code` is the exit code.
    pub const EXITED: u32 = 0;
    /// Terminated by the kernel; `reason` is one of [`super::kill_reason`].
    pub const KILLED: u32 = 1;
}

pub mod kill_reason {
    pub const NONE: u32 = 0;
    pub const PAGE_FAULT: u32 = 1;
    pub const GENERAL_PROTECTION: u32 = 2;
    pub const INVALID_OPCODE: u32 = 3;
    pub const DIVIDE_ERROR: u32 = 4;
    pub const OTHER_EXCEPTION: u32 = 5;
    /// `SYS_KILL` from another process.
    pub const SIGNAL: u32 = 6;
    /// `#DB` (single-step / hardware breakpoint) raised in ring 3.
    pub const DEBUG: u32 = 7;
    /// `int3` raised in ring 3.
    pub const BREAKPOINT: u32 = 8;

    pub const fn name(r: u32) -> &'static str {
        match r {
            NONE => "none",
            PAGE_FAULT => "page fault",
            GENERAL_PROTECTION => "general protection fault",
            INVALID_OPCODE => "invalid opcode",
            DIVIDE_ERROR => "divide error",
            OTHER_EXCEPTION => "cpu exception",
            SIGNAL => "killed",
            DEBUG => "debug trap",
            BREAKPOINT => "breakpoint",
            _ => "unknown",
        }
    }
}

/// Exit status reported by `SYS_WAIT`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ExitStatus {
    pub kind: u32,
    pub code: i32,
    pub reason: u32,
    pub _pad: u32,
    /// Faulting address for `PAGE_FAULT`, faulting `rip` otherwise.
    pub fault_addr: u64,
}

impl ExitStatus {
    pub const fn exited(code: i32) -> Self {
        ExitStatus { kind: exit_kind::EXITED, code, reason: 0, _pad: 0, fault_addr: 0 }
    }
    pub const fn killed(reason: u32, fault_addr: u64) -> Self {
        ExitStatus { kind: exit_kind::KILLED, code: -1, reason, _pad: 0, fault_addr }
    }
    pub const fn is_exited_with(&self, code: i32) -> bool {
        self.kind == exit_kind::EXITED && self.code == code
    }
    pub const fn is_killed_by(&self, reason: u32) -> bool {
        self.kind == exit_kind::KILLED && self.reason == reason
    }
}

/// Result of `SYS_SELF_INFO`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct SelfInfo {
    pub pid: u64,
    pub quota_pages: u64,
    pub used_pages: u64,
    pub abi_version: u32,
    pub _pad: u32,
}

/// Result of `SYS_KSTATS`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct KernelStats {
    pub frames_total: u64,
    pub frames_free: u64,
    pub heap_total: u64,
    pub heap_used: u64,
    pub processes_live: u64,
    pub threads_live: u64,
    pub uptime_ms: u64,
    pub context_switches: u64,
    /// Sectors of the mounted volume, 0 when the machine has no usable disk.
    /// User space uses it to tell "no storage on this machine" from "read failed".
    pub volume_sectors: u64,
}

/// Result of `SYS_FS_STAT`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FileStat {
    pub size: u64,
    /// Sector size of the backing block device (0 when not block backed).
    pub block_size: u64,
}

/// Longest path `SYS_FS_OPEN` accepts.
pub const PATH_MAX: usize = 255;

/// Result of `SYS_HANDLE_INFO`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct HandleInfo {
    pub kind: u32,
    pub rights: u32,
}

/// Operations for `SYS_DEBUG` (require the root DEBUG right).
pub mod debug_op {
    /// Deliberate `panic!` in the kernel – exercises the crash log path.
    pub const PANIC: u64 = 1;
    /// Deliberate kernel-mode page fault – exercises the exception dump path.
    pub const KERNEL_FAULT: u64 = 2;
}

/// Exit codes written to the QEMU `isa-debug-exit` device by `SYS_SHUTDOWN` and the
/// panic handler. QEMU's process exit status is `(value << 1) | 1`.
pub mod qemu_exit {
    pub const SUCCESS: u32 = 0x10;
    pub const FAILURE: u32 = 0x11;
    pub const PANIC: u32 = 0x3f;
}
