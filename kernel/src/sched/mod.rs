//! Preemptive scheduler for every CPU (ADR-0024), with service classes (ADR-0035).
//!
//! One run queue per service class serves all CPUs; each CPU has its own idle
//! thread, its own current thread and its own quantum. The boot CPU's PIT keeps the
//! time (ticks, sleepers); the other CPUs' local APIC timers only end quanta.
//!
//! The most urgent class with a thread ready goes first: interactive, then normal,
//! then background, round robin within a class. A thread queued in a more urgent
//! class than some CPU is running gets that CPU at once (a reschedule IPI), not at
//! the end of its quantum. A class whose threads have waited [`STARVE_TICKS`] while
//! more urgent ones ran gets the next pick, so nothing waits forever.
//!
//! Invariants:
//! * `schedule()` is entered and left with interrupts disabled;
//! * a thread that blocks registers itself somewhere (wait queue, sleepers) and sets
//!   its state to `Blocked` *before* calling `schedule()`, with interrupts disabled,
//!   so a wake-up cannot be lost;
//! * a thread is in the run queue only while no CPU is on its stack (`on_cpu` is
//!   false). The CPU that switches away from a thread puts it back once the switch
//!   is complete (`finish_switch`); a wake-up that finds the thread still on a CPU
//!   only marks it `Ready`, and that CPU queues it. So two CPUs never run on one
//!   stack, and no CPU ever waits for another to leave one;
//! * the thread that runs right after a switch reaps the previous thread if it died
//!   (`finish_switch`), so a dead thread's kernel stack is freed by someone else;
//! * a CPU loads the page tables of the thread it switches to every time, and the
//!   kernel's own when it goes idle. With one thread per process, that keeps every
//!   CPU's TLB free of mappings another CPU has removed (ADR-0024).

use alloc::collections::VecDeque;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};

use spaceabi::error::Error;

use crate::arch;
use crate::arch::context;
use crate::arch::percpu::{self, MAX_CPUS};
use crate::fpu::FpuArea;
use crate::mm::kstack::KernelStack;
use crate::proc::Process;
use crate::sync::SpinLock;

pub const TICK_HZ: u32 = 1000;
/// The boot CPU's quantum, in PIT ticks.
const QUANTUM_TICKS: u32 = 10;
/// An application processor's quantum, in its own (100 Hz) ticks: the same 10 ms.
const AP_QUANTUM_TICKS: u32 = 1;
/// Room made at boot in the run queue and the sleepers list, which grow only past
/// this many threads at once. Grown later, they would lift the kernel heap for good
/// at whatever moment the most threads first happened to wait at once -- on several
/// CPUs, any moment -- and a heap that never comes back to where it was reads as a
/// leak in the stability run (ADR-0019).
const QUEUE_ROOM: usize = 256;
/// Run queues, one per service class, most urgent first: `sched_class` 1, 2, 3.
const CLASSES: usize = 3;
/// What an idle CPU counts as: less urgent than any thread.
const IDLE_CLASS: u8 = CLASSES as u8;
/// How long a class may wait while more urgent ones run before it gets the next pick
/// (100 ms): enough for background work to move under a busy desktop, too rare to be
/// felt there.
const STARVE_TICKS: u64 = TICK_HZ as u64 / 10;

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
    /// A CPU is on this thread's stack: running it, or switching away from it.
    /// Changed only under the scheduler lock.
    on_cpu: AtomicBool,
    pub user_entry: (u64, u64),
    /// FP/SIMD registers while the thread is off the CPU (`crate::fpu`); none for
    /// idle threads, which run only kernel code.
    fpu: Option<FpuArea>,
    /// Its run queue: the process's service class, 0 for interactive
    /// ([`IDLE_CLASS`] for an idle thread).
    class: u8,
}

// SAFETY: `saved_rsp` is written by the CPU switching away from the thread and read
// by the one switching to it; `on_cpu` (under the scheduler lock) orders the two.
unsafe impl Sync for Thread {}
unsafe impl Send for Thread {}

static NEXT_TID: AtomicU64 = AtomicU64::new(1);

