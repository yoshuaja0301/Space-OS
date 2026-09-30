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
    /// `wait(process_handle, out: *mut ExitStatus, flags) -> 0`, see [`wait_flags`].
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
    /// `fs_list(root_handle, path_ptr, path_len, out: *mut DirEntry, cap) -> entries`
    pub const FS_LIST: usize = 26;
    /// `console_read(root_handle, buf_ptr, len) -> bytes` – what has been typed.
    /// Never blocks: 0 means nothing is waiting.
    pub const CONSOLE_READ: usize = 27;
    /// `fs_create(root_handle, path_ptr, path_len) -> file_handle` – create the file
    /// empty, or empty it if it is already there. Needs root `FS | FS_WRITE`.
    pub const FS_CREATE: usize = 28;
    /// `fs_write(file_handle, offset, buf_ptr, len) -> bytes_written`
    pub const FS_WRITE: usize = 29;
    /// `net_open(root_handle) -> nic_handle` – take the network device. Needs the
    /// root `NET` right; only one lease exists at a time (`Busy` otherwise).
    pub const NET_OPEN: usize = 30;
    /// `net_info(nic_handle, out: *mut NetInfo) -> 0`
    pub const NET_INFO: usize = 31;
    /// `net_send(nic_handle, frame_ptr, len) -> len` – transmit one Ethernet frame.
    /// Never blocks: a full transmit ring yields `WouldBlock`.
    pub const NET_SEND: usize = 32;
    /// `net_recv(nic_handle, buf_ptr, cap) -> len` – take one received frame.
    /// Never blocks: `WouldBlock` when nothing has arrived; wait with `WAIT_ANY`.
    pub const NET_RECV: usize = 33;
    /// `wait_any(handles_ptr, count, timeout_ms) -> index` – block until one of the
    /// handles is ready (see [`super::WAIT_MAX`], [`super::WAIT_FOREVER`]); `TimedOut`
    /// when the timeout passes first.
    pub const WAIT_ANY: usize = 34;
    /// `clock_realtime() -> milliseconds since 1970-01-01T00:00:00Z`, from the
    /// real-time clock read at boot plus the monotonic tick since.
    pub const CLOCK_REALTIME: usize = 35;
    /// `random(buf, len) -> len`: `len` (at most [`super::RANDOM_MAX`]) bytes from the
    /// kernel's entropy source (virtio-rng, else RDRAND). `NotFound` when the machine
    /// has neither: there is no weaker fallback. Needs no right.
    pub const RANDOM: usize = 36;
    /// `cmdline(root_handle, buf_ptr, len) -> total_len`: the kernel command line
    /// (`cmdline=` in `spaceos.cfg`), as much of it as fits in `len` bytes; the
    /// result is its whole length, so a short buffer can be retried with the right
    /// size. Needs the root `STATS` right: it is how the kernel was configured,
    /// which a program told nothing else has no business reading.
    pub const CMDLINE: usize = 37;
    /// `display_open(root_handle) -> display_handle`: take the screen. Needs the root
    /// `DISPLAY` right; one lease at a time (`Busy`), `NotFound` without a usable
    /// framebuffer. The handle is a memory object: `SYS_VMO_MAP` maps the pixels.
    /// While it or a mapping of it lives the kernel console stops drawing, and it
    /// takes the screen back when the last of them is gone.
    pub const DISPLAY_OPEN: usize = 38;
    /// `display_info(display_handle, out: *mut DisplayInfo) -> 0`
    pub const DISPLAY_INFO: usize = 39;
    /// `input_read(root_handle, out: *mut InputEvent, count) -> events`: key presses
    /// and releases with the modifiers held, oldest first (see [`crate::input`]).
    /// Needs the root `CONSOLE` right; at most [`super::INPUT_READ_MAX`] per call.
    /// Never blocks: 0 means nothing happened.
    pub const INPUT_READ: usize = 40;

    pub const COUNT: usize = 41;
}

/// Most bytes one `SYS_RANDOM` call returns.
pub const RANDOM_MAX: usize = 256;

/// Most events one `SYS_INPUT_READ` call returns.
pub const INPUT_READ_MAX: usize = 64;

/// What `SYS_DISPLAY_INFO` reports about the leased screen.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DisplayInfo {
    pub width: u32,
    pub height: u32,
    /// Pixels per row in the mapping; may exceed `width`.
    pub stride: u32,
    /// [`crate::boot::fb_format::RGBX`] or [`crate::boot::fb_format::BGRX`], four
    /// bytes per pixel.
    pub format: u32,
    /// Byte offset of pixel (0, 0) in the mapping.
    pub offset: u64,
    /// Bytes in the mapping.
    pub size: u64,
}

