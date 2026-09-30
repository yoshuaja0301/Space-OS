//! Kernel synchronisation primitives.
//!
//! A spinlock disables interrupts on the CPU that holds it, so an interrupt handler
//! on that CPU can never find it taken by the code it interrupted. Each lock knows
//! which CPU holds it: that CPU asking again can only be a bug -- nobody else would
//! ever release it -- and panics at once instead of hanging. Another CPU holding it
//! is ordinary, and waiting for it is what a spinlock is for; only a wait of many
//! seconds is taken for a deadlock and reported.

use core::cell::UnsafeCell;
use core::ops::{Deref, DerefMut};
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use x86_64::instructions::interrupts;

/// Spins before a wait on another CPU counts as a deadlock (several seconds).
const SPINS_MAX: u64 = 1 << 32;

pub struct SpinLock<T> {
    locked: AtomicBool,
    /// Index + 1 of the CPU holding the lock, 0 when free.
    owner: AtomicU32,
    data: UnsafeCell<T>,
}

// SAFETY: access to `data` is serialised by `locked` (and interrupts are disabled while held).
unsafe impl<T: Send> Sync for SpinLock<T> {}
unsafe impl<T: Send> Send for SpinLock<T> {}

pub struct SpinLockGuard<'a, T> {
    lock: &'a SpinLock<T>,
    reenable_irq: bool,
}

impl<T> SpinLock<T> {
    pub const fn new(data: T) -> Self {
        SpinLock { locked: AtomicBool::new(false), owner: AtomicU32::new(0), data: UnsafeCell::new(data) }
    }

    pub fn lock(&self) -> SpinLockGuard<'_, T> {
        let reenable_irq = interrupts::are_enabled();
        interrupts::disable();
        let me = crate::arch::percpu::index() as u32 + 1;
        if self.owner.load(Ordering::Relaxed) == me {
            // Only this CPU writes its own number here, and it clears it before
            // letting go: the lock is held by the code this CPU is already in.
            panic!("spinlock taken again by the CPU that holds it (cpu {})", me - 1);
        }
        let mut spins: u64 = 0;
        while self.locked.compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed).is_err() {
            spins += 1;
            if spins == SPINS_MAX {
                let holder = self.owner.load(Ordering::Relaxed);
                panic!("spinlock deadlock: cpu {} waited seconds for cpu {}", me - 1, holder.wrapping_sub(1));
            }
            core::hint::spin_loop();
        }
        self.owner.store(me, Ordering::Relaxed);
        SpinLockGuard { lock: self, reenable_irq }
    }

    /// Acquire only if free (used on the panic path to avoid self-deadlock).
    pub fn try_lock(&self) -> Option<SpinLockGuard<'_, T>> {
        let reenable_irq = interrupts::are_enabled();
        interrupts::disable();
        if self.locked.compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed).is_ok() {
            self.owner.store(crate::arch::percpu::index() as u32 + 1, Ordering::Relaxed);
            Some(SpinLockGuard { lock: self, reenable_irq })
        } else {
            if reenable_irq {
                interrupts::enable();
            }
            None
        }
    }

    /// Break a lock during a panic so the panic handler can print.
    ///
    /// # Safety
    /// Only from the panic path, when no other code will use the guard again.
    pub unsafe fn force_unlock(&self) {
        self.owner.store(0, Ordering::Relaxed);
        self.locked.store(false, Ordering::Release);
    }
}

impl<T> Deref for SpinLockGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        // SAFETY: guard holds the lock.
        unsafe { &*self.lock.data.get() }
    }
}

impl<T> DerefMut for SpinLockGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        // SAFETY: guard holds the lock exclusively.
        unsafe { &mut *self.lock.data.get() }
    }
}

impl<T> Drop for SpinLockGuard<'_, T> {
    fn drop(&mut self) {
        self.lock.owner.store(0, Ordering::Relaxed);
        self.lock.locked.store(false, Ordering::Release);
        if self.reenable_irq {
            interrupts::enable();
        }
    }
}

/// A `static` cell for data that is set up before anything could race on it (the
/// boot CPU during early boot, or a CPU's own per-CPU data) and otherwise only read
/// (GDT, IDT, TSS, ISR tables).
pub struct StaticCell<T>(UnsafeCell<T>);

// SAFETY: every writer documents why nothing else can touch the cell at that time.
unsafe impl<T> Sync for StaticCell<T> {}

impl<T> StaticCell<T> {
    pub const fn new(v: T) -> Self {
        StaticCell(UnsafeCell::new(v))
    }

    pub fn get(&self) -> *mut T {
        self.0.get()
    }

    /// # Safety
    /// Caller guarantees no concurrent access (early boot or interrupts disabled).
    #[allow(clippy::mut_from_ref)]
    pub unsafe fn get_mut(&self) -> &mut T {
        // SAFETY: see above.
        unsafe { &mut *self.0.get() }
    }
}

/// Run `f` with interrupts disabled, restoring the previous state afterwards.
pub fn without_interrupts<R>(f: impl FnOnce() -> R) -> R {
    let was = interrupts::are_enabled();
    interrupts::disable();
    let r = f();
    if was {
        interrupts::enable();
    }
    r
}
