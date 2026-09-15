//! Round-robin preemptive scheduler (single CPU).
//!
//! Invariants:
//! * `schedule()` is entered and left with interrupts disabled;
//! * a thread that blocks registers itself somewhere (wait queue, sleepers) and sets
//!   its state to `Blocked` *before* calling `schedule()`, with interrupts disabled,
//!   so a wake-up cannot be lost;
//! * the thread that runs right after a switch reaps the previous thread if it died
//!   (`finish_switch`), so a dead thread's kernel stack is freed by someone else.

use alloc::collections::VecDeque;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicU8, AtomicU64, Ordering};

use spaceabi::error::Error;

use crate::arch;
use crate::arch::context;
use crate::mm::kstack::KernelStack;
use crate::proc::Process;
use crate::sync::SpinLock;

pub const TICK_HZ: u32 = 1000;
const QUANTUM_TICKS: u32 = 10;

#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ThreadState {
    Ready = 0,
    Running = 1,
    Blocked = 2,
    Dead = 3,
}

pub struct Thread {
    #[allow(dead_code)]
    pub tid: u64,
    pub process: Option<Arc<Process>>,
    _kstack: Option<KernelStack>,
    pub kstack_top: u64,
    saved_rsp: UnsafeCell<u64>,
    state: AtomicU8,
    pub user_entry: (u64, u64),
}

// SAFETY: `saved_rsp` is only touched by `schedule()` with interrupts disabled.
unsafe impl Sync for Thread {}
unsafe impl Send for Thread {}

static NEXT_TID: AtomicU64 = AtomicU64::new(1);

impl Thread {
    pub fn new_user(process: Arc<Process>, rip: u64, rsp: u64) -> Result<Arc<Thread>, Error> {
        let ks = KernelStack::new()?;
        let top = ks.top;
        // SAFETY: `top` is the top of a freshly mapped kernel stack.
        let rsp0 = unsafe { context::prepare_initial_stack(top, user_thread_trampoline) };
        Ok(Arc::new(Thread {
            tid: NEXT_TID.fetch_add(1, Ordering::Relaxed),
            process: Some(process),
            _kstack: Some(ks),
            kstack_top: top,
            saved_rsp: UnsafeCell::new(rsp0),
            state: AtomicU8::new(ThreadState::Ready as u8),
            user_entry: (rip, rsp),
        }))
    }

    pub fn state(&self) -> ThreadState {
        match self.state.load(Ordering::Relaxed) {
            0 => ThreadState::Ready,
            1 => ThreadState::Running,
            2 => ThreadState::Blocked,
            _ => ThreadState::Dead,
        }
    }

    pub fn set_state(&self, s: ThreadState) {
        self.state.store(s as u8, Ordering::Relaxed);
    }

    pub fn name(&self) -> String {
        match &self.process {
            Some(p) => alloc::format!("pid {} '{}'", p.pid, p.name),
            None => String::from("idle"),
        }
    }
}

struct Scheduler {
    run_queue: VecDeque<Arc<Thread>>,
    current: Option<Arc<Thread>>,
    idle: Option<Arc<Thread>>,
    sleepers: Vec<(u64, Arc<Thread>)>,
    prev_dead: Option<Arc<Thread>>,
    ticks: u64,
    quantum_left: u32,
    switches: u64,
    live_threads: u64,
}

static SCHED: SpinLock<Scheduler> = SpinLock::new(Scheduler {
    run_queue: VecDeque::new(),
    current: None,
    idle: None,
    sleepers: Vec::new(),
    prev_dead: None,
    ticks: 0,
    quantum_left: QUANTUM_TICKS,
    switches: 0,
    live_threads: 0,
});

/// Turn the boot context into the idle thread. `idle_stack_top` is the top of the
/// guarded kernel stack the boot context was moved onto.
pub fn init(idle_stack_top: u64) {
    let idle = Arc::new(Thread {
        tid: 0,
        process: None,
        _kstack: None,
        kstack_top: idle_stack_top,
        saved_rsp: UnsafeCell::new(0),
        state: AtomicU8::new(ThreadState::Running as u8),
        user_entry: (0, 0),
    });
    let mut s = SCHED.lock();
    s.current = Some(idle.clone());
    s.idle = Some(idle);
    println!("[kernel] scheduler: {} Hz tick, {} ms quantum", TICK_HZ, QUANTUM_TICKS * 1000 / TICK_HZ);
}

pub fn add(thread: Arc<Thread>) {
    let mut s = SCHED.lock();
    thread.set_state(ThreadState::Ready);
    s.run_queue.push_back(thread);
    s.live_threads += 1;
}

pub fn current() -> Arc<Thread> {
    SCHED.lock().current.clone().expect("scheduler not initialised")
}

/// Name of the current thread, if the scheduler lock is free (panic path).
pub fn try_current_name() -> Option<String> {
    let s = SCHED.try_lock()?;
    s.current.as_ref().map(|t| t.name())
}

pub fn uptime_ms() -> u64 {
    match SCHED.try_lock() {
        Some(s) => s.ticks * 1000 / TICK_HZ as u64,
        None => 0,
    }
}

/// `(live threads, context switches)`.
pub fn stats() -> (u64, u64) {
    let s = SCHED.lock();
    (s.live_threads, s.switches)
}

