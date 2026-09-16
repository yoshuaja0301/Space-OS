//! Client side of the Space Compute ABI (see `spaceabi::compute`).
//!
//! Control messages travel over a channel; tensors live in memory objects the
//! service allocates and both sides map, so no tensor data is ever copied through
//! the channel and no raw pointer crosses a process boundary.

use spaceabi::compute::{BufferRef, Op, Request, Response, req, state};
use spaceabi::error::Error;
use spaceabi::handle::Handle;

use crate::sys;

/// A buffer the service allocated and this process has mapped.
#[derive(Clone, Copy, Debug)]
pub struct Buffer {
    pub id: u32,
    pub handle: Handle,
    pub ptr: *mut u8,
    pub len: usize,
}

impl Buffer {
    /// The buffer as `f32`s. The service hands out page-aligned memory, so the
    /// alignment always holds.
    ///
    /// # Safety
    /// The caller must not alias the same range through another slice while the
    /// service is running an operation on it.
    pub unsafe fn as_f32_mut(&self) -> &'static mut [f32] {
        // SAFETY: a mapped memory object of `len` bytes, page aligned.
        unsafe { core::slice::from_raw_parts_mut(self.ptr as *mut f32, self.len / 4) }
    }

    pub fn reference(&self) -> BufferRef {
        BufferRef::whole(self.id, self.len as u64)
    }
}

/// A connection to a compute service.
pub struct Compute {
    channel: Handle,
}

fn as_bytes<T>(v: &T) -> &[u8] {
    // SAFETY: `T` is a `repr(C)` plain-data message.
    unsafe { core::slice::from_raw_parts(v as *const T as *const u8, core::mem::size_of::<T>()) }
}

impl Compute {
    /// Attach to a service reachable over `channel` and negotiate the ABI version.
    pub fn connect(channel: Handle) -> Result<Compute, Error> {
        let c = Compute { channel };
        let r = c.call(&Request {
            kind: req::HELLO,
            abi_version: spaceabi::compute::ABI_VERSION,
            ..Default::default()
        })?;
        r.result()?;
        Ok(c)
    }

    /// Send a request and wait for its reply. The reply status is left to the
    /// caller so contract tests can inspect failures.
    pub fn call(&self, request: &Request) -> Result<Response, Error> {
        sys::send(self.channel, as_bytes(request), None)?;
        let mut buf = [0u8; core::mem::size_of::<Response>()];
        let (n, _) = sys::recv(self.channel, &mut buf, false)?;
        if n != buf.len() {
            return Err(Error::MsgSize);
        }
        // SAFETY: the service replies with exactly one `Response`.
        Ok(unsafe { core::ptr::read_unaligned(buf.as_ptr() as *const Response) })
    }

    pub fn device_query(&self) -> Result<(u32, u32, u64), Error> {
        let r = self.call(&Request { kind: req::DEVICE_QUERY, ..Default::default() })?;
        let limit = r.result()?;
        Ok((r.flags, r.ticket, limit))
    }

    /// Allocate a buffer and map it into this process.
    pub fn buffer_create(&self, len: usize) -> Result<Buffer, Error> {
        sys::send(
            self.channel,
            as_bytes(&Request { kind: req::BUFFER_CREATE, len: len as u64, ..Default::default() }),
            None,
        )?;
        let mut buf = [0u8; core::mem::size_of::<Response>()];
        let (n, handle) = sys::recv(self.channel, &mut buf, false)?;
        if n != buf.len() {
            return Err(Error::MsgSize);
        }
        // SAFETY: exactly one `Response`.
        let r: Response = unsafe { core::ptr::read_unaligned(buf.as_ptr() as *const Response) };
        if r.status != 0 {
            if let Some(h) = handle {
                sys::handle_close(h).ok();
            }
            return Err(r.result().unwrap_err());
        }
        let handle = handle.ok_or(Error::Invalid)?;
        let ptr = match sys::vmo_map(handle, false) {
            Ok(p) => p,
            Err(e) => {
                // Neither side should keep a buffer this client cannot use: drop the
                // handle and hand the service's slot back.
                sys::handle_close(handle).ok();
                self.call(&Request { kind: req::BUFFER_RELEASE, buffer: r.buffer, ..Default::default() })
                    .ok();
                return Err(e);
            }
        };
        Ok(Buffer { id: r.buffer, handle, ptr, len })
    }

    pub fn buffer_release(&self, b: &Buffer) -> Result<(), Error> {
        let r = self.call(&Request { kind: req::BUFFER_RELEASE, buffer: b.id, ..Default::default() })?;
        r.result()?;
        sys::mem_unmap(b.ptr, b.len).ok();
        sys::handle_close(b.handle).ok();
        Ok(())
    }

    pub fn queue_create(&self) -> Result<u32, Error> {
        let r = self.call(&Request { kind: req::QUEUE_CREATE, ..Default::default() })?;
        r.result()?;
        Ok(r.queue)
    }

    pub fn submit(&self, queue: u32, op: Op) -> Result<u32, Error> {
        let r = self.call(&Request { kind: req::SUBMIT, queue, op, ..Default::default() })?;
        r.result()?;
        Ok(r.ticket)
    }

    /// Run the ticket to completion; returns its result value.
    pub fn wait(&self, ticket: u32, timeout_ms: u64) -> Result<u64, Error> {
        let r = self.call(&Request { kind: req::WAIT, ticket, timeout_ms, ..Default::default() })?;
        let value = r.result()?;
        if r.flags == state::DONE { Ok(value) } else { Err(Error::WouldBlock) }
    }

    /// Wait without turning a timeout into an error: returns `(status flags, value)`.
    pub fn wait_raw(&self, ticket: u32, timeout_ms: u64) -> Result<Response, Error> {
        self.call(&Request { kind: req::WAIT, ticket, timeout_ms, ..Default::default() })
    }

    pub fn cancel(&self, ticket: u32) -> Result<(), Error> {
        let r = self.call(&Request { kind: req::CANCEL, ticket, ..Default::default() })?;
        r.result().map(|_| ())
    }

    /// Submit `op` and wait for it, the common case.
    pub fn run(&self, queue: u32, op: Op) -> Result<u64, Error> {
        let t = self.submit(queue, op)?;
        self.wait(t, 60_000)
    }

    pub fn shutdown(&self) -> Result<(), Error> {
        let r = self.call(&Request { kind: req::SHUTDOWN, ..Default::default() })?;
        r.result().map(|_| ())
    }
}
