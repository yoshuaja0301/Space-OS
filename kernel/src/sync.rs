//! Kernel synchronisation primitives.
//!
//! Space OS currently runs one CPU, so a spinlock is really an "interrupts off"
//! section with a re-entrancy check: acquiring a lock that is already held can only
//! be a bug (there is nobody else to release it), and we panic instead of hanging.

use core::cell::UnsafeCell;
use core::ops::{Deref, DerefMut};
use core::sync::atomic::{AtomicBool, Ordering};

use x86_64::instructions::interrupts;

pub struct SpinLock<T> {
    locked: AtomicBool,
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
        SpinLock { locked: AtomicBool::new(false), data: UnsafeCell::new(data) }
    }

    pub fn lock(&self) -> SpinLockGuard<'_, T> {
        let reenable_irq = interrupts::are_enabled();
        interrupts::disable();
        let mut spins: u32 = 0;
        while self.locked.compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed).is_err() {
            spins += 1;
            if spins > 10_000_000 {
                panic!("spinlock deadlock (re-entrant lock on a single CPU)");
            }
            core::hint::spin_loop();
        }
        SpinLockGuard { lock: self, reenable_irq }
    }

    /// Acquire only if free (used on the panic path to avoid self-deadlock).
    pub fn try_lock(&self) -> Option<SpinLockGuard<'_, T>> {
        let reenable_irq = interrupts::are_enabled();
        interrupts::disable();
        if self.locked.compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed).is_ok() {
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
        self.lock.locked.store(false, Ordering::Release);
        if self.reenable_irq {
            interrupts::enable();
        }
    }
}

/// A `static` cell for data that is initialised once at boot by the single CPU and
/// then only read (GDT, IDT, TSS, ISR tables).
pub struct StaticCell<T>(UnsafeCell<T>);

// SAFETY: the kernel only mutates the cell during single-threaded early boot.
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
