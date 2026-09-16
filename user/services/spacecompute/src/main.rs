//! `spacecompute` – the Space Compute service (CPU backend), ABI version 0.
//!
//! A user-space service, as the PRD requires: the kernel knows about memory
//! objects and channels, not about tensors. Clients negotiate the ABI version,
//! allocate buffers (memory objects both sides map), create a queue, submit
//! operations and wait for tickets with a deadline or cancel them.
//!
//! Work runs in bounded steps between deadline checks, so `WAIT` really can time
//! out and `CANCEL` really can stop a running ticket - the contract tests depend on
//! both being observable, not merely declared.
#![no_std]
#![no_main]

extern crate alloc;

use alloc::vec::Vec;

use libspace::spaceabi::compute::{
    ABI_VERSION, BufferRef, MAX_BUFFER_BYTES, MAX_BUFFERS, MAX_QUEUES, MAX_TICKETS, Op, Request, Response,
    backend, op, req, state,
};
use libspace::spaceabi::error::Error;
use libspace::spaceabi::math::{cos_f32, exp_f32, powf_f32, silu_f32, sin_f32, sqrt_f32};
use libspace::{Handle, handle, println, sys};

/// Rotary embedding base, pinned so the guest and the baseline tool agree.
const ROPE_THETA: f32 = 10000.0;

/// Work units executed between two deadline checks.
const STEP_UNITS: u64 = 250_000;

struct Buf {
    handle: Handle,
    ptr: *mut u8,
    len: usize,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum TicketState {
    Pending,
    Done,
    Cancelled,
}

struct Ticket {
    id: u32,
    queue: u32,
    op: Op,
    state: TicketState,
    progress: u64,
    value: u64,
    /// Non-zero when the work itself failed; reported to the client with the ticket.
    status: i32,
}

struct Service {
    channel: Handle,
    hello: bool,
    buffers: Vec<Option<Buf>>,
    queues: Vec<bool>,
    tickets: Vec<Ticket>,
    next_ticket: u32,
}

fn status_of(e: Error) -> i32 {
    -(e as u32 as i32)
}

/// A dimension that indexes element 0 must not be zero: every kernel below reads
/// `a[0]`, and a zero-length slice would turn a client's request into a panic.
fn elems(d: u32) -> Result<usize, Error> {
    if d == 0 { Err(Error::Invalid) } else { Ok(d as usize) }
}

/// Product of two dimensions, refusing anything that would wrap.
fn area(a: usize, b: usize) -> Result<usize, Error> {
    a.checked_mul(b).filter(|v| *v <= u32::MAX as usize).ok_or(Error::Invalid)
}

fn as_bytes<T>(v: &T) -> &[u8] {
    // SAFETY: `T` is a `repr(C)` plain-data message.
    unsafe { core::slice::from_raw_parts(v as *const T as *const u8, core::mem::size_of::<T>()) }
}

impl Service {
    fn new(channel: Handle) -> Self {
        let mut buffers = Vec::new();
        buffers.resize_with(MAX_BUFFERS, || None);
        Service {
            channel,
            hello: false,
            buffers,
            queues: alloc::vec![false; MAX_QUEUES],
            tickets: Vec::new(),
            next_ticket: 1,
        }
    }

    fn reply(&self, r: Response) {
        let _ = sys::send(self.channel, as_bytes(&r), None);
    }

    fn reply_err(&self, e: Error) {
        self.reply(Response { status: status_of(e), ..Default::default() });
    }

    /// Resolve a buffer reference to a slice of `f32`, checking every bound.
    fn slice(&self, r: &BufferRef, need_floats: usize) -> Result<&'static mut [f32], Error> {
        if r.is_none() {
            return Err(Error::Invalid);
        }
        let buf = self.buffers.get(r.id as usize).and_then(|b| b.as_ref()).ok_or(Error::BadHandle)?;
        let end = r.offset.checked_add(r.len).ok_or(Error::Invalid)?;
        if end > buf.len as u64 || !r.offset.is_multiple_of(4) || !r.len.is_multiple_of(4) {
            return Err(Error::Invalid);
        }
        let floats = (r.len / 4) as usize;
        if floats < need_floats {
            return Err(Error::MsgSize);
        }
        // SAFETY: inside a mapped memory object, 4-byte aligned, bounds checked.
        Ok(unsafe { core::slice::from_raw_parts_mut(buf.ptr.add(r.offset as usize) as *mut f32, floats) })
    }

