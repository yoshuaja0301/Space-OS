//! Cloud adapter protocol (`spacecloud`, requirement I01, ADR-0018).
//!
//! One program talks to a cloud model provider, and it holds everything that takes:
//! the provider's trust anchor, the API credential, a network session that reaches
//! the provider's one address and nothing else, and a channel to the Tool Broker
//! for the model's tool calls. None of it is handed on. A client only *asks*; the
//! adapter decides whether the ask may leave the machine at all (local-only work
//! never does) and whether it fits the budget, then streams the answer back.
//!
//! Two kinds of channel:
//!
//! * the **operator channel** (the adapter's bootstrap handle): [`op`] requests
//!   that configure it -- trust anchor, credential, endpoint, network session,
//!   broker, budget -- and hand it client channels;
//! * a **client channel**: [`Ask`] in, a stream of [`Event`]s out, ending with
//!   [`event::DONE`] or [`event::ERROR`].
//!
//! Money is counted in micro-dollars (µ$), prices in µ$ per million tokens, all in
//! integers: see [`cost`].

use crate::error::Error;

pub const ABI_VERSION: u32 = 0;

/// Longest model name in an ask.
pub const MODEL_MAX: usize = 32;
/// Longest prompt one ask carries.
pub const PROMPT_MAX: usize = 176;
/// Text bytes carried by one event.
pub const TEXT_MAX: usize = 216;
/// Payload bytes of one operator request (a trust-anchor chunk, a credential, an
/// endpoint).
pub const OP_DATA_MAX: usize = 192;
/// Text bytes of an operator reply.
pub const REPLY_TEXT_MAX: usize = 128;
/// Largest trust anchor the adapter accepts (DER).
pub const TRUST_MAX: usize = 4096;

/// Operator requests ([`Op::kind`]).
pub mod op {
    /// Version check; the reply names the adapter and its integration status.
    pub const HELLO: u32 = 1;
    /// Bytes `offset..offset + len` of the provider's trust anchor (DER), whose full
    /// size is `total`. The anchor is in force once every byte has arrived.
    pub const TRUST: u32 = 2;
    /// The API credential. Replaces the one held, if any.
    pub const CREDENTIAL: u32 = 3;
    /// `host:port` of the provider.
    pub const ENDPOINT: u32 = 4;
    /// Carries the network session the provider is reached through.
    pub const NET: u32 = 5;
    /// Carries the channel to the Tool Broker the model's tool calls go through.
    pub const TOOLS: u32 = 6;
    /// Budget (µ$), prices (µ$ per million tokens) and attempts per request.
    pub const BUDGET: u32 = 7;
    /// Carries a client channel. Its asks are served until it closes.
    pub const CLIENT: u32 = 8;
    /// Spend and counters.
    pub const STATS: u32 = 9;
    /// Reply, then exit.
    pub const QUIT: u32 = 10;
}

/// Client requests ([`Ask::kind`]).
pub mod ask {
    pub const ASK: u32 = 1;
}

/// [`Ask::flags`].
pub mod flags {
    /// The work must stay on this machine: refused, and nothing is sent.
    pub const LOCAL_ONLY: u32 = 1;
    /// The model may call tools, through the Tool Broker.
    pub const TOOLS: u32 = 2;
}

/// Events the adapter sends a client ([`Event::kind`]).
pub mod event {
    /// Answer text, as the provider streamed it.
    pub const DELTA: u32 = 1;
    /// A tool call the model made; `code` is the broker's verdict
    /// ([`crate::agent::verdict`]), `text` the call.
    pub const TOOL: u32 = 2;
    /// The answer is complete; the counters are final.
    pub const DONE: u32 = 3;
    /// The ask failed; `code` says why ([`fail`]), `text` in words.
    pub const ERROR: u32 = 4;
}

/// Why an ask failed ([`Event::code`] of an [`event::ERROR`]).
pub mod fail {
    /// The ask is local-only: refused, nothing sent.
    pub const LOCAL_ONLY: u32 = 1;
    /// It could cost more than the budget has left: refused, nothing sent.
    pub const BUDGET: u32 = 2;
    /// The provider went past what was reserved for the answer: cut off.
    pub const OVER_BUDGET: u32 = 3;
    /// The provider refused the credential.
    pub const AUTH: u32 = 4;
    /// Nothing more within the time allowed.
    pub const TIMEOUT: u32 = 5;
    /// The provider stayed overloaded through every attempt.
    pub const OVERLOADED: u32 = 6;
    /// The provider answered with an error of its own.
    pub const PROVIDER: u32 = 7;
    /// The provider's answer did not parse.
    pub const PROTOCOL: u32 = 8;
    /// The connection could not be opened, or broke.
    pub const NET: u32 = 9;
    /// TLS refused the provider, or failed.
    pub const TLS: u32 = 10;
    /// The model kept calling tools past the limit, or no broker is attached.
    pub const TOOLS: u32 = 11;
    /// The adapter has no credential, trust anchor, endpoint or session yet.
    pub const NOT_READY: u32 = 12;
    /// The ask itself is malformed.
    pub const INVALID: u32 = 13;