impl Thread {
    pub fn new_user(process: Arc<Process>, rip: u64, rsp: u64) -> Result<Arc<Thread>, Error> {
        let fpu = FpuArea::new()?;
        let ks = KernelStack::new()?;
        let top = ks.top;
        let class = (process.class.clamp(1, CLASSES as u32) - 1) as u8;
        // SAFETY: `top` is the top of a freshly mapped kernel stack.
        let rsp0 = unsafe { context::prepare_initial_stack(top, user_thread_trampoline) };
        Ok(Arc::new(Thread {
            tid: NEXT_TID.fetch_add(1, Ordering::Relaxed),
            process: Some(process),
            _kstack: Some(ks),
            kstack_top: top,
            saved_rsp: UnsafeCell::new(rsp0),
            state: AtomicU8::new(ThreadState::Ready as u8),
            on_cpu: AtomicBool::new(false),
            user_entry: (rip, rsp),
            fpu: Some(fpu),
            class,
        }))
    }

    /// The idle thread of a CPU: the context that is already running there.
    fn idle(stack_top: u64) -> Arc<Thread> {
        Arc::new(Thread {
            tid: 0,
            process: None,
            _kstack: None,
            kstack_top: stack_top,
            saved_rsp: UnsafeCell::new(0),
            state: AtomicU8::new(ThreadState::Running as u8),
            on_cpu: AtomicBool::new(true),
            user_entry: (0, 0),
            fpu: None,
            class: IDLE_CLASS,
        })
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

/// What one CPU keeps to itself. Only that CPU touches its slot, and only with
/// interrupts disabled.
struct CpuSlot {
    current: UnsafeCell<Option<Arc<Thread>>>,
    idle: UnsafeCell<Option<Arc<Thread>>>,
    /// The thread this CPU switched away from, until `finish_switch` has dealt with it.
    prev: UnsafeCell<Option<Arc<Thread>>>,
    quantum_left: UnsafeCell<u32>,
    /// The thread here got the CPU because its class had waited too long: it keeps
    /// it for its whole quantum, whatever more urgent work is queued meanwhile.
    starved_turn: UnsafeCell<bool>,
}

// SAFETY: see the type's documentation.
unsafe impl Sync for CpuSlot {}

impl CpuSlot {
    const fn new() -> Self {
        CpuSlot {
            current: UnsafeCell::new(None),
            idle: UnsafeCell::new(None),
            prev: UnsafeCell::new(None),
            quantum_left: UnsafeCell::new(QUANTUM_TICKS),
            starved_turn: UnsafeCell::new(false),
        }
    }
}

static CPUS: [CpuSlot; MAX_CPUS] = [const { CpuSlot::new() }; MAX_CPUS];

/// This CPU's slot. Callers keep interrupts disabled while they use it.
fn slot() -> &'static CpuSlot {
    &CPUS[percpu::index()]
}

struct Scheduler {
    run_queues: [VecDeque<Arc<Thread>>; CLASSES],
    /// Per class: the tick it last got a CPU, or its first thread arrived. A class
    /// with threads waiting [`STARVE_TICKS`] past it goes next.
    served_at: [u64; CLASSES],
    /// The last [`Scheduler::pick`] chose a class that had waited too long over a
    /// more urgent one.
    starved_pick: bool,
    sleepers: Vec<(u64, Arc<Thread>)>,
    switches: u64,
    live_threads: u64,
}

impl Scheduler {
    /// Queue a runnable thread behind the others of its class.
    fn queue(&mut self, t: Arc<Thread>) {
        let c = usize::from(t.class);
        if self.run_queues[c].is_empty() {
            self.served_at[c] = now_ticks();
        }
        self.run_queues[c].push_back(t);
    }

    /// The most urgent class with a thread queued.
    fn most_urgent(&self) -> Option<usize> {
        self.run_queues.iter().position(|q| !q.is_empty())
    }