    /// Validate an operation before it is queued: unknown kinds and out-of-range
    /// buffer references must be refused at submit time, not when the work runs.
    fn validate(&self, o: &Op) -> Result<(), Error> {
        let d = o.dims;
        match o.kind {
            op::FILL => {
                self.slice(&o.dst, elems(d[0])?)?;
            }
            op::COPY | op::SCALE => {
                let n = elems(d[0])?;
                self.slice(&o.dst, n)?;
                self.slice(&o.a, n)?;
            }
            op::ADD | op::MUL | op::SILU_MUL | op::RMSNORM => {
                let n = elems(d[0])?;
                self.slice(&o.dst, n)?;
                self.slice(&o.a, n)?;
                self.slice(&o.b, n)?;
            }
            op::MATMUL => {
                let (m, k, n) = (elems(d[0])?, elems(d[1])?, elems(d[2])?);
                self.slice(&o.dst, area(m, n)?)?;
                self.slice(&o.a, area(m, k)?)?;
                self.slice(&o.b, area(n, k)?)?;
            }
            op::SOFTMAX => {
                let n = elems(d[0])?;
                self.slice(&o.dst, n)?;
                self.slice(&o.a, n)?;
            }
            op::ROPE => {
                let (heads, head_dim) = (elems(d[0])?, elems(d[1])?);
                if !head_dim.is_multiple_of(2) {
                    return Err(Error::Invalid);
                }
                self.slice(&o.dst, area(heads, head_dim)?)?;
            }
            op::EMBED => {
                let (row, index) = (elems(d[0])?, d[1] as usize);
                self.slice(&o.dst, row)?;
                let rows = index.checked_add(1).ok_or(Error::Invalid)?;
                self.slice(&o.a, area(row, rows)?)?;
            }
            op::ARGMAX => {
                self.slice(&o.a, elems(d[0])?)?;
            }
            op::SPIN => {}
            _ => return Err(Error::NoSys),
        }
        Ok(())
    }

    /// True while a pending ticket still points at buffer `id`.
    fn buffer_in_use(&self, id: u32) -> bool {
        self.tickets
            .iter()
            .filter(|t| t.state == TicketState::Pending)
            .any(|t| [&t.op.dst, &t.op.a, &t.op.b].iter().any(|r| !r.is_none() && r.id == id))
    }

