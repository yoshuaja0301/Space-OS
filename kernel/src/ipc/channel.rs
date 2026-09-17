//! Bidirectional message channels with bounded queues and handle transfer.
//!
//! A channel has two sides; each side owns a queue of messages *sent to it*, a wait
//! queue of blocked receivers and an `open` flag. An [`Endpoint`] is a reference to
//! one side; when the last endpoint of a side is dropped the side closes and the
//! peer's receivers are woken so they observe `PeerClosed`.

use alloc::collections::VecDeque;
use alloc::sync::Arc;
use alloc::vec::Vec;

use spaceabi::error::Error;
use spaceabi::syscall::MSG_MAX;

use crate::proc::handles::HandleEntry;
use crate::sched::WaitQueue;
use crate::sync::SpinLock;

pub const MAX_QUEUE: usize = 64;

pub struct Message {
    pub data: Vec<u8>,
    pub handle: Option<HandleEntry>,
}

struct Side {
    queue: VecDeque<Message>,
    open: bool,
}

struct Channel {
    sides: SpinLock<[Side; 2]>,
    waiters: [WaitQueue; 2],
}

pub struct Endpoint {
    chan: Arc<Channel>,
    side: usize,
}

impl Endpoint {
    /// Create a connected pair of endpoints.
    pub fn pair() -> (Arc<Endpoint>, Arc<Endpoint>) {
        let chan = Arc::new(Channel {
            sides: SpinLock::new([
                Side { queue: VecDeque::new(), open: true },
                Side { queue: VecDeque::new(), open: true },
            ]),
            waiters: [WaitQueue::new(), WaitQueue::new()],
        });
        (Arc::new(Endpoint { chan: chan.clone(), side: 0 }), Arc::new(Endpoint { chan, side: 1 }))
    }

    /// Put a message back at the head of our own queue.
    ///
    /// Used when delivery to the receiving process fails after the message was
    /// dequeued (a full handle table): without this the payload and any capability
    /// inside it would be destroyed by an error the receiver can retry.
    pub fn requeue(&self, msg: Message) {
        self.chan.sides.lock()[self.side].queue.push_front(msg);
    }

    /// True when both endpoints belong to the same channel (either side).
    pub fn same_channel(&self, other: &Endpoint) -> bool {
        Arc::ptr_eq(&self.chan, &other.chan)
    }

    /// Queue a message for the peer. Never blocks: a full queue yields `WouldBlock`.
    pub fn send(&self, msg: Message) -> Result<(), Error> {
        if msg.data.len() > MSG_MAX {
            return Err(Error::MsgSize);
        }
        let peer = 1 - self.side;
        {
            let mut sides = self.chan.sides.lock();
            if !sides[peer].open {
                return Err(Error::PeerClosed);
            }
            if sides[peer].queue.len() >= MAX_QUEUE {
                return Err(Error::WouldBlock);
            }
            sides[peer].queue.try_reserve(1).map_err(|_| Error::NoMemory)?;
            sides[peer].queue.push_back(msg);
        }
        self.chan.waiters[peer].wake_all();
        Ok(())
    }

    /// Dequeue the next message addressed to this side.
    ///
    /// `cap` is the receiver's buffer size: a message that does not fit stays queued
    /// and `MsgSize` is returned. Blocks unless `nonblock`.
    pub fn recv(&self, cap: usize, nonblock: bool) -> Result<Message, Error> {
        crate::sync::without_interrupts(|| {
            loop {
                let mut sides = self.chan.sides.lock();
                let me = &mut sides[self.side];
                if let Some(front) = me.queue.front() {
                    if front.data.len() > cap {
                        return Err(Error::MsgSize);
                    }
                    return Ok(me.queue.pop_front().expect("front exists"));
                }
                if !sides[1 - self.side].open {
                    return Err(Error::PeerClosed);
                }
                if nonblock {
                    return Err(Error::WouldBlock);
                }
                // Register as a waiter, release the channel lock, then sleep.
                self.chan.waiters[self.side].sleep_after(move || drop(sides))?;
                if crate::proc::has_pending_kill() {
                    return Err(Error::Interrupted);
                }
            }
        })
    }
}

impl Drop for Endpoint {
    fn drop(&mut self) {
        let drained: Vec<Message> = {
            let mut sides = self.chan.sides.lock();
            sides[self.side].open = false;
            let mut d: Vec<Message> = sides[self.side].queue.drain(..).collect();
            if !sides[1 - self.side].open {
                // Nobody can receive on either side any more: release everything
                // still queued so objects inside messages (handles) do not leak.
                d.extend(sides[1 - self.side].queue.drain(..));
            }
            d
        };
        drop(drained); // may release transferred handles
        // Wake the peer so it observes PeerClosed, and drain our own side's waiter
        // list so no stale thread references outlive the endpoint.
        self.chan.waiters[1 - self.side].wake_all();
        self.chan.waiters[self.side].wake_all();
    }
}