    /// The next thread for a CPU whose current thread is still runnable in class
    /// `running` (`None` when it blocked, ended, or is the idle thread). `None` back
    /// means: keep running it.
    fn pick(&mut self, running: Option<usize>) -> Option<Arc<Thread>> {
        let queued = self.most_urgent()?;
        let urgent = running.map_or(queued, |r| r.min(queued));
        let now = now_ticks();
        // A less urgent class that has waited too long goes first; the least urgent
        // of those, since everything above it has had its turn.
        let class = (urgent + 1..CLASSES)
            .rev()
            .find(|&c| {
                !self.run_queues[c].is_empty() && now.saturating_sub(self.served_at[c]) >= STARVE_TICKS
            })
            .unwrap_or(urgent);
        self.starved_pick = class != urgent;
        // Round robin within the class; `None` when only the running thread is in it.
        let next = self.run_queues[class].pop_front();
        if next.is_some() || Some(class) == running {
            self.served_at[class] = now;
        }
        next
    }
}

static SCHED: SpinLock<Scheduler> = SpinLock::new(Scheduler {
    run_queues: [VecDeque::new(), VecDeque::new(), VecDeque::new()],
    served_at: [0; CLASSES],
    starved_pick: false,
    sleepers: Vec::new(),
    switches: 0,
    live_threads: 0,
});

/// Ticks since boot: as many as the clock's counter says went by (`crate::clock`), or
/// the boot CPU's tick interrupts counted where there is no counter.
static TICKS: AtomicU64 = AtomicU64::new(0);
/// Bit `i` set while CPU `i` runs its idle thread (changed under the scheduler lock).
static IDLE_CPUS: AtomicU64 = AtomicU64::new(0);
/// Bit `i` set once CPU `i` schedules.
static ONLINE_CPUS: AtomicU64 = AtomicU64::new(0);
/// The class of what each CPU runs ([`IDLE_CLASS`] while idle): where a more urgent
/// thread can take a CPU. Written under the scheduler lock, read without it.
static CPU_CLASS: [AtomicU8; MAX_CPUS] = [const { AtomicU8::new(IDLE_CLASS) }; MAX_CPUS];

fn quantum_for(cpu: usize) -> u32 {
    if cpu == 0 { QUANTUM_TICKS } else { AP_QUANTUM_TICKS }
}

/// Make the context running on this CPU its idle thread. `stack_top` is the top of
/// the guarded kernel stack it runs on.
fn adopt_idle(stack_top: u64) {
    let cpu = percpu::index();
    let idle = Thread::idle(stack_top);
    crate::sync::without_interrupts(|| {
        let s = slot();
        // SAFETY: this CPU's slot, interrupts disabled.
        unsafe {
            *s.current.get() = Some(idle.clone());
            *s.idle.get() = Some(idle);
            *s.quantum_left.get() = quantum_for(cpu);
        }
        let _g = SCHED.lock();
        IDLE_CPUS.fetch_or(1 << cpu, Ordering::Relaxed);
        ONLINE_CPUS.fetch_or(1 << cpu, Ordering::Relaxed);
        CPU_CLASS[cpu].store(IDLE_CLASS, Ordering::Relaxed);
    });
}

/// Turn the boot context into the boot CPU's idle thread.
pub fn init(idle_stack_top: u64) {
    {
        let mut s = SCHED.lock();
        // Without the room they grow on demand, as they always could.
        for q in s.run_queues.iter_mut() {
            let _ = q.try_reserve(QUEUE_ROOM);
        }
        let _ = s.sleepers.try_reserve(QUEUE_ROOM);
    }
    adopt_idle(idle_stack_top);
    println!(
        "[kernel] scheduler: {} Hz tick, {} ms quantum, {CLASSES} service classes (a class waits at most {} ms)",
        TICK_HZ,
        QUANTUM_TICKS * 1000 / TICK_HZ,
        STARVE_TICKS * 1000 / u64::from(TICK_HZ)
    );
}

/// An application processor joins: its starting context becomes its idle thread.
pub fn init_ap(idle_stack_top: u64) {
    adopt_idle(idle_stack_top);
}

/// CPUs that schedule.
pub fn online_cpus() -> u32 {
    ONLINE_CPUS.load(Ordering::Relaxed).count_ones()
}

/// A thread of `class` was just queued: get a CPU other than this one to take it.
/// An idle one if there is one; else the one running the least urgent thread, if
/// that is less urgent. (This CPU sees to itself at its next tick.)
fn kick_for(class: u8) {
    let me = percpu::index();
    let idle = IDLE_CPUS.load(Ordering::Relaxed) & !(1u64 << me);
    if idle != 0 {
        arch::smp::kick(idle.trailing_zeros() as usize);
        return;
    }
    let online = ONLINE_CPUS.load(Ordering::Relaxed) & !(1u64 << me);
    let mut target: Option<(usize, u8)> = None;
    for cpu in (0..MAX_CPUS).filter(|&c| online & (1u64 << c) != 0) {
        let running = CPU_CLASS[cpu].load(Ordering::Relaxed);
        if running > class && target.is_none_or(|(_, t)| running > t) {
            target = Some((cpu, running));
        }
    }
    if let Some((cpu, _)) = target {
        arch::smp::kick(cpu);
    }
}

pub fn add(thread: Arc<Thread>) {
    let class = thread.class;
    {
        let mut s = SCHED.lock();
        thread.set_state(ThreadState::Ready);
        s.queue(thread);
        s.live_threads += 1;
    }
    kick_for(class);
}

pub fn current() -> Arc<Thread> {
    // SAFETY: this CPU's slot, read with interrupts disabled.
    crate::sync::without_interrupts(|| unsafe { (*slot().current.get()).clone() })
        .expect("scheduler not initialised")
}

/// Name of the current thread (panic path: takes no lock).
pub fn try_current_name() -> Option<String> {
    // SAFETY: this CPU's slot; the panic path runs with interrupts disabled.
    let cur = unsafe { (*slot().current.get()).clone() };
    cur.map(|t| t.name())
}

pub fn uptime_ms() -> u64 {
    TICKS.load(Ordering::Relaxed) * 1000 / TICK_HZ as u64
}

/// `(live threads, context switches)`.
pub fn stats() -> (u64, u64) {
    let s = SCHED.lock();
    (s.live_threads, s.switches)
}

/// Pick the next runnable thread and switch to it. Interrupts must be disabled.
pub fn schedule() {
    debug_assert!(!arch::interrupts_enabled(), "schedule() with interrupts enabled");
    let cpu = percpu::index();
    let me = slot();
    let prev_slot: *mut u64;
    let next_rsp: u64;
    let next_top: u64;
    let next_cr3;
    let prev_fpu: Option<*const FpuArea>;
    let next_fpu: Option<*const FpuArea>;
    {
        // SAFETY: this CPU's slot, interrupts disabled.
        let prev = unsafe { (*me.current.get()).clone() }.expect("no current thread");
        let idle = unsafe { (*me.idle.get()).clone() }.expect("no idle thread");
        let prev_is_idle = Arc::ptr_eq(&prev, &idle);
        let mut s = SCHED.lock();
        let prev_state = prev.state();
        let prev_runnable = matches!(prev_state, ThreadState::Running | ThreadState::Ready);
        let running = (prev_runnable && !prev_is_idle).then_some(usize::from(prev.class));
        let next = match s.pick(running) {
            Some(t) => t,
            None if prev_runnable => {
                // Nothing else to run, or nothing as urgent: keep going, for a new
                // quantum (a thread woken while it was about to block is simply
                // running again).
                prev.set_state(ThreadState::Running);
                // SAFETY: this CPU's slot, interrupts disabled.
                unsafe {
                    *me.quantum_left.get() = quantum_for(cpu);
                    *me.starved_turn.get() = false;
                }
                return;
            }
            None => idle.clone(),
        };
        let starved = s.starved_pick && !Arc::ptr_eq(&next, &idle);
        if prev_runnable && !prev_is_idle {
            // Queued again by `finish_switch`, once this CPU is off its stack.
            prev.set_state(ThreadState::Ready);
        }
        next.set_state(ThreadState::Running);
        next.on_cpu.store(true, Ordering::Relaxed);
        if Arc::ptr_eq(&next, &idle) {
            IDLE_CPUS.fetch_or(1 << cpu, Ordering::Relaxed);
        } else {
            IDLE_CPUS.fetch_and(!(1 << cpu), Ordering::Relaxed);
        }
        CPU_CLASS[cpu].store(next.class, Ordering::Relaxed);
        s.switches += 1;
        drop(s);
        prev_slot = prev.saved_rsp.get();
        // SAFETY: `next` came off the run queue, so no CPU is on its stack and its
        // saved `rsp` was written before it was queued.
        next_rsp = unsafe { *next.saved_rsp.get() };
        next_top = next.kstack_top;
        next_cr3 = next.process.as_ref().map(|p| p.cr3);
        prev_fpu = prev.fpu.as_ref().map(|a| a as *const FpuArea);
        next_fpu = next.fpu.as_ref().map(|a| a as *const FpuArea);
        // SAFETY: this CPU's slot, interrupts disabled.
        unsafe {
            *me.quantum_left.get() = quantum_for(cpu);
            *me.starved_turn.get() = starved;
            *me.prev.get() = Some(prev);
            *me.current.get() = Some(next);
        }
        // `idle` is dropped here; the slot keeps every thread involved alive, so no
        // reference dies on a stack that is about to be abandoned.
    }
    match next_cr3 {
        Some(cr3) => crate::mm::paging::load_cr3(cr3),
        None => crate::mm::paging::activate_kernel(),
    }
    if next_top != 0 {
        arch::set_kernel_stack(next_top);
    }
    // The leaving thread's FP/SIMD registers to its area, the next one's back
    // (ADR-0031). The kernel uses none of them between here and user mode, so the
    // next thread's are loaded before its stack is.
    // SAFETY: both threads are kept alive by this CPU's slot; this CPU is leaving
    // `prev` (it is queued again only after the switch) and `next` is on no CPU.
    unsafe {
        if let Some(a) = prev_fpu {
            (*a).save();
        }
        if let Some(a) = next_fpu {
            (*a).restore();
        }
    }
    // SAFETY: both stacks were prepared by this module; interrupts are disabled.
    unsafe { context::switch_to(prev_slot, next_rsp) };
    finish_switch();
}

/// Deal with the thread this CPU just switched away from: queue it again if it is
/// still runnable, reap it if it died.
pub fn finish_switch() {
    // SAFETY: this CPU's slot, interrupts disabled (right after a switch).
    let Some(prev) = (unsafe { (*slot().prev.get()).take() }) else { return };
    let mut queued = None;
    let left = {
        let mut s = SCHED.lock();
        prev.on_cpu.store(false, Ordering::Relaxed);
        match prev.state() {
            // Preempted, or woken while this CPU was leaving it. An idle thread is
            // never `Ready`, so it never ends up here.
            ThreadState::Ready => {
                queued = Some(prev.class);
                s.queue(prev);
                None
            }
            // Dropped outside the lock: this may be the last reference, and freeing
            // a kernel stack takes other locks.
            _ => Some(prev),
        }
    };
    if let Some(t) = left {
        // A dead thread's process may have its exit to tell; the stack goes first
        // (this is its last reference), then the news.
        let proc = if t.state() == ThreadState::Dead { t.process.clone() } else { None };
        drop(t);
        if let Some(p) = proc {
            crate::proc::reaped(&p);
        }
    }
    if let Some(class) = queued {
        kick_for(class);
    }
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
        let cur = current();
        let mut s = SCHED.lock();
        cur.set_state(ThreadState::Dead);
        s.live_threads -= 1;
    }
    schedule();
    unreachable!("dead thread was scheduled again");
}