    /// Execute at most one step of `t`. Returns true when the ticket finished.
    ///
    /// Every buffer is resolved again here rather than trusted from submit time: a
    /// client can release a buffer between two steps, and a service that panicked on
    /// that would take the whole compute device down with it.
    fn step(&self, t: &mut Ticket) -> Result<bool, Error> {
        let o = t.op;
        let d = o.dims;
        match o.kind {
            op::SPIN => {
                let total = d[0] as u64;
                let mut done = t.progress;
                let target = (done + STEP_UNITS).min(total);
                let mut acc = t.value;
                while done < target {
                    acc = acc.wrapping_mul(6364136223846793005).wrapping_add(done);
                    done += 1;
                }
                t.progress = done;
                t.value = acc;
                if done >= total {
                    t.value = total;
                    return Ok(true);
                }
                Ok(false)
            }
            op::MATMUL => {
                let (m, k, n) = (d[0] as usize, d[1] as usize, d[2] as usize);
                let dst = self.slice(&o.dst, area(m, n)?)?;
                let a = self.slice(&o.a, area(m, k)?)?;
                let b = self.slice(&o.b, area(n, k)?)?;
                // One row of the output per step keeps long matmuls interruptible.
                let row = t.progress as usize;
                let rows_this_step = 1.max(STEP_UNITS as usize / (k * n).max(1));
                let end = (row + rows_this_step).min(m);
                for r in row..end {
                    for c in 0..n {
                        let mut sum = 0.0f32;
                        for i in 0..k {
                            sum += a[r * k + i] * b[c * k + i];
                        }
                        dst[r * n + c] = sum;
                    }
                }
                t.progress = end as u64;
                Ok(end >= m)
            }
            op::FILL => {
                let dst = self.slice(&o.dst, d[0] as usize)?;
                for v in dst[..d[0] as usize].iter_mut() {
                    *v = o.scalar;
                }
                Ok(true)
            }
            op::COPY => {
                let n = d[0] as usize;
                let dst = self.slice(&o.dst, n)?;
                let a = self.slice(&o.a, n)?;
                dst[..n].copy_from_slice(&a[..n]);
                Ok(true)
            }
            op::SCALE => {
                let n = d[0] as usize;
                let dst = self.slice(&o.dst, n)?;
                let a = self.slice(&o.a, n)?;
                for i in 0..n {
                    dst[i] = a[i] * o.scalar;
                }
                Ok(true)
            }
            op::ADD | op::MUL => {
                let n = d[0] as usize;
                let dst = self.slice(&o.dst, n)?;
                let a = self.slice(&o.a, n)?;
                let b = self.slice(&o.b, n)?;
                for i in 0..n {
                    dst[i] = if o.kind == op::ADD { a[i] + b[i] } else { a[i] * b[i] };
                }
                Ok(true)
            }
            op::SILU_MUL => {
                let n = d[0] as usize;
                let dst = self.slice(&o.dst, n)?;
                let a = self.slice(&o.a, n)?;
                let b = self.slice(&o.b, n)?;
                for i in 0..n {
                    dst[i] = silu_f32(a[i]) * b[i];
                }
                Ok(true)
            }
            op::RMSNORM => {
                let n = d[0] as usize;
                let dst = self.slice(&o.dst, n)?;
                let a = self.slice(&o.a, n)?;
                let w = self.slice(&o.b, n)?;
                let sum: f32 = a[..n].iter().map(|v| v * v).sum();
                let scale = 1.0 / sqrt_f32(sum / n as f32 + 1e-5);
                for i in 0..n {
                    dst[i] = a[i] * scale * w[i];
                }
                Ok(true)
            }
            op::SOFTMAX => {
                let n = d[0] as usize;
                let dst = self.slice(&o.dst, n)?;
                let a = self.slice(&o.a, n)?;
                let mut max = a[0];
                for &v in &a[1..n] {
                    if v > max {
                        max = v;
                    }
                }
                let mut sum = 0.0f32;
                for i in 0..n {
                    let e = exp_f32(a[i] - max);
                    dst[i] = e;
                    sum += e;
                }
                for v in dst[..n].iter_mut() {
                    *v /= sum;
                }
                Ok(true)
            }
            op::ROPE => {
                let (heads, head_dim, pos) = (d[0] as usize, d[1] as usize, d[2] as usize);
                let x = self.slice(&o.dst, area(heads, head_dim)?)?;
                for h in 0..heads {
                    for i in (0..head_dim).step_by(2) {
                        let freq = 1.0 / powf_f32(ROPE_THETA, i as f32 / head_dim as f32);
                        let angle = pos as f32 * freq;
                        let (s, c) = (sin_f32(angle), cos_f32(angle));
                        let base = h * head_dim + i;
                        let (re, im) = (x[base], x[base + 1]);
                        x[base] = re * c - im * s;
                        x[base + 1] = re * s + im * c;
                    }
                }
                Ok(true)
            }
            op::EMBED => {
                let (row, index) = (d[0] as usize, d[1] as usize);
                let dst = self.slice(&o.dst, row)?;
                let table = self.slice(&o.a, area(row, index.checked_add(1).ok_or(Error::Invalid)?)?)?;
                dst[..row].copy_from_slice(&table[index * row..index * row + row]);
                Ok(true)
            }
            op::ARGMAX => {
                let n = d[0] as usize;
                let a = self.slice(&o.a, n)?;
                let mut best = 0usize;
                for i in 1..n {
                    if a[i] > a[best] {
                        best = i;
                    }
                }
                t.value = best as u64;
                Ok(true)
            }
            _ => Ok(true),
        }
    }

