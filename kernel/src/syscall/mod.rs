//! Syscall dispatch (ABI v0, see `spaceabi::syscall`).
//!
//! Every user pointer is validated against the caller's page tables before it is
//! touched; every handle is checked for existence, object kind and rights.

use alloc::string::String;
use alloc::sync::Arc;

use spaceabi::error::{Error, encode};
use spaceabi::handle::{self, Handle, rights};
use spaceabi::syscall::{
    DIR_ENTRIES_MAX, DirEntry, ExitStatus, FileStat, HandleInfo, KernelStats, MSG_MAX, PATH_MAX, RecvArgs,
    SelfInfo, SpawnArgs, debug_op, kill_reason, map_flags, nr, qemu_exit, recv_flags, wait_flags,
};

use crate::arch;
use crate::arch::syscall::SyscallFrame;
use crate::fs;
use crate::ipc::channel::{Endpoint, Message};
use crate::mm::{PAGE_SIZE, frame, heap};
use crate::proc::handles::{HandleEntry, MemoryObject, Object, OpenFile};
use crate::proc::{self, Process};
use crate::sched;

/// Largest single user buffer a syscall will touch.
const MAX_USER_COPY: u64 = 1024 * 1024;
const MAX_NAME: u64 = 128;

pub fn dispatch(frame: &mut SyscallFrame) -> isize {
    arch::enable_interrupts();
    let a = [frame.arg0, frame.arg1, frame.arg2, frame.arg3, frame.arg4, frame.arg5];
    let r = match frame.nr as usize {
        nr::EXIT => proc::exit_current(ExitStatus::exited(a[0] as i32)),
        nr::LOG => sys_log(a[0], a[1]),
        nr::YIELD => {
            sched::yield_now();
            Ok(0)
        }
        nr::SLEEP => sched::sleep_ms(a[0]).map(|()| 0),
        nr::TICKS => Ok(sched::uptime_ms() as usize),
        nr::MEM_MAP => sys_mem_map(a[0], a[1] as u32),
        nr::MEM_UNMAP => sys_mem_unmap(a[0], a[1]),
        nr::CHANNEL_CREATE => sys_channel_create(a[0]),
        nr::SEND => sys_send(a[0] as Handle, a[1], a[2], a[3] as Handle),
        nr::RECV => sys_recv(a[0] as Handle, a[1]),
        nr::HANDLE_CLOSE => sys_handle_close(a[0] as Handle),
        nr::HANDLE_DUP => sys_handle_dup(a[0] as Handle, a[1] as u32),
        nr::SPAWN => sys_spawn(a[0] as Handle, a[1]),
        nr::WAIT => sys_wait(a[0] as Handle, a[1], a[2] as u32),
        nr::KILL => sys_kill(a[0] as Handle),
        nr::SELF_INFO => sys_self_info(a[0]),
        nr::KSTATS => sys_kstats(a[0] as Handle, a[1]),
        nr::SHUTDOWN => sys_shutdown(a[0] as Handle, a[1] as u32),
        nr::DEBUG => sys_debug(a[0] as Handle, a[1]),
        nr::HANDLE_INFO => sys_handle_info(a[0] as Handle, a[1]),
        nr::FS_OPEN => sys_fs_open(a[0] as Handle, a[1], a[2]),
        nr::FS_READ => sys_fs_read(a[0] as Handle, a[1], a[2], a[3]),
        nr::FS_STAT => sys_fs_stat(a[0] as Handle, a[1]),
        nr::VMO_CREATE => sys_vmo_create(a[0]),
        nr::VMO_MAP => sys_vmo_map(a[0] as Handle, a[1] as u32),
        nr::VMO_SIZE => sys_vmo_size(a[0] as Handle),
        nr::FS_LIST => sys_fs_list(a[0] as Handle, a[1], a[2], a[3], a[4]),
        nr::CONSOLE_READ => sys_console_read(a[0] as Handle, a[1], a[2]),
        _ => Err(Error::NoSys),
    };
    arch::disable_interrupts();
    proc::check_pending_kill();
    // Defence in depth for the Intel `sysret` hazard: returning to a non-canonical
    // rip raises #GP in ring 0. User mappings never reach the top of the lower half,
    // but a process whose return address is not user space is terminated instead.
    if frame.rip >= crate::mm::USER_SPACE_END {
        proc::exit_current(ExitStatus::killed(kill_reason::GENERAL_PROTECTION, frame.rip));
    }
    encode(r)
}