pub fn yield_now() {
    crate::sync::without_interrupts(schedule);
}

/// Sleep: the caller has registered the current thread wherever its wake-up comes
/// from and set it `Blocked`. Unless a kill is already pending -- a kill that came
/// while the thread was still running found nothing to wake, and this is where it
/// is noticed instead of never. Interrupts must be disabled.
pub fn block() {
    // The state store above must be visible before the kill flag is read, and the
    // killer publishes its flag before it reads the state (`proc::kill`): one of the
    // two always sees the other.
    core::sync::atomic::fence(Ordering::SeqCst);
    if crate::proc::has_pending_kill() {
        current().set_state(ThreadState::Running);
        return;
    }
    schedule();
}

/// Make a blocked thread runnable (no-op for any other state). Under the scheduler
/// lock; returns the class it was queued in, for the caller to find it a CPU.
fn make_ready(s: &mut Scheduler, t: &Arc<Thread>) -> Option<u8> {
    if t.state() != ThreadState::Blocked {
        return None;
    }
    t.set_state(ThreadState::Ready);
    if t.on_cpu.load(Ordering::Relaxed) {
        // Still on the CPU it was blocking on: that CPU queues it (or keeps running it).
        return None;
    }
    s.queue(t.clone());
    Some(t.class)
}