    fn ticket_index(&self, id: u32) -> Option<usize> {
        self.tickets.iter().position(|t| t.id == id)
    }

    fn handle(&mut self, r: &Request) -> bool {
        if !self.hello && r.kind != req::HELLO {
            self.reply_err(Error::Denied);
            return true;
        }
        match r.kind {
            req::HELLO => {
                if r.abi_version != ABI_VERSION {
                    println!(
                        "[compute] rejecting ABI v{} (this service speaks v{ABI_VERSION})",
                        r.abi_version
                    );
                    self.reply_err(Error::Invalid);
                } else {
                    self.hello = true;
                    self.reply(Response { value: ABI_VERSION as u64, ..Default::default() });
                }
            }
            req::DEVICE_QUERY => self.reply(Response {
                value: MAX_BUFFER_BYTES,
                flags: backend::CPU,
                ticket: ABI_VERSION,
                ..Default::default()
            }),
            req::BUFFER_CREATE => self.buffer_create(r.len),
            req::BUFFER_RELEASE => self.buffer_release(r.buffer),
            req::QUEUE_CREATE => match self.queues.iter().position(|used| !used) {
                Some(q) => {
                    self.queues[q] = true;
                    self.reply(Response { queue: q as u32, ..Default::default() });
                }
                None => self.reply_err(Error::NoMemory),
            },
            req::QUEUE_RELEASE => match self.queues.get_mut(r.queue as usize) {
                Some(used) if *used => {
                    *used = false;
                    self.reply(Response::default());
                }
                _ => self.reply_err(Error::BadHandle),
            },
            req::SUBMIT => self.submit(r),
            req::WAIT => self.wait(r),
            req::CANCEL => match self.ticket_index(r.ticket) {
                Some(i) => {
                    self.tickets[i].state = TicketState::Cancelled;
                    self.reply(Response { ticket: r.ticket, flags: state::CANCELLED, ..Default::default() });
                }
                None => self.reply_err(Error::BadHandle),
            },
            req::SHUTDOWN => {
                self.reply(Response::default());
                return false;
            }
            _ => self.reply_err(Error::NoSys),
        }
        true
    }

    fn buffer_create(&mut self, len: u64) {
        if len == 0 || len > MAX_BUFFER_BYTES {
            self.reply_err(Error::Invalid);
            return;
        }
        let Some(id) = self.buffers.iter().position(Option::is_none) else {
            self.reply_err(Error::NoMemory);
            return;
        };
        let handle = match sys::vmo_create(len as usize) {
            Ok(h) => h,
            Err(e) => return self.reply_err(e),
        };
        let ptr = match sys::vmo_map(handle, false) {
            Ok(p) => p,
            Err(e) => {
                sys::handle_close(handle).ok();
                return self.reply_err(e);
            }
        };
        // The client gets its own handle to the same object.
        let client = match sys::handle_dup(handle, libspace::spaceabi::handle::rights::MEMORY_ALL) {
            Ok(h) => h,
            Err(e) => {
                sys::mem_unmap(ptr, len as usize).ok();
                sys::handle_close(handle).ok();
                return self.reply_err(e);
            }
        };
        self.buffers[id] = Some(Buf { handle, ptr, len: len as usize });
        let response = Response { buffer: id as u32, value: len, ..Default::default() };
        if sys::send(self.channel, as_bytes(&response), Some(client)).is_err() {
            sys::handle_close(client).ok();
        }
    }

    fn buffer_release(&mut self, id: u32) {
        // Releasing memory a queued operation still points at would leave the step
        // loop resolving a buffer that no longer exists.
        if self.buffer_in_use(id) {
            return self.reply_err(Error::WouldBlock);
        }
        match self.buffers.get_mut(id as usize).and_then(Option::take) {
            Some(b) => {
                sys::mem_unmap(b.ptr, b.len).ok();
                sys::handle_close(b.handle).ok();
                self.reply(Response::default());
            }
            None => self.reply_err(Error::BadHandle),
        }
    }