fn with_file<R>(
    h: Handle,
    need: u32,
    f: impl FnOnce(&Arc<OpenFile>) -> Result<R, Error>,
) -> Result<R, Error> {
    let p = current();
    let file = {
        let t = p.handles.lock();
        let e = t.get(h)?;
        match &e.object {
            Object::File(f) if e.has(need) => f.clone(),
            _ => return Err(Error::Denied),
        }
    };
    f(&file)
}

fn sys_fs_open(root: Handle, path_ptr: u64, path_len: u64) -> Result<usize, Error> {
    require_root(root, rights::FS)?;
    heap::reserve(cost::HANDLE)?;
    let path = read_user_str(path_ptr, path_len, PATH_MAX as u64)?;
    let p = current();
    if !p.handles.lock().has_free_slot() {
        return Err(Error::TooManyHandles);
    }
    let node = fs::open(&path)?;
    let entry = HandleEntry { object: Object::File(Arc::new(OpenFile { node })), rights: rights::FILE_ALL };
    Ok(p.handles.lock().insert(entry)? as usize)
}

fn sys_fs_read(h: Handle, offset: u64, buf_ptr: u64, len: u64) -> Result<usize, Error> {
    if len > MAX_USER_COPY {
        return Err(Error::Invalid);
    }
    // Validate the destination before touching the device so a bad pointer cannot
    // consume a read.
    user_bytes(buf_ptr, len, true)?;
    // The bounce buffer is kernel heap driven straight by a user-supplied length:
    // charge it against the reserve so it can never eat into the headroom the
    // kernel's own infallible allocations depend on.
    heap::reserve(len as usize)?;
    let node = with_file(h, rights::READ, |f| Ok(f.node))?;
    let mut tmp = alloc::vec::Vec::new();
    tmp.try_reserve_exact(len as usize).map_err(|_| Error::NoMemory)?;
    tmp.resize(len as usize, 0);
    let n = fs::read(&node, offset, &mut tmp)?;
    let buf = user_bytes(buf_ptr, len, true)?;
    buf[..n].copy_from_slice(&tmp[..n]);
    Ok(n)
}

/// List a directory. `cap` entries fit in the caller's buffer; the return value is
/// how many were written, capped at [`DIR_ENTRIES_MAX`].
fn sys_fs_list(root: Handle, path_ptr: u64, path_len: u64, out: u64, cap: u64) -> Result<usize, Error> {
    require_root(root, rights::FS)?;
    if cap == 0 {
        return Err(Error::Invalid);
    }
    let want = (cap as usize).min(DIR_ENTRIES_MAX);
    let bytes = (want * core::mem::size_of::<DirEntry>()) as u64;
    user_bytes(out, bytes, true)?;
    let path = read_user_str(path_ptr, path_len, PATH_MAX as u64)?;
    heap::reserve(want * core::mem::size_of::<DirEntry>())?;
    let entries = fs::list(&path, want)?;
    let dst = user_bytes(out, bytes, true)?;
    for (i, e) in entries.iter().enumerate() {
        let off = i * core::mem::size_of::<DirEntry>();
        // SAFETY: `dst` is `want * size_of::<DirEntry>()` writable user bytes and
        // `i < want`; DirEntry is plain `repr(C)` data with no padding requirements
        // beyond 8-byte alignment, so an unaligned write is used.
        unsafe { core::ptr::write_unaligned(dst[off..].as_mut_ptr() as *mut DirEntry, *e) };
    }
    Ok(entries.len())
}