/// Wake a blocked thread (no-op for any other state).
pub fn wake(t: &Arc<Thread>) {
    let queued = {
        let mut s = SCHED.lock();
        // A woken thread is no longer a sleeper: dropping the entry here releases the
        // reference immediately instead of at `wake_at` (which a long sleep puts far
        // in the future, keeping a killed thread's kernel stack and process alive).
        s.sleepers.retain(|(_, st)| !Arc::ptr_eq(st, t));
        make_ready(&mut s, t)
    };
    if let Some(class) = queued {
        kick_for(class);
    }
}

fn deadline_from(now: u64, ms: u64) -> u64 {
    now.saturating_add(ms.saturating_mul(TICK_HZ as u64).div_ceil(1000).max(1))
}

pub fn sleep_ms(ms: u64) -> Result<(), Error> {
    crate::sync::without_interrupts(|| {
        let cur = current();
        {
            let mut s = SCHED.lock();
            // Bookkeeping must not be able to exhaust the heap on a blocking path.
            if s.sleepers.try_reserve(1).is_err() {
                return Err(Error::NoMemory);
            }
            let wake_at = deadline_from(TICKS.load(Ordering::Relaxed), ms);
            cur.set_state(ThreadState::Blocked);
            s.sleepers.push((wake_at, cur.clone()));
        }
        block();
        // Woken normally (timer) or by a kill: make sure no sleeper entry survives.
        SCHED.lock().sleepers.retain(|(_, st)| !Arc::ptr_eq(st, &cur));
        Ok(())
    })
}

