//! Space Compute ABI, version 0.
//!
//! The contract between a client (the AI runtime) and the `spacecompute` service
//! that owns a compute backend. Control travels as fixed-size messages over a
//! channel; bulk data lives in memory objects both sides map, so tensors never
//! cross the channel (PRD §4: no raw pointers between processes, explicit types
//! and sizes, opaque handles, ownership rules, error codes, timeout, cancellation
//! and version negotiation).
//!
//! Every reply carries a status: `0` for success, or the negated
//! [`crate::error::Error`] code, so a client decodes errors exactly as it decodes
//! syscall results.

/// Version of this contract. A client sends it in [`req::HELLO`]; a service that
/// speaks a different version refuses with `Error::Invalid`.
pub const ABI_VERSION: u32 = 0;

/// Backend identifiers reported by [`req::DEVICE_QUERY`].
pub mod backend {
    pub const CPU: u32 = 1;
    pub const GPU: u32 = 2;
    pub const NPU: u32 = 3;
}

/// Request kinds.
pub mod req {
    /// Negotiate the ABI version. Must be the first message on a connection.
    pub const HELLO: u32 = 1;
    /// Report backend, ABI version and limits.
    pub const DEVICE_QUERY: u32 = 2;
    /// Allocate a buffer; the reply transfers a memory-object handle.
    pub const BUFFER_CREATE: u32 = 3;
    /// Release a buffer the service holds.
    pub const BUFFER_RELEASE: u32 = 4;
    /// Create a submission queue.
    pub const QUEUE_CREATE: u32 = 5;
    /// Queue one operation; the reply carries a ticket.
    pub const SUBMIT: u32 = 6;
    /// Run queued work until the ticket completes, the deadline passes, or it is cancelled.
    pub const WAIT: u32 = 7;
    /// Cancel a queued or running ticket.
    pub const CANCEL: u32 = 8;
    /// Release a queue.
    pub const QUEUE_RELEASE: u32 = 9;
    /// Ask the service to exit.
    pub const SHUTDOWN: u32 = 10;
}

/// Operations the CPU backend implements. Anything else must be refused with
/// `Error::NoSys` rather than silently ignored.
pub mod op {
    /// Write `scalar` to every element of `dst`.
    pub const FILL: u32 = 1;
    /// `dst = a`
    pub const COPY: u32 = 2;
    /// `dst = a + b`
    pub const ADD: u32 = 3;
    /// `dst = a * b` (element-wise)
    pub const MUL: u32 = 4;
    /// `dst[m,n] = sum_k a[m,k] * b[n,k]` with `dims = [m, k, n, 0]`
    /// (`b` is stored transposed, the layout weights use).
    pub const MATMUL: u32 = 5;
    /// RMS normalisation of `a` scaled by weights `b`; `dims[0]` = length.
    pub const RMSNORM: u32 = 6;
    /// In-place softmax over the first `dims[0]` elements of `a` into `dst`.
    pub const SOFTMAX: u32 = 7;
    /// SiLU gate: `dst = a * sigmoid(a) * b`.
    pub const SILU_MUL: u32 = 8;
    /// Rotary embedding of `dst` in place; `dims = [heads, head_dim, position, 0]`.
    pub const ROPE: u32 = 9;
    /// Gather row `dims[1]` of the table in `a` (row length `dims[0]`) into `dst`.
    pub const EMBED: u32 = 10;
    /// Index of the largest element of `a` over `dims[0]` values, returned in the reply.
    pub const ARGMAX: u32 = 11;
    /// `dst = a * scalar` over `dims[0]` elements.
    pub const SCALE: u32 = 13;
    /// Burn `dims[0]` work units. Exists so timeout and cancellation can be tested
    /// deterministically.
    pub const SPIN: u32 = 12;
}

/// A slice of a buffer, in bytes.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct BufferRef {
    pub id: u32,
    pub _pad: u32,
    pub offset: u64,
    pub len: u64,
}

impl BufferRef {
    pub const NONE: BufferRef = BufferRef { id: u32::MAX, _pad: 0, offset: 0, len: 0 };

    pub const fn whole(id: u32, len: u64) -> Self {
        BufferRef { id, _pad: 0, offset: 0, len }
    }

    pub const fn is_none(&self) -> bool {
        self.id == u32::MAX
    }
}

/// One operation.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct Op {
    pub kind: u32,
    pub _pad: u32,
    pub dst: BufferRef,
    pub a: BufferRef,
    pub b: BufferRef,
    pub dims: [u32; 4],
    pub scalar: f32,
    pub _pad2: u32,
}

/// A control message from client to service.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct Request {
    pub kind: u32,
    pub abi_version: u32,
    pub queue: u32,
    pub ticket: u32,
    pub buffer: u32,
    pub _pad: u32,
    pub len: u64,
    pub timeout_ms: u64,
    pub op: Op,
}

/// The reply to a [`Request`].
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct Response {
    /// 0 on success, otherwise the negated [`crate::error::Error`] code.
    pub status: i32,
    pub ticket: u32,
    pub queue: u32,
    pub buffer: u32,
    /// Operation result (argmax index, device limit, ...).
    pub value: u64,
    pub flags: u32,
    pub _pad: u32,
}

impl Response {
    pub fn result(&self) -> Result<u64, crate::error::Error> {
        if self.status == 0 {
            Ok(self.value)
        } else {
            crate::error::decode(self.status as isize).map(|v| v as u64)
        }
    }
}

/// Ticket states reported by [`req::WAIT`] in [`Response::flags`].
pub mod state {
    pub const DONE: u32 = 1;
    pub const RUNNING: u32 = 2;
    pub const CANCELLED: u32 = 3;
}

/// Limits a service must enforce.
pub const MAX_BUFFERS: usize = 64;
pub const MAX_QUEUES: usize = 4;
pub const MAX_TICKETS: usize = 64;
/// Largest buffer the CPU backend hands out (bytes).
pub const MAX_BUFFER_BYTES: u64 = 64 * 1024 * 1024;