/// Hand user space whatever has been typed. Never blocks: a session service polls
/// this alongside its control channel, and blocking here would be one more way for
/// it to stop answering.
fn sys_console_read(root: Handle, buf_ptr: u64, len: u64) -> Result<usize, Error> {
    require_root(root, rights::CONSOLE)?;
    if len > MAX_USER_COPY {
        return Err(Error::Invalid);
    }
    let buf = user_bytes(buf_ptr, len, true)?;
    // Loss first, bytes second. A reader that is told "the stream has a hole in it"
    // only after it has been handed the bytes on the far side of the hole has already
    // assembled a line nobody typed. Nothing is consumed here: the surviving bytes
    // are still queued for the next call.
    let dropped = crate::input::take_dropped();
    if dropped > 0 {
        println!("[kernel] console input: {dropped} byte(s) dropped, the buffer was full");
        return Err(Error::DataLoss);
    }
    let n = crate::input::read(buf);
    // The ring has room again, so a serial port that had to be paused mid-flood can
    // start delivering once more.
    crate::arch::serial::resume_input();
    Ok(n)
}

fn sys_fs_stat(h: Handle, out: u64) -> Result<usize, Error> {
    let stat = with_file(h, rights::READ, |f| {
        Ok(FileStat { size: f.node.size, block_size: crate::dev::virtio_blk::SECTOR_SIZE })
    })?;
    write_user(out, stat)?;
    Ok(0)
}

fn with_memory<R>(
    h: Handle,
    need: u32,
    f: impl FnOnce(&Arc<MemoryObject>, u32) -> Result<R, Error>,
) -> Result<R, Error> {
    let p = current();
    let (obj, held) = {
        let t = p.handles.lock();
        let e = t.get(h)?;
        match &e.object {
            Object::Memory(m) if e.has(need) => (m.clone(), e.rights),
            _ => return Err(Error::Denied),
        }
    };
    f(&obj, held)
}

fn sys_vmo_create(len: u64) -> Result<usize, Error> {
    if len == 0 || len > spaceabi::compute::MAX_BUFFER_BYTES {
        return Err(Error::Invalid);
    }
    heap::reserve(cost::HANDLE)?;
    let pages = len.div_ceil(PAGE_SIZE) as usize;
    let p = current();
    if !p.handles.lock().has_free_slot() {
        return Err(Error::TooManyHandles);
    }
    // The creator pays for the frames; mapping the object elsewhere is free.
    if p.space.lock().used_pages + pages > p.quota_pages {
        return Err(Error::Quota);
    }
    let mut frames = alloc::vec::Vec::new();
    frames.try_reserve_exact(pages).map_err(|_| Error::NoMemory)?;
    for _ in 0..pages {
        match frame::alloc_zeroed() {
            Some(f) => frames.push(f),
            None => {
                for f in frames.drain(..) {
                    frame::free(f);
                }
                return Err(Error::NoMemory);
            }
        }
    }
    p.space.lock().used_pages += pages;
    // From here the object owns the frames: dropping it frees them and refunds the
    // quota, so a failed insert cannot leak.
    let obj = Arc::new(MemoryObject { frames, len, owner: Arc::downgrade(&p) });
    let entry = HandleEntry { object: Object::Memory(obj), rights: rights::MEMORY_ALL };
    Ok(p.handles.lock().insert(entry)? as usize)
}

fn sys_vmo_map(h: Handle, flags: u32) -> Result<usize, Error> {
    if flags & !map_flags::READ_ONLY != 0 {
        return Err(Error::Invalid);
    }
    let (obj, writable) = with_memory(h, rights::MAP | rights::READ, |m, held| {
        Ok((m.clone(), held & rights::WRITE != 0 && flags & map_flags::READ_ONLY == 0))
    })?;
    let p = current();
    // The mapping holds its own reference to the object, so the frames survive for
    // as long as they are mapped even if every handle to the object is closed.
    let addr = p.space.lock().map_shared(obj, writable)?;
    Ok(addr as usize)
}