    fn submit(&mut self, r: &Request) {
        if self.queues.get(r.queue as usize).copied() != Some(true) {
            return self.reply_err(Error::BadHandle);
        }
        if self.tickets.len() >= MAX_TICKETS {
            return self.reply_err(Error::NoMemory);
        }
        if let Err(e) = self.validate(&r.op) {
            return self.reply_err(e);
        }
        let id = self.next_ticket;
        self.next_ticket = self.next_ticket.wrapping_add(1).max(1);
        self.tickets.push(Ticket {
            id,
            queue: r.queue,
            op: r.op,
            state: TicketState::Pending,
            progress: 0,
            value: 0,
            status: 0,
        });
        self.reply(Response { ticket: id, flags: state::RUNNING, ..Default::default() });
    }

    fn wait(&mut self, r: &Request) {
        let Some(index) = self.ticket_index(r.ticket) else {
            return self.reply_err(Error::BadHandle);
        };
        let deadline = sys::ticks_ms().saturating_add(r.timeout_ms);
        loop {
            match self.tickets[index].state {
                TicketState::Cancelled => {
                    let t = self.tickets.remove(index);
                    return self.reply(Response {
                        ticket: t.id,
                        queue: t.queue,
                        flags: state::CANCELLED,
                        ..Default::default()
                    });
                }
                TicketState::Done => {
                    let t = self.tickets.remove(index);
                    return self.reply(Response {
                        ticket: t.id,
                        queue: t.queue,
                        value: t.value,
                        status: t.status,
                        flags: state::DONE,
                        ..Default::default()
                    });
                }
                TicketState::Pending => {}
            }
            let mut ticket = core::mem::replace(
                &mut self.tickets[index],
                Ticket {
                    id: 0,
                    queue: 0,
                    op: Op::default(),
                    state: TicketState::Pending,
                    progress: 0,
                    value: 0,
                    status: 0,
                },
            );
            let finished = match self.step(&mut ticket) {
                Ok(done) => done,
                // The work itself failed (a buffer went away underneath it). Finish
                // the ticket with the error instead of taking the service down.
                Err(e) => {
                    ticket.status = status_of(e);
                    ticket.value = 0;
                    true
                }
            };
            if finished {
                ticket.state = TicketState::Done;
            }
            self.tickets[index] = ticket;
            if !finished && sys::ticks_ms() >= deadline {
                let t = &self.tickets[index];
                return self.reply(Response {
                    ticket: t.id,
                    queue: t.queue,
                    status: status_of(Error::WouldBlock),
                    flags: state::RUNNING,
                    ..Default::default()
                });
            }
        }
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn space_main() -> i32 {
    println!("[compute] Space Compute service, ABI v{ABI_VERSION}, CPU backend");
    let mut svc = Service::new(handle::BOOTSTRAP);
    let mut buf = [0u8; core::mem::size_of::<Request>()];
    loop {
        match sys::recv(handle::BOOTSTRAP, &mut buf, false) {
            // No request carries a handle. Close anything a client transfers anyway,
            // or a stream of them would exhaust this process's handle table.
            Ok((n, transferred)) if n == buf.len() => {
                if let Some(h) = transferred {
                    sys::handle_close(h).ok();
                }
                // SAFETY: the client sends exactly one `Request`.
                let r: Request = unsafe { core::ptr::read_unaligned(buf.as_ptr() as *const Request) };
                if !svc.handle(&r) {
                    println!("[compute] shutdown requested; exiting");
                    return 0;
                }
            }
            Ok((n, transferred)) => {
                if let Some(h) = transferred {
                    sys::handle_close(h).ok();
                }
                println!("[compute] malformed request of {n} bytes");
                svc.reply_err(Error::MsgSize);
            }
            Err(Error::PeerClosed) => {
                println!("[compute] client disconnected; exiting");
                return 0;
            }
            Err(e) => {
                println!("[compute] receive failed: {e:?}");
                return 1;
            }
        }
    }
}