    pub fn name(code: u32) -> &'static str {
        match code {
            LOCAL_ONLY => "local-only",
            BUDGET => "budget",
            OVER_BUDGET => "over-budget",
            AUTH => "auth",
            TIMEOUT => "timeout",
            OVERLOADED => "overloaded",
            PROVIDER => "provider",
            PROTOCOL => "protocol",
            NET => "net",
            TLS => "tls",
            TOOLS => "tools",
            NOT_READY => "not-ready",
            INVALID => "invalid",
            _ => "?",
        }
    }
}

/// What `input` and `output` tokens cost at these prices, in µ$, rounded up: the
/// adapter never under-counts what it spends.
pub fn cost(input: u64, output: u64, price_in: u64, price_out: u64) -> u64 {
    let micro_millions = input.saturating_mul(price_in).saturating_add(output.saturating_mul(price_out));
    micro_millions.div_ceil(1_000_000)
}

fn copy_in(dst: &mut [u8], src: &[u8]) -> u32 {
    let n = src.len().min(dst.len());
    dst[..n].copy_from_slice(&src[..n]);
    n as u32
}

fn text_of(b: &[u8], len: u32) -> &str {
    let n = (len as usize).min(b.len());
    match core::str::from_utf8(&b[..n]) {
        Ok(s) => s,
        // A chunk boundary can split a character: show what is whole.
        Err(e) => core::str::from_utf8(&b[..e.valid_up_to()]).unwrap_or(""),
    }
}

macro_rules! record {
    ($t:ident) => {
        impl $t {
            pub fn as_bytes(&self) -> &[u8] {
                // SAFETY: plain `repr(C)` data; every padding byte is an explicit
                // field, so no byte is uninitialised.
                unsafe {
                    core::slice::from_raw_parts(
                        self as *const Self as *const u8,
                        core::mem::size_of::<Self>(),
                    )
                }
            }

            pub fn from_bytes(b: &[u8]) -> Option<$t> {
                if b.len() != core::mem::size_of::<$t>() {
                    return None;
                }
                // SAFETY: exact size; every bit pattern is a valid value.
                Some(unsafe { core::ptr::read_unaligned(b.as_ptr() as *const $t) })
            }
        }
    };
}

/// An operator request.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct Op {
    pub kind: u32,
    pub abi_version: u32,
    pub offset: u32,
    pub total: u32,
    pub len: u32,
    /// [`op::BUDGET`]: attempts one request may take (first try included).
    pub attempts: u32,
    /// [`op::BUDGET`]: the cost cap, µ$.
    pub budget: u64,
    /// [`op::BUDGET`]: µ$ per million input and output tokens.
    pub price_in: u64,
    pub price_out: u64,
    pub data: [u8; OP_DATA_MAX],
}
record!(Op);

impl Op {
    pub fn new(kind: u32) -> Op {
        Op {
            kind,
            abi_version: ABI_VERSION,
            offset: 0,
            total: 0,
            len: 0,
            attempts: 0,
            budget: 0,
            price_in: 0,
            price_out: 0,
            data: [0; OP_DATA_MAX],
        }
    }

    /// `kind` carrying `data` (cut to [`OP_DATA_MAX`]).
    pub fn with_data(kind: u32, data: &[u8]) -> Op {
        let mut o = Op::new(kind);
        o.len = copy_in(&mut o.data, data);
        o
    }

    pub fn data(&self) -> &[u8] {
        &self.data[..(self.len as usize).min(OP_DATA_MAX)]
    }
}

/// The adapter's answer to an operator request.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct OpReply {
    /// 0 or a negated [`Error`].
    pub status: i32,
    pub len: u32,
    /// µ$ charged so far, and the cap.
    pub spent: u64,
    pub budget: u64,
    pub asks: u32,
    /// Asks refused before anything was sent (local-only, budget).
    pub refused: u32,
    /// Connections opened to the provider.
    pub connections: u32,
    pub tool_calls: u32,
    pub text: [u8; REPLY_TEXT_MAX],
}
record!(OpReply);

impl Default for OpReply {
    fn default() -> OpReply {
        OpReply {
            status: 0,
            len: 0,
            spent: 0,
            budget: 0,
            asks: 0,
            refused: 0,
            connections: 0,
            tool_calls: 0,
            text: [0; REPLY_TEXT_MAX],
        }
    }
}

impl OpReply {
    pub fn error(e: Error) -> OpReply {
        OpReply { status: -(e as u32 as i32), ..Default::default() }
    }

    pub fn set_text(&mut self, s: &str) {
        self.len = copy_in(&mut self.text, s.as_bytes());
    }

    pub fn text(&self) -> &str {
        text_of(&self.text, self.len)
    }

