//! Typed syscall wrappers.

use core::arch::asm;

use spaceabi::error::{Error, decode};
use spaceabi::handle::{self, Handle};
use spaceabi::syscall::{
    ExitStatus, FileStat, HandleInfo, KernelStats, RecvArgs, SelfInfo, SpawnArgs, nr, recv_flags,
};

/// Raw syscall with up to six arguments. Public so tests can probe invalid numbers.
///
/// # Safety
/// The kernel validates everything, but pointer arguments must follow the ABI contract.
#[inline(always)]
pub unsafe fn raw(nr: usize, a0: u64, a1: u64, a2: u64, a3: u64, a4: u64, a5: u64) -> isize {
    let ret: isize;
    // SAFETY: the `syscall` instruction with the ABI v0 register convention.
    unsafe {
        asm!(
            "syscall",
            inlateout("rax") nr as isize => ret,
            in("rdi") a0,
            in("rsi") a1,
            in("rdx") a2,
            in("r10") a3,
            in("r8") a4,
            in("r9") a5,
            lateout("rcx") _,
            lateout("r11") _,
            options(nostack)
        );
    }
    ret
}

fn call(nr: usize, a: [u64; 6]) -> Result<usize, Error> {
    // SAFETY: see `raw`; all wrappers pass validated arguments.
    decode(unsafe { raw(nr, a[0], a[1], a[2], a[3], a[4], a[5]) })
}

pub fn exit(code: i32) -> ! {
    let _ = call(nr::EXIT, [code as u64, 0, 0, 0, 0, 0]);
    loop {
        core::hint::spin_loop();
    }
}

pub fn log(s: &str) {
    let _ = call(nr::LOG, [s.as_ptr() as u64, s.len() as u64, 0, 0, 0, 0]);
}

pub fn yield_now() {
    let _ = call(nr::YIELD, [0; 6]);
}

pub fn sleep_ms(ms: u64) {
    let _ = call(nr::SLEEP, [ms, 0, 0, 0, 0, 0]);
}

pub fn ticks_ms() -> u64 {
    call(nr::TICKS, [0; 6]).unwrap_or(0) as u64
}

pub fn mem_map(len: usize) -> Result<*mut u8, Error> {
    call(nr::MEM_MAP, [len as u64, 0, 0, 0, 0, 0]).map(|a| a as *mut u8)
}

pub fn mem_unmap(addr: *mut u8, len: usize) -> Result<(), Error> {
    call(nr::MEM_UNMAP, [addr as u64, len as u64, 0, 0, 0, 0]).map(|_| ())
}

pub fn channel_create() -> Result<(Handle, Handle), Error> {
    let mut out = [handle::INVALID; 2];
    call(nr::CHANNEL_CREATE, [out.as_mut_ptr() as u64, 0, 0, 0, 0, 0])?;
    Ok((out[0], out[1]))
}

pub fn send(h: Handle, data: &[u8], transfer: Option<Handle>) -> Result<(), Error> {
    let t = transfer.unwrap_or(handle::INVALID);
    call(nr::SEND, [h as u64, data.as_ptr() as u64, data.len() as u64, t as u64, 0, 0]).map(|_| ())
}

/// Receive into `buf`; returns `(len, transferred handle)`.
pub fn recv(h: Handle, buf: &mut [u8], nonblock: bool) -> Result<(usize, Option<Handle>), Error> {
    let mut args = RecvArgs {
        buf: buf.as_mut_ptr() as u64,
        buf_cap: buf.len() as u64,
        len: 0,
        handle: handle::INVALID,
        flags: if nonblock { recv_flags::NONBLOCK } else { 0 },
    };
    call(nr::RECV, [h as u64, &mut args as *mut RecvArgs as u64, 0, 0, 0, 0])?;
    let th = if args.handle == handle::INVALID { None } else { Some(args.handle) };
    Ok((args.len as usize, th))
}

pub fn handle_close(h: Handle) -> Result<(), Error> {
    call(nr::HANDLE_CLOSE, [h as u64, 0, 0, 0, 0, 0]).map(|_| ())
}