fn sys_vmo_size(h: Handle) -> Result<usize, Error> {
    with_memory(h, rights::READ, |m, _| Ok(m.len as usize))
}

/// Rough kernel-heap cost of the objects a syscall may create.
mod cost {
    pub const MESSAGE: usize = 1024;
    pub const HANDLE: usize = 128;
    pub const CHANNEL: usize = 4096;
    pub const PROCESS: usize = 256 * 1024;
}

fn current() -> Arc<Process> {
    proc::current_process().expect("syscall from a kernel thread")
}

// ---- user memory ------------------------------------------------------------

fn user_bytes<'a>(addr: u64, len: u64, write: bool) -> Result<&'a mut [u8], Error> {
    if len > MAX_USER_COPY {
        return Err(Error::Invalid);
    }
    let p = current();
    let ok = p.space.lock().check_user_range(addr, len, write);
    if !ok {
        return Err(Error::Fault);
    }
    if len == 0 {
        // `from_raw_parts_mut` requires a non-null, aligned pointer even for length 0,
        // and user space is free to pass 0 as the address of an empty buffer.
        return Ok(&mut []);
    }
    // SAFETY: the range is mapped user memory of the current address space with the
    // requested access; the process is single-threaded and blocked in this syscall,
    // so nobody can unmap it underneath us.
    Ok(unsafe { core::slice::from_raw_parts_mut(addr as *mut u8, len as usize) })
}

fn read_user<T: Copy>(addr: u64) -> Result<T, Error> {
    let b = user_bytes(addr, core::mem::size_of::<T>() as u64, false)?;
    // SAFETY: `b` is size_of::<T>() readable bytes; T is a plain `repr(C)` value type.
    Ok(unsafe { core::ptr::read_unaligned(b.as_ptr() as *const T) })
}

fn write_user<T: Copy>(addr: u64, v: T) -> Result<(), Error> {
    let b = user_bytes(addr, core::mem::size_of::<T>() as u64, true)?;
    // SAFETY: `b` is size_of::<T>() writable bytes.
    unsafe { core::ptr::write_unaligned(b.as_mut_ptr() as *mut T, v) };
    Ok(())
}

fn read_user_str(addr: u64, len: u64, max: u64) -> Result<String, Error> {
    if len > max {
        return Err(Error::Invalid);
    }
    let b = user_bytes(addr, len, false)?;
    core::str::from_utf8(b).map(String::from).map_err(|_| Error::Invalid)
}

// ---- handles -----------------------------------------------------------------

fn with_channel<R>(
    h: Handle,
    need: u32,
    f: impl FnOnce(&Arc<Endpoint>) -> Result<R, Error>,
) -> Result<R, Error> {
    let p = current();
    let ep = {
        let t = p.handles.lock();
        let e = t.get(h)?;
        match &e.object {
            Object::Channel(ep) if e.has(need) => ep.clone(),
            _ => return Err(Error::Denied),
        }
    };
    f(&ep)
}

fn with_process<R>(
    h: Handle,
    need: u32,
    f: impl FnOnce(&Arc<Process>) -> Result<R, Error>,
) -> Result<R, Error> {
    let p = current();
    let target = {
        let t = p.handles.lock();
        let e = t.get(h)?;
        match &e.object {
            Object::Process(tp) if e.has(need) => tp.clone(),
            _ => return Err(Error::Denied),
        }
    };
    f(&target)
}

fn require_root(h: Handle, need: u32) -> Result<(), Error> {
    let p = current();
    let t = p.handles.lock();
    let e = t.get(h)?;
    match e.object {
        Object::Root if e.has(need) => Ok(()),
        _ => Err(Error::Denied),
    }
}

// ---- syscalls ------------------------------------------------------------------

fn sys_log(ptr: u64, len: u64) -> Result<usize, Error> {
    if len > MSG_MAX as u64 * 4 {
        return Err(Error::Invalid);
    }
    let b = user_bytes(ptr, len, false)?;
    let s = core::str::from_utf8(b).map_err(|_| Error::Invalid)?;
    crate::print!("{s}");
    Ok(len as usize)
}

