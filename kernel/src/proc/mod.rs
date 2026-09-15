//! Processes: an address space, a handle table, a memory quota and (for now) one thread.

pub mod handles;

use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::sync::{Arc, Weak};
use core::sync::atomic::{AtomicU64, Ordering};

use spaceabi::PAGE_SIZE;
use spaceabi::elf::Elf;
use spaceabi::error::Error;
use spaceabi::handle::rights;
use spaceabi::syscall::ExitStatus;
use x86_64::structures::paging::PhysFrame;

use self::handles::{HandleEntry, HandleTable, Object};
use crate::mm::{AddressSpace, USER_SPACE_END};
use crate::sched::{self, Thread, WaitQueue};
use crate::sync::SpinLock;
use crate::{arch, initrd};

pub const USER_STACK_TOP: u64 = 0x0000_7FFF_F000_0000;
pub const USER_STACK_PAGES: usize = 16;
/// Quota for `init`: 8 MiB.
pub const INIT_QUOTA_PAGES: usize = 2048;
pub const MAX_QUOTA_PAGES: usize = 1 << 20; // 4 GiB

pub struct Process {
    pub pid: u64,
    pub name: String,
    pub space: SpinLock<AddressSpace>,
    pub cr3: PhysFrame,
    pub handles: SpinLock<HandleTable>,
    pub quota_pages: usize,
    pub status: SpinLock<Option<ExitStatus>>,
    pub exit_waiters: WaitQueue,
    pub pending_kill: SpinLock<Option<ExitStatus>>,
    pub thread: SpinLock<Option<Weak<Thread>>>,
}

static PROCESSES: SpinLock<BTreeMap<u64, Arc<Process>>> = SpinLock::new(BTreeMap::new());
static NEXT_PID: AtomicU64 = AtomicU64::new(1);

pub fn live_count() -> usize {
    PROCESSES.lock().len()
}

pub fn spawn_init() -> Result<Arc<Process>, Error> {
    let root = HandleEntry { object: Object::Root, rights: rights::ROOT_ALL };
    spawn("bin/init", INIT_QUOTA_PAGES, Some(root))
}

/// Load `name` from the initrd into a fresh address space and schedule its main thread.
pub fn spawn(name: &str, quota_pages: usize, bootstrap: Option<HandleEntry>) -> Result<Arc<Process>, Error> {
    if quota_pages == 0 || quota_pages > MAX_QUOTA_PAGES {
        return Err(Error::Invalid);
    }
    let image = initrd::find(name).ok_or(Error::NotFound)?;
    let elf = Elf::parse(image).map_err(|_| Error::NoExec)?;
    let mut space = AddressSpace::new()?;

    let page = PAGE_SIZE as u64;
    let stack_bottom = USER_STACK_TOP - (USER_STACK_PAGES as u64) * page;
    let mut needed = USER_STACK_PAGES;
    let mut segs = alloc::vec::Vec::new();
    for seg in elf.load_segments() {
        let seg = seg.map_err(|_| Error::NoExec)?;
        if seg.memsz == 0 {
            continue;
        }
        let start = seg.vaddr & !(page - 1);
        let end = (seg.vaddr + seg.memsz).div_ceil(page) * page;
        if start < page || end > stack_bottom || end > USER_SPACE_END {
            return Err(Error::NoExec);
        }
        let pages = ((end - start) / page) as usize;
        needed += pages;
        segs.push((seg, start, pages));
    }
    if segs.is_empty() {
        return Err(Error::NoExec);
    }
    // The entry point must land inside an executable segment: `iretq` to a
    // non-canonical address would fault in ring 0, and anything outside the image
    // is not a program we loaded.
    let entry_ok = segs
        .iter()
        .any(|(seg, _, _)| seg.executable() && elf.entry >= seg.vaddr && elf.entry < seg.vaddr + seg.memsz);
    if !entry_ok || elf.entry >= USER_SPACE_END {
        return Err(Error::NoExec);
    }
    if needed > quota_pages {
        return Err(Error::Quota);
    }
    for (seg, start, pages) in &segs {
        space.map_region(*start, *pages, seg.writable(), seg.executable()).map_err(|e| match e {
            Error::Invalid => Error::NoExec,
            e => e,
        })?;
        space.write_initial(seg.vaddr, elf.segment_data(seg))?;
    }
    space.map_region(stack_bottom, USER_STACK_PAGES, true, false)?;
    let used = space.used_pages;
    let cr3 = space.cr3();

    let pid = NEXT_PID.fetch_add(1, Ordering::Relaxed);
    let mut table = HandleTable::new();
    if let Some(b) = bootstrap {
        table.insert_at(spaceabi::handle::BOOTSTRAP as usize, b);
    }
    let proc = Arc::new(Process {
        pid,
        name: String::from(name),
        space: SpinLock::new(space),
        cr3,
        handles: SpinLock::new(table),
        quota_pages,
        status: SpinLock::new(None),
        exit_waiters: WaitQueue::new(),
        pending_kill: SpinLock::new(None),
        thread: SpinLock::new(None),
    });
    let thread = Thread::new_user(proc.clone(), elf.entry, USER_STACK_TOP)?;
    *proc.thread.lock() = Some(Arc::downgrade(&thread));
    PROCESSES.lock().insert(pid, proc.clone());
    println!(
        "[kernel] spawn pid {pid} '{name}': entry={:#x}, {} pages mapped, quota {} pages",
        elf.entry, used, quota_pages
    );
    sched::add(thread);
    Ok(proc)
}