/// Tick at which a wait of `ms` milliseconds from now ends (at least one tick away).
pub fn deadline_after_ms(ms: u64) -> u64 {
    deadline_from(TICKS.load(Ordering::Relaxed), ms)
}

/// Ticks since boot.
pub fn now_ticks() -> u64 {
    TICKS.load(Ordering::Relaxed)
}

/// Arrange for `t` to be woken at tick `deadline` without blocking it (the caller
/// blocks it and schedules). Interrupts must be disabled.
pub fn add_sleeper(deadline: u64, t: &Arc<Thread>) -> Result<(), Error> {
    let mut s = SCHED.lock();
    if s.sleepers.try_reserve(1).is_err() {
        return Err(Error::NoMemory);
    }
    s.sleepers.push((deadline, t.clone()));
    Ok(())
}

/// Drop any timed wake-up registered for `t`.
pub fn remove_sleeper(t: &Arc<Thread>) {
    let gone: Vec<(u64, Arc<Thread>)> = {
        let mut s = SCHED.lock();
        let mut gone = Vec::new();
        let mut i = 0;
        while i < s.sleepers.len() {
            if Arc::ptr_eq(&s.sleepers[i].1, t) {
                let e = s.sleepers.swap_remove(i);
                // A failed reservation just means the entry is dropped under the lock.
                if gone.try_reserve(1).is_ok() {
                    gone.push(e);
                }
            } else {
                i += 1;
            }
        }
        gone
    };
    drop(gone);
}

/// The boot CPU's tick, under the scheduler lock: the tick count catches up with the
/// clock's counter -- ticks that came late or merged cost no time -- and never goes
/// back. Without a counter, one more tick.
fn advance_ticks() -> u64 {
    match crate::clock::ticks(u64::from(TICK_HZ)) {
        Some(by_counter) => {
            let now = by_counter.max(TICKS.load(Ordering::Relaxed));
            TICKS.store(now, Ordering::Relaxed);
            now
        }
        None => TICKS.fetch_add(1, Ordering::Relaxed) + 1,
    }
}