/// Largest Ethernet frame `SYS_NET_SEND` accepts and `SYS_NET_RECV` returns: 14 bytes
/// of header and 1500 of payload. No VLAN tags, no jumbo frames, no FCS.
pub const FRAME_MAX: usize = 1514;
/// Smallest frame `SYS_NET_SEND` accepts: an Ethernet header and nothing else.
pub const FRAME_MIN: usize = 14;

/// Most handles one `SYS_WAIT_ANY` call may watch.
pub const WAIT_MAX: usize = 32;
/// `timeout_ms` for `SYS_WAIT_ANY` that never expires.
pub const WAIT_FOREVER: u64 = u64::MAX;

/// What `SYS_NET_INFO` reports about the network device.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct NetInfo {
    /// Station address the device answers to.
    pub mac: [u8; 6],
    /// 1 when the link is up. A device that cannot report link state reports up.
    pub link_up: u8,
    pub _pad: u8,
    /// Largest Ethernet payload (the IP MTU).
    pub mtu: u32,
    /// Largest whole frame, see [`FRAME_MAX`].
    pub frame_max: u32,
    /// Frames handed to user space since boot.
    pub rx_frames: u64,
    /// Frames accepted for transmission since boot.
    pub tx_frames: u64,
    /// Frames the kernel discarded: malformed by the device, or left behind by a
    /// previous lease holder.
    pub rx_dropped: u64,
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
    /// An x87 instruction (`#NM`): user space has no floating-point unit, since the
    /// kernel keeps no FPU state per thread.
    pub const NO_FPU: u32 = 9;

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
            NO_FPU => "x87 instruction without an FPU",
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
    /// The fewest free frames there have been since boot: how close the machine
    /// came to running out, which `frames_free` alone forgets as soon as the
    /// memory is back.
    pub frames_free_min: u64,
    /// The most kernel-heap bytes in use at once since boot.
    pub heap_used_peak: u64,
    /// CPUs the kernel schedules threads on: 1 when the machine has one, or when
    /// the others could not be started (the boot log says why).
    pub cpus_online: u64,
}

/// Flags for `SYS_WAIT`.
pub mod wait_flags {
    pub const NONE: u32 = 0;
    /// Return `WouldBlock` instead of blocking while the process is still alive.
    /// A single-threaded supervisor needs this: it must stay able to answer its
    /// control channel while a child runs.
    pub const NONBLOCK: u32 = 1;
}

/// One entry of a directory, as returned by `SYS_FS_LIST`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DirEntry {
    /// 8.3 name, upper case, NUL padded.
    pub name: [u8; 12],
    /// 1 when the entry is a directory.
    pub is_dir: u8,
    pub _pad: [u8; 3],
    pub size: u64,
}

impl DirEntry {
    pub fn name(&self) -> &str {
        let end = self.name.iter().position(|b| *b == 0).unwrap_or(self.name.len());
        core::str::from_utf8(&self.name[..end]).unwrap_or("?")
    }
}

/// Most entries `SYS_FS_LIST` returns in one call.
pub const DIR_ENTRIES_MAX: usize = 64;

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
    /// Overflow the console input ring on purpose – exercises the input-loss path,
    /// which cannot be provoked from user space any other way (the ring is fed by
    /// interrupts). Pushes [`CONSOLE_FLOOD_LEN`] bytes of a known pattern.
    pub const CONSOLE_FLOOD: u64 = 3;
    /// Make the PS/2 controller deliver one key press ([`PS2_INJECT_CHAR`]), so the
    /// keyboard interrupt path can be tested on a machine nobody is typing at.
    pub const PS2_INJECT: u64 = 4;
}

/// The character [`debug_op::PS2_INJECT`] makes the keyboard produce.
pub const PS2_INJECT_CHAR: u8 = b'a';

/// Bytes pushed by [`debug_op::CONSOLE_FLOOD`], and the pattern they follow:
/// byte `i` is `b'A' + (i % 26)`. Larger than the kernel ring, so the oldest bytes
/// are dropped and the loss is reported to the next reader.
pub const CONSOLE_FLOOD_LEN: usize = 264;

/// The byte `debug_op::CONSOLE_FLOOD` pushes at index `i`.
pub const fn console_flood_byte(i: usize) -> u8 {
    b'A' + (i % 26) as u8
}

/// Exit codes written to the QEMU `isa-debug-exit` device by `SYS_SHUTDOWN` and the
/// panic handler. QEMU's process exit status is `(value << 1) | 1`.
pub mod qemu_exit {
    pub const SUCCESS: u32 = 0x10;
    pub const FAILURE: u32 = 0x11;
    pub const PANIC: u32 = 0x3f;
}