    pub fn result(&self) -> Result<(), Error> {
        if self.status == 0 {
            Ok(())
        } else {
            Err(Error::from_code((-self.status) as u32).unwrap_or(Error::Invalid))
        }
    }
}

/// A client's request.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct Ask {
    pub kind: u32,
    pub abi_version: u32,
    pub flags: u32,
    /// Most tokens the answer may have: the adapter reserves their cost up front.
    pub max_tokens: u32,
    /// For the whole ask, tool rounds included.
    pub timeout_ms: u32,
    pub model_len: u32,
    pub prompt_len: u32,
    pub _pad: u32,
    pub model: [u8; MODEL_MAX],
    pub prompt: [u8; PROMPT_MAX],
}
record!(Ask);

impl Ask {
    pub fn new(model: &str, prompt: &str, flags: u32, max_tokens: u32, timeout_ms: u32) -> Ask {
        let mut a = Ask {
            kind: ask::ASK,
            abi_version: ABI_VERSION,
            flags,
            max_tokens,
            timeout_ms,
            model_len: 0,
            prompt_len: 0,
            _pad: 0,
            model: [0; MODEL_MAX],
            prompt: [0; PROMPT_MAX],
        };
        a.model_len = copy_in(&mut a.model, model.as_bytes());
        a.prompt_len = copy_in(&mut a.prompt, prompt.as_bytes());
        a
    }

    pub fn model(&self) -> &str {
        text_of(&self.model, self.model_len)
    }

    pub fn prompt(&self) -> &str {
        text_of(&self.prompt, self.prompt_len)
    }
}

/// One event of an answer.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct Event {
    pub kind: u32,
    pub code: u32,
    /// Tokens the provider has reported for this ask so far.
    pub input_tokens: u32,
    pub output_tokens: u32,
    /// µ$ charged for this ask so far.
    pub cost: u64,
    /// Requests made to the provider for this ask, retries included.
    pub attempts: u32,
    pub len: u32,
    pub text: [u8; TEXT_MAX],
}
record!(Event);

impl Event {
    pub fn new(kind: u32, code: u32, text: &str) -> Event {
        let mut e = Event {
            kind,
            code,
            input_tokens: 0,
            output_tokens: 0,
            cost: 0,
            attempts: 0,
            len: 0,
            text: [0; TEXT_MAX],
        };
        e.len = copy_in(&mut e.text, text.as_bytes());
        e
    }

    pub fn text(&self) -> &str {
        text_of(&self.text, self.len)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::syscall::MSG_MAX;

    #[test]
    fn records_fit_a_message_and_have_no_hidden_padding() {
        assert_eq!(core::mem::size_of::<Op>(), 6 * 4 + 3 * 8 + OP_DATA_MAX);
        assert_eq!(core::mem::size_of::<OpReply>(), 2 * 4 + 2 * 8 + 4 * 4 + REPLY_TEXT_MAX);
        assert_eq!(core::mem::size_of::<Ask>(), 8 * 4 + MODEL_MAX + PROMPT_MAX);
        assert_eq!(core::mem::size_of::<Event>(), 4 * 4 + 8 + 2 * 4 + TEXT_MAX);
        for size in [
            core::mem::size_of::<Op>(),
            core::mem::size_of::<OpReply>(),
            core::mem::size_of::<Ask>(),
            core::mem::size_of::<Event>(),
        ] {
            assert!(size <= MSG_MAX, "{size} bytes do not fit a {MSG_MAX}-byte message");
        }
    }

    #[test]
    fn cost_rounds_up_and_does_not_overflow() {
        // 1000 input tokens at $3/MTok and 100 output tokens at $15/MTok: 4500 µ$.
        assert_eq!(cost(1000, 100, 3_000_000, 15_000_000), 4500);
        // One token at $3/MTok is 3 µ$ exactly; at $0.25/MTok it rounds up to 1 µ$.
        assert_eq!(cost(1, 0, 3_000_000, 0), 3);
        assert_eq!(cost(1, 0, 250_000, 0), 1);
        assert_eq!(cost(0, 0, 3_000_000, 15_000_000), 0);
        assert_eq!(cost(u64::MAX, u64::MAX, u64::MAX, u64::MAX), u64::MAX.div_ceil(1_000_000));
    }

    #[test]
    fn text_survives_a_round_trip_and_a_split_character() {
        let a = Ask::new("lab-echo", "héllo", flags::TOOLS, 100, 5000);
        let b = Ask::from_bytes(a.as_bytes()).unwrap();
        assert_eq!((b.model(), b.prompt(), b.flags, b.max_tokens), ("lab-echo", "héllo", flags::TOOLS, 100));
        // "é" is two bytes: cutting between them leaves the whole characters only.
        let mut e = Event::new(event::DELTA, 0, "hé");
        e.len = 2;
        assert_eq!(e.text(), "h");
        assert!(Op::from_bytes(&[0u8; 3]).is_none());
    }
}