fn sys_mem_map(len: u64, flags: u32) -> Result<usize, Error> {
    if flags != map_flags::NONE || len == 0 || len > (1u64 << 30) {
        return Err(Error::Invalid);
    }
    heap::reserve(cost::HANDLE)?;
    let pages = len.div_ceil(PAGE_SIZE) as usize;
    let p = current();
    let mut space = p.space.lock();
    if space.used_pages + pages > p.quota_pages {
        return Err(Error::Quota);
    }
    let addr = space.map_anonymous(pages)?;
    Ok(addr as usize)
}

fn sys_mem_unmap(addr: u64, len: u64) -> Result<usize, Error> {
    if len == 0 {
        return Err(Error::Invalid);
    }
    let pages = len.div_ceil(PAGE_SIZE) as usize;
    let p = current();
    // Bind the detached object: dropping it refunds the creator's quota, and that
    // creator may be this process, whose address-space lock is released first.
    let _object = p.space.lock().unmap_region(addr, pages)?;
    Ok(0)
}

fn sys_channel_create(out: u64) -> Result<usize, Error> {
    // Validate the output pointer before creating anything.
    user_bytes(out, 8, true)?;
    heap::reserve(cost::CHANNEL)?;
    let (a, b) = Endpoint::pair();
    let p = current();
    let (ha, hb) = {
        let mut t = p.handles.lock();
        let ha = t.insert(HandleEntry { object: Object::Channel(a), rights: rights::CHANNEL_ALL })?;
        let hb = match t.insert(HandleEntry { object: Object::Channel(b), rights: rights::CHANNEL_ALL }) {
            Ok(h) => h,
            Err(e) => {
                let _ = t.take(ha);
                return Err(e);
            }
        };
        (ha, hb)
    };
    write_user(out, [ha, hb])?;
    Ok(0)
}

fn sys_send(h: Handle, ptr: u64, len: u64, transfer: Handle) -> Result<usize, Error> {
    if len > MSG_MAX as u64 {
        return Err(Error::MsgSize);
    }
    if transfer != handle::INVALID && transfer == h {
        return Err(Error::Invalid);
    }
    heap::reserve(cost::MESSAGE)?;
    let src = user_bytes(ptr, len, false)?;
    let mut data = alloc::vec::Vec::new();
    data.try_reserve_exact(src.len()).map_err(|_| Error::NoMemory)?;
    data.extend_from_slice(src);
    let p = current();
    // Validate everything before taking the transferred handle out of the table.
    let (ep, moved) = {
        let mut t = p.handles.lock();
        let e = t.get(h)?;
        let ep = match &e.object {
            Object::Channel(ep) if e.has(rights::SEND) => ep.clone(),
            _ => return Err(Error::Denied),
        };
        let moved = if transfer != handle::INVALID {
            let te = t.get(transfer)?;
            if !te.has(rights::TRANSFER) {
                return Err(Error::Denied);
            }
            // An endpoint of this very channel travelling through it would create a
            // reference cycle nobody can ever receive: refuse it.
            if let Object::Channel(other) = &te.object
                && other.same_channel(&ep)
            {
                return Err(Error::Invalid);
            }
            Some(t.take(transfer)?)
        } else {
            None
        };
        (ep, moved)
    };
    // If the send fails the message (and any handle moved into it) is dropped: the
    // handle is consumed either way, which keeps transfer semantics simple (ADR-0004).
    ep.send(Message { data, handle: moved })?;
    Ok(0)
}