/// Pick the next runnable thread and switch to it. Interrupts must be disabled.
pub fn schedule() {
    debug_assert!(!arch::interrupts_enabled(), "schedule() with interrupts enabled");
    let prev_slot: *mut u64;
    let next_rsp: u64;
    let next_top: u64;
    let next_cr3;
    {
        let mut s = SCHED.lock();
        let prev = s.current.clone().expect("no current thread");
        let idle = s.idle.clone().expect("no idle thread");
        let prev_is_idle = Arc::ptr_eq(&prev, &idle);
        let prev_state = prev.state();
        let prev_runnable = matches!(prev_state, ThreadState::Running | ThreadState::Ready);

        let next = match s.run_queue.pop_front() {
            Some(t) => t,
            None if prev_runnable => return, // nothing else to run: keep going
            None => idle.clone(),
        };
        if prev_runnable && !prev_is_idle {
            prev.set_state(ThreadState::Ready);
            s.run_queue.push_back(prev.clone());
        }
        if prev_state == ThreadState::Dead {
            s.prev_dead = Some(prev.clone());
        }
        next.set_state(ThreadState::Running);
        s.current = Some(next.clone());
        s.switches += 1;
        s.quantum_left = QUANTUM_TICKS;
        prev_slot = prev.saved_rsp.get();
        // SAFETY: only the scheduler reads saved_rsp, with interrupts disabled.
        next_rsp = unsafe { *next.saved_rsp.get() };
        next_top = next.kstack_top;
        next_cr3 = next.process.as_ref().map(|p| p.cr3);
        // `prev`/`next`/`idle` Arcs are dropped here, before the switch, so a dying
        // thread does not leak references from its own (abandoned) stack frame.
    }
    if let Some(cr3) = next_cr3 {
        crate::mm::paging::activate_frame(cr3);
    }
    if next_top != 0 {
        arch::gdt::set_kernel_stack(next_top);
        arch::syscall::set_kernel_stack(next_top);
    }
    // SAFETY: both stacks were prepared by this module; interrupts are disabled.
    unsafe { context::switch_to(prev_slot, next_rsp) };
    finish_switch();
}

/// Reap the thread we switched away from if it died.
pub fn finish_switch() {
    let dead = SCHED.lock().prev_dead.take();
    drop(dead);
}

extern "C" fn user_thread_trampoline() -> ! {
    finish_switch();
    let (rip, rsp) = {
        let t = current();
        t.user_entry
    };
    // SAFETY: the entry point and stack were set up by `proc::spawn` in the address
    // space that `schedule()` just activated.
    unsafe { context::enter_user(rip, rsp) }
}

/// Mark the current thread dead and switch away for good.
pub fn exit_current_thread() -> ! {
    arch::disable_interrupts();
    {
        let mut s = SCHED.lock();
        s.current.as_ref().expect("no current").set_state(ThreadState::Dead);
        s.live_threads -= 1;
    }
    schedule();
    unreachable!("dead thread was scheduled again");
}

pub fn yield_now() {
    crate::sync::without_interrupts(schedule);
}

/// Wake a blocked thread (no-op for any other state).
pub fn wake(t: &Arc<Thread>) {
    let mut s = SCHED.lock();
    if t.state() == ThreadState::Blocked {
        t.set_state(ThreadState::Ready);
        s.run_queue.push_back(t.clone());
    }
}

pub fn sleep_ms(ms: u64) {
    crate::sync::without_interrupts(|| {
        {
            let mut s = SCHED.lock();
            let cur = s.current.clone().expect("no current");
            let wake_at = s.ticks.saturating_add(ms.saturating_mul(TICK_HZ as u64).div_ceil(1000).max(1));
            cur.set_state(ThreadState::Blocked);
            s.sleepers.push((wake_at, cur));
        }
        schedule();
    });
}

/// Called from the timer IRQ with interrupts disabled.
pub fn timer_tick() {
    let need_resched = {
        let mut s = SCHED.lock();
        s.ticks += 1;
        let now = s.ticks;
        let mut i = 0;
        while i < s.sleepers.len() {
            if s.sleepers[i].0 <= now {
                let (_, t) = s.sleepers.swap_remove(i);
                if t.state() == ThreadState::Blocked {
                    t.set_state(ThreadState::Ready);
                    s.run_queue.push_back(t);
                }
            } else {
                i += 1;
            }
        }
        let cur_is_idle = match (&s.current, &s.idle) {
            (Some(c), Some(i)) => Arc::ptr_eq(c, i),
            _ => true,
        };
        s.quantum_left = s.quantum_left.saturating_sub(1);
        !s.run_queue.is_empty() && (cur_is_idle || s.quantum_left == 0)
    };
    if need_resched {
        schedule();
    }
}

pub fn idle_loop() -> ! {
    loop {
        x86_64::instructions::interrupts::enable_and_hlt();
    }
}

/// A list of threads waiting for an event.
pub struct WaitQueue {
    waiters: SpinLock<VecDeque<Arc<Thread>>>,
}

impl WaitQueue {
    pub const fn new() -> Self {
        WaitQueue { waiters: SpinLock::new(VecDeque::new()) }
    }

    /// Block the current thread on this queue. Must be called with interrupts
    /// disabled; `release` runs after the thread is registered (drop your lock there).
    pub fn sleep_after(&self, release: impl FnOnce()) {
        let cur = current();
        cur.set_state(ThreadState::Blocked);
        self.waiters.lock().push_back(cur);
        release();
        schedule();
    }

    pub fn wake_all(&self) {
        let list: Vec<Arc<Thread>> = self.waiters.lock().drain(..).collect();
        for t in list {
            wake(&t);
        }
    }
}

impl Default for WaitQueue {
    fn default() -> Self {
        Self::new()
    }
}