/// Called from a timer interrupt with interrupts disabled: the PIT on the boot CPU,
/// the local APIC timer on the others.
pub fn timer_tick() {
    let cpu = percpu::index();
    // The most urgent class a sleeper was woken into.
    let mut woke: Option<u8> = None;
    let mut expired_refs: Vec<Arc<Thread>> = Vec::new();
    let need_resched = {
        let mut s = SCHED.lock();
        if cpu == 0 {
            let now = advance_ticks();
            let mut i = 0;
            while i < s.sleepers.len() {
                if s.sleepers[i].0 <= now {
                    let (_, t) = s.sleepers.swap_remove(i);
                    if let Some(c) = make_ready(&mut s, &t) {
                        woke = Some(woke.map_or(c, |w| w.min(c)));
                    }
                    if expired_refs.try_reserve(1).is_ok() {
                        expired_refs.push(t);
                    }
                } else {
                    i += 1;
                }
            }
        }
        let me = slot();
        // SAFETY: this CPU's slot, interrupts disabled.
        let (cur_is_idle, cur_class, quantum_left, starved_turn) = unsafe {
            let q = &mut *me.quantum_left.get();
            *q = q.saturating_sub(1);
            let (idle, class) = match (&*me.current.get(), &*me.idle.get()) {
                (Some(c), Some(i)) => (Arc::ptr_eq(c, i), usize::from(c.class)),
                _ => (true, usize::from(IDLE_CLASS)),
            };
            (idle, class, *q, *me.starved_turn.get())
        };
        // Something queued, and: this CPU is idle, its quantum is over, or the queued
        // thread is more urgent than the one it runs (unless that one is having the
        // turn its class waited for).
        s.most_urgent().is_some_and(|c| cur_is_idle || quantum_left == 0 || (c < cur_class && !starved_turn))
    };
    drop(expired_refs);
    if let Some(class) = woke {
        kick_for(class);
    }
    if need_resched {
        schedule();
    }
}

/// Another CPU queued work: this one is idle, or runs something less urgent.
pub fn reschedule_ipi() {
    // SAFETY: this CPU's slot, interrupts disabled (interrupt handler).
    let (idle, class, starved_turn) = unsafe {
        let me = slot();
        let starved_turn = *me.starved_turn.get();
        match (&*me.current.get(), &*me.idle.get()) {
            (Some(c), Some(i)) => (Arc::ptr_eq(c, i), usize::from(c.class), starved_turn),
            _ => (false, usize::from(IDLE_CLASS), starved_turn),
        }
    };
    let preempt = !idle && !starved_turn && SCHED.lock().most_urgent().is_some_and(|c| c < class);
    if idle || preempt {
        schedule();
    }
}

pub fn idle_loop() -> ! {
    loop {
        // Whatever became runnable while this CPU was on its way here is taken now;
        // after that, only an interrupt (tick or reschedule IPI) brings work.
        arch::disable_interrupts();
        schedule();
        arch::enable_interrupts_and_wait();
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
    /// disabled; `release` runs after the thread is registered (drop your lock there)
    /// and is always called, including on the error path.
    ///
    /// On return the caller is no longer referenced by this queue: a thread woken by
    /// `wake_all` was already drained, and one woken by a kill removes itself here.
    /// Without that, killing a thread blocked on a queue whose event never happens
    /// (a `wait` on a process that keeps running) would pin its kernel stack and its
    /// address space forever.
    pub fn sleep_after(&self, release: impl FnOnce()) -> Result<(), Error> {
        let cur = current();
        {
            let mut w = self.waiters.lock();
            if w.try_reserve(1).is_err() {
                drop(w);
                release();
                return Err(Error::NoMemory);
            }
            cur.set_state(ThreadState::Blocked);
            w.push_back(cur.clone());
        }
        release();
        block();
        self.unregister(&cur);
        Ok(())
    }

    /// Add `t` to this queue without blocking it: for a thread about to wait on
    /// several queues at once (`SYS_WAIT_ANY`). The caller blocks and schedules, and
    /// must [`unregister`](Self::unregister) from every queue once it runs again.
    pub fn register(&self, t: &Arc<Thread>) -> Result<(), Error> {
        let mut w = self.waiters.lock();
        if w.try_reserve(1).is_err() {
            return Err(Error::NoMemory);
        }
        w.push_back(t.clone());
        Ok(())
    }

    pub fn unregister(&self, t: &Arc<Thread>) {
        self.waiters.lock().retain(|x| !Arc::ptr_eq(x, t));
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