pub fn current_process() -> Option<Arc<Process>> {
    sched::current().process.clone()
}

pub fn current_identity() -> (u64, String) {
    match current_process() {
        Some(p) => (p.pid, p.name.clone()),
        None => (0, String::from("kernel")),
    }
}

fn describe(status: &ExitStatus) -> String {
    if status.kind == spaceabi::syscall::exit_kind::EXITED {
        alloc::format!("exited with code {}", status.code)
    } else {
        alloc::format!(
            "killed ({}, addr {:#x})",
            spaceabi::syscall::kill_reason::name(status.reason),
            status.fault_addr
        )
    }
}

fn release_current(status: ExitStatus) {
    let proc = current_process().expect("exit_current on the idle thread");
    println!("[kernel] pid {} '{}' {}", proc.pid, proc.name, describe(&status));
    // Leave the dying address space before tearing it down.
    crate::mm::paging::activate_kernel();
    *proc.status.lock() = Some(status);
    proc.exit_waiters.wake_all();
    // Closing handles may close channel endpoints and wake peers.
    proc.handles.lock().clear();
    proc.space.lock().teardown();
    PROCESSES.lock().remove(&proc.pid);
    *proc.thread.lock() = None;
}

/// Terminate the current process. Never returns.
pub fn exit_current(status: ExitStatus) -> ! {
    arch::disable_interrupts();
    release_current(status);
    sched::exit_current_thread()
}

/// Ask another process to die. It is terminated the next time it would run user code
/// (or immediately if blocked in the kernel).
pub fn kill(target: &Arc<Process>, status: ExitStatus) -> Result<(), Error> {
    if target.status.lock().is_some() {
        return Err(Error::Exited);
    }
    *target.pending_kill.lock() = Some(status);
    // A self-kill needs no wake-up: the caller is running and dies on syscall return.
    let is_self = current_process().is_some_and(|p| Arc::ptr_eq(&p, target));
    if !is_self {
        let thread = target.thread.lock().as_ref().and_then(Weak::upgrade);
        if let Some(t) = thread {
            sched::wake(&t);
        }
    }
    Ok(())
}

/// True when the current process has been asked to die. Blocking kernel paths use
/// this to unwind (`Error::Interrupted`) so that every `Arc` on the kernel stack is
/// dropped before `check_pending_kill` terminates the thread for good.
pub fn has_pending_kill() -> bool {
    current_process().is_some_and(|p| p.pending_kill.lock().is_some())
}

/// Called before returning to user mode (syscall epilogue, trap return). Never call
/// it while holding kernel references on the stack: it does not return.
pub fn check_pending_kill() {
    if let Some(p) = current_process() {
        let pending = p.pending_kill.lock().take();
        if let Some(st) = pending {
            drop(p);
            exit_current(st);
        }
    }
}