pub fn handle_dup(h: Handle, rights_mask: u32) -> Result<Handle, Error> {
    call(nr::HANDLE_DUP, [h as u64, rights_mask as u64, 0, 0, 0, 0]).map(|v| v as Handle)
}

pub fn handle_info(h: Handle) -> Result<HandleInfo, Error> {
    let mut info = HandleInfo::default();
    call(nr::HANDLE_INFO, [h as u64, &mut info as *mut HandleInfo as u64, 0, 0, 0, 0])?;
    Ok(info)
}

pub fn spawn(root: Handle, name: &str, quota_pages: u64, pass: Option<Handle>) -> Result<Handle, Error> {
    let args = SpawnArgs {
        name: name.as_ptr() as u64,
        name_len: name.len() as u64,
        quota_pages,
        pass_handle: pass.unwrap_or(handle::INVALID),
        _pad: 0,
    };
    call(nr::SPAWN, [root as u64, &args as *const SpawnArgs as u64, 0, 0, 0, 0]).map(|v| v as Handle)
}

pub fn wait(h: Handle) -> Result<ExitStatus, Error> {
    let mut st = ExitStatus::default();
    call(nr::WAIT, [h as u64, &mut st as *mut ExitStatus as u64, 0, 0, 0, 0])?;
    Ok(st)
}

pub fn kill(h: Handle) -> Result<(), Error> {
    call(nr::KILL, [h as u64, 0, 0, 0, 0, 0]).map(|_| ())
}

pub fn self_info() -> Result<SelfInfo, Error> {
    let mut info = SelfInfo::default();
    call(nr::SELF_INFO, [&mut info as *mut SelfInfo as u64, 0, 0, 0, 0, 0])?;
    Ok(info)
}

pub fn kstats(root: Handle) -> Result<KernelStats, Error> {
    let mut s = KernelStats::default();
    call(nr::KSTATS, [root as u64, &mut s as *mut KernelStats as u64, 0, 0, 0, 0])?;
    Ok(s)
}

pub fn shutdown(root: Handle, code: u32) -> Result<(), Error> {
    call(nr::SHUTDOWN, [root as u64, code as u64, 0, 0, 0, 0]).map(|_| ())
}

/// Allocate a shareable, zero-filled memory object (charged to this process).
pub fn vmo_create(len: usize) -> Result<Handle, Error> {
    call(nr::VMO_CREATE, [len as u64, 0, 0, 0, 0, 0]).map(|v| v as Handle)
}

/// Map a memory object into this address space; returns its base address.
pub fn vmo_map(h: Handle, read_only: bool) -> Result<*mut u8, Error> {
    let flags = if read_only { spaceabi::syscall::map_flags::READ_ONLY } else { 0 };
    call(nr::VMO_MAP, [h as u64, flags as u64, 0, 0, 0, 0]).map(|a| a as *mut u8)
}

pub fn vmo_size(h: Handle) -> Result<usize, Error> {
    call(nr::VMO_SIZE, [h as u64, 0, 0, 0, 0, 0])
}

/// Open a file on a mounted volume (requires the root FS right).
pub fn fs_open(root: Handle, path: &str) -> Result<Handle, Error> {
    call(nr::FS_OPEN, [root as u64, path.as_ptr() as u64, path.len() as u64, 0, 0, 0]).map(|v| v as Handle)
}

/// Read at `offset`; returns the number of bytes placed in `buf` (0 at end of file).
pub fn fs_read(file: Handle, offset: u64, buf: &mut [u8]) -> Result<usize, Error> {
    call(nr::FS_READ, [file as u64, offset, buf.as_mut_ptr() as u64, buf.len() as u64, 0, 0])
}

pub fn fs_stat(file: Handle) -> Result<FileStat, Error> {
    let mut st = FileStat::default();
    call(nr::FS_STAT, [file as u64, &mut st as *mut FileStat as u64, 0, 0, 0, 0])?;
    Ok(st)
}

pub fn debug(root: Handle, op: u64) -> Result<(), Error> {
    call(nr::DEBUG, [root as u64, op, 0, 0, 0, 0]).map(|_| ())
}
