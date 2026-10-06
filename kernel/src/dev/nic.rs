//! The network card, whichever driver runs it.
//!
//! virtio-net when the machine has one, otherwise a card of Intel's e1000 family
//! (ADR-0026). The kernel moves Ethernet frames and nothing else (ADR-0016), and
//! one lease on the card exists at a time: whoever holds it receives every frame
//! and may send any, which is why leasing needs its own root right (`NET`). Both
//! drivers poll; a frame that has arrived wakes the lease holder's waiters on the
//! next timer tick.

use alloc::string::String;
use alloc::sync::Arc;
use core::sync::atomic::{AtomicBool, AtomicU8, Ordering};

use spaceabi::error::Error;
use spaceabi::syscall::{FRAME_MAX, NetInfo};

use super::{e1000, virtio_net};
use crate::sched::WaitQueue;

const NONE: u8 = 0;
const VIRTIO: u8 = 1;
const E1000: u8 = 2;

/// Which driver has the card.
static CARD: AtomicU8 = AtomicU8::new(NONE);
/// Threads waiting for a frame (`SYS_WAIT_ANY` on a lease).
static WAITERS: WaitQueue = WaitQueue::new();
/// Set while a lease exists.
static LEASED: AtomicBool = AtomicBool::new(false);

/// Bring up the card: virtio-net if there is one, else an e1000.
pub fn init() {
    virtio_net::init();
    if virtio_net::present() {
        CARD.store(VIRTIO, Ordering::Relaxed);
        return;
    }
    e1000::init();
    if e1000::present() {
        CARD.store(E1000, Ordering::Relaxed);
    }
}

pub fn fmt_mac(m: &[u8; 6]) -> String {
    alloc::format!("{:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}", m[0], m[1], m[2], m[3], m[4], m[5])
}

/// True when a card is present and usable.
pub fn present() -> bool {
    CARD.load(Ordering::Relaxed) != NONE
}

/// A lease on the card. Dropping the last reference releases it.
pub struct NicLease {
    _private: (),
}

impl Drop for NicLease {
    fn drop(&mut self) {
        LEASED.store(false, Ordering::Release);
    }
}

/// Take the card. Frames that arrived for a previous holder are discarded: a new
/// owner must not read traffic that was addressed to the old one's conversations.
pub fn lease() -> Result<Arc<NicLease>, Error> {
    if !present() {
        return Err(Error::NotFound);
    }
    if LEASED.swap(true, Ordering::AcqRel) {
        return Err(Error::Busy);
    }
    match CARD.load(Ordering::Relaxed) {
        VIRTIO => virtio_net::drain(),
        _ => e1000::drain(),
    }
    Ok(Arc::new(NicLease { _private: () }))
}

pub fn info() -> Result<NetInfo, Error> {
    match CARD.load(Ordering::Relaxed) {
        VIRTIO => virtio_net::info(),
        E1000 => e1000::info(),
        _ => Err(Error::NotFound),
    }
}

/// Queue one frame for transmission. `WouldBlock` when every transmit buffer is
/// still the card's.
pub fn send(frame: &[u8]) -> Result<(), Error> {
    match CARD.load(Ordering::Relaxed) {
        VIRTIO => virtio_net::send(frame),
        E1000 => e1000::send(frame),
        _ => Err(Error::NotFound),
    }
}

/// Take one received frame; `WouldBlock` when nothing is waiting.
pub fn recv(out: &mut [u8; FRAME_MAX]) -> Result<usize, Error> {
    match CARD.load(Ordering::Relaxed) {
        VIRTIO => virtio_net::recv(out),
        E1000 => e1000::recv(out),
        _ => Err(Error::NotFound),
    }
}

/// True when a received frame is waiting.
pub fn rx_ready() -> bool {
    match CARD.load(Ordering::Relaxed) {
        VIRTIO => virtio_net::rx_ready(),
        E1000 => e1000::rx_ready(),
        _ => false,
    }
}

/// Queue of threads waiting for a frame.
pub fn waiters() -> &'static WaitQueue {
    &WAITERS
}

/// Timer-tick hook (interrupts disabled): wake frame waiters when one has arrived.
pub fn poll_tick() {
    let ready = match CARD.load(Ordering::Relaxed) {
        VIRTIO => virtio_net::rx_ready_nowait(),
        E1000 => e1000::rx_ready_nowait(),
        _ => false,
    };
    if ready {
        WAITERS.wake_all();
    }
}