fn sys_recv(h: Handle, args_ptr: u64) -> Result<usize, Error> {
    // The args block is written back with the result, so it must be writable up
    // front: a message must never be dequeued and then lost on a late Fault.
    user_bytes(args_ptr, core::mem::size_of::<RecvArgs>() as u64, true)?;
    heap::reserve(cost::HANDLE)?;
    let args: RecvArgs = read_user(args_ptr)?;
    if args.buf_cap > MSG_MAX as u64 * 4 {
        return Err(Error::Invalid);
    }
    let nonblock = args.flags & recv_flags::NONBLOCK != 0;
    if args.flags & !recv_flags::NONBLOCK != 0 {
        return Err(Error::Invalid);
    }
    // Validate the buffer up front so a message is never dequeued and then lost.
    user_bytes(args.buf, args.buf_cap, true)?;
    let ep = with_channel(h, rights::RECV, |ep| Ok(ep.clone()))?;
    let mut msg = ep.recv(args.buf_cap as usize, nonblock)?;
    // Install the transferred handle before anything else can fail: if the table is
    // full the whole message goes back to the head of the queue, so neither the
    // payload nor the capability inside it is destroyed by a retryable error.
    let handle = match msg.handle.take() {
        Some(entry) => match current().handles.lock().insert(entry) {
            Ok(h) => h,
            Err(e) => {
                ep.requeue(msg);
                return Err(e);
            }
        },
        None => handle::INVALID,
    };
    let buf = user_bytes(args.buf, args.buf_cap, true)?;
    buf[..msg.data.len()].copy_from_slice(&msg.data);
    let out = RecvArgs {
        buf: args.buf,
        buf_cap: args.buf_cap,
        len: msg.data.len() as u64,
        handle,
        flags: args.flags,
    };
    write_user(args_ptr, out)?;
    Ok(0)
}

fn sys_handle_close(h: Handle) -> Result<usize, Error> {
    let entry = current().handles.lock().take(h)?;
    drop(entry);
    Ok(0)
}

fn sys_handle_dup(h: Handle, mask: u32) -> Result<usize, Error> {
    heap::reserve(cost::HANDLE)?;
    let p = current();
    let mut t = p.handles.lock();
    let e = t.get(h)?;
    if !e.has(rights::DUP) {
        return Err(Error::Denied);
    }
    let new = HandleEntry { object: e.object.clone_ref(), rights: e.rights & mask };
    Ok(t.insert(new)? as usize)
}

fn sys_handle_info(h: Handle, out: u64) -> Result<usize, Error> {
    let info = {
        let p = current();
        let t = p.handles.lock();
        let e = t.get(h)?;
        HandleInfo { kind: e.object.kind(), rights: e.rights }
    };
    write_user(out, info)?;
    Ok(0)
}

fn sys_spawn(root: Handle, args_ptr: u64) -> Result<usize, Error> {
    require_root(root, rights::SPAWN)?;
    heap::reserve(cost::PROCESS)?;
    let args: SpawnArgs = read_user(args_ptr)?;
    let name = read_user_str(args.name, args.name_len, MAX_NAME)?;
    let p = current();
    let bootstrap = if args.pass_handle != handle::INVALID {
        let mut t = p.handles.lock();
        let e = t.get(args.pass_handle)?;
        if !e.has(rights::TRANSFER) {
            return Err(Error::Denied);
        }
        Some(t.take(args.pass_handle)?)
    } else {
        None
    };
    // Refuse before creating anything when the caller could not hold the process
    // handle: a child the parent cannot see is unkillable.
    if !p.handles.lock().has_free_slot() {
        return Err(Error::TooManyHandles);
    }
    // A failed spawn drops the bootstrap handle (same consume-on-transfer rule as send).
    let child = proc::spawn(&name, args.quota_pages as usize, bootstrap)?;
    let entry = HandleEntry { object: Object::Process(child.clone()), rights: rights::PROCESS_ALL };
    match p.handles.lock().insert(entry) {
        Ok(h) => Ok(h as usize),
        Err(e) => {
            // Lost the last slot after all: terminate the child rather than leaving a
            // running process nobody holds a handle to.
            let _ = proc::kill(&child, ExitStatus::killed(kill_reason::SIGNAL, 0));
            Err(e)
        }
    }
}

fn sys_wait(h: Handle, out: u64, flags: u32) -> Result<usize, Error> {
    if flags & !wait_flags::NONBLOCK != 0 {
        return Err(Error::Invalid);
    }
    user_bytes(out, core::mem::size_of::<ExitStatus>() as u64, true)?;
    let status = with_process(h, rights::WAIT, |target| {
        if Arc::ptr_eq(target, &current()) {
            return Err(Error::Invalid);
        }
        crate::sync::without_interrupts(|| {
            loop {
                {
                    let st = target.status.lock();
                    if let Some(s) = *st {
                        return Ok(s);
                    }
                    if flags & wait_flags::NONBLOCK != 0 {
                        return Err(Error::WouldBlock);
                    }
                    target.exit_waiters.sleep_after(move || drop(st))?;
                }
                if proc::has_pending_kill() {
                    return Err(Error::Interrupted);
                }
            }
        })
    })?;
    write_user(out, status)?;
    Ok(0)
}

fn sys_kill(h: Handle) -> Result<usize, Error> {
    with_process(h, rights::KILL, |target| {
        proc::kill(target, ExitStatus::killed(kill_reason::SIGNAL, 0))?;
        Ok(0)
    })
}

fn sys_self_info(out: u64) -> Result<usize, Error> {
    let p = current();
    let used = p.space.lock().used_pages as u64;
    let info = SelfInfo {
        pid: p.pid,
        quota_pages: p.quota_pages as u64,
        used_pages: used,
        abi_version: spaceabi::ABI_VERSION,
        _pad: 0,
    };
    write_user(out, info)?;
    Ok(0)
}

pub fn kernel_stats() -> KernelStats {
    let (frames_total, frames_free) = frame::stats();
    let (heap_total, heap_used) = heap::stats();
    let (threads_live, context_switches) = sched::stats();
    KernelStats {
        frames_total: frames_total as u64,
        frames_free: frames_free as u64,
        heap_total: heap_total as u64,
        heap_used: heap_used as u64,
        processes_live: proc::live_count() as u64,
        threads_live,
        uptime_ms: sched::uptime_ms(),
        context_switches,
        volume_sectors: fs::volume_sectors(),
    }
}

fn sys_kstats(root: Handle, out: u64) -> Result<usize, Error> {
    require_root(root, rights::STATS)?;
    write_user(out, kernel_stats())?;
    Ok(0)
}

fn sys_shutdown(root: Handle, code: u32) -> Result<usize, Error> {
    require_root(root, rights::SHUTDOWN)?;
    let (pid, name) = proc::current_identity();
    let s = kernel_stats();
    println!(
        "[kernel] shutdown requested by pid {pid} '{name}' with code {code} (uptime {} ms, {} context switches)",
        s.uptime_ms, s.context_switches
    );
    arch::disable_interrupts();
    arch::qemu_exit(if code == 0 { qemu_exit::SUCCESS } else { qemu_exit::FAILURE });
    println!("[kernel] no debug-exit device; halting");
    arch::halt_forever();
}

fn sys_debug(root: Handle, op: u64) -> Result<usize, Error> {
    require_root(root, rights::DEBUG)?;
    let (pid, _) = proc::current_identity();
    match op {
        debug_op::PANIC => panic!("deliberate kernel panic requested by pid {pid} (SYS_DEBUG PANIC)"),
        debug_op::KERNEL_FAULT => {
            println!("[kernel] pid {pid} requested a deliberate kernel-mode page fault");
            let bad = 0xFFFF_F000_DEAD_0000u64 as *const u64;
            // SAFETY: intentionally unsound: this address is never mapped; the fault is the point.
            let v = unsafe { core::ptr::read_volatile(bad) };
            Ok(v as usize)
        }
        debug_op::CONSOLE_FLOOD => {
            // The console ring is fed by interrupts, so user space cannot make it
            // overflow on purpose. Without this, the input-loss path would be code no
            // test ever runs -- and untested error paths are where a terminal quietly
            // starts executing commands nobody typed.
            for i in 0..spaceabi::syscall::CONSOLE_FLOOD_LEN {
                crate::input::push(spaceabi::syscall::console_flood_byte(i));
            }
            Ok(0)
        }
        _ => Err(Error::Invalid),
    }
}
