//! `spacecloud` – the cloud model adapter (requirement I01, ADR-0018).
//!
//! The one program that talks to a cloud model provider. It holds what that takes
//! and hands none of it on: the provider's trust anchor and the API credential
//! (given by the operator, never read from anywhere by this program: it has no file
//! capability at all), a network session that reaches the provider's one address,
//! and a channel to the Tool Broker, through which -- and only through which -- the
//! model's tool calls touch the machine.
//!
//! A client sends an [`Ask`]; the answer comes back as it is generated, as
//! [`Event`]s. Before anything is sent, the ask must be allowed to leave the machine
//! at all (local-only work is refused) and its worst case -- every token it may
//! produce -- must fit what is left of the budget. While it streams, the provider is
//! held to that reservation. Busy providers are retried a bounded number of times,
//! and only before any answer has reached the client.
//!
//! The wire format is a Messages-style API (HTTP/1.1, JSON, server-sent events). It
//! is tested against the lab's mock provider only, and says so when asked.
#![no_std]
#![no_main]

extern crate alloc;

mod http;
mod sse;

use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;

use libspace::net::Session;
use libspace::spaceabi::agent::{self, CHUNK_MAX, ToolReply, ToolRequest, tool, verdict};
use libspace::spaceabi::cloud::{
    ABI_VERSION, Ask, Event, Op, OpReply, TEXT_MAX, TRUST_MAX, ask, cost, event, fail, flags, op,
};
use libspace::spaceabi::dns;
use libspace::spaceabi::error::Error;
use libspace::spaceabi::syscall::{MSG_MAX, WAIT_FOREVER};
use libspace::{Handle, handle, println, sys};
use serde_json::{Value, json};
use spacetls::{TlsConfig, TlsStream};

use http::Conn;

/// TLS, JSON and the conversation so far.
const HEAP_PAGES: usize = 256;
const IDENTITY: &str = "spacecloud 0.1: Messages API 2023-06-01 over TLS 1.3; experimental, tested against the lab mock provider only";
const API_VERSION: &str = "2023-06-01";
const MAX_TOKENS_MAX: u32 = 8192;
const TIMEOUT_MAX: u32 = 600_000;
/// Requests one ask may make for tool calls before it is stopped.
const TOOL_ROUNDS_MAX: usize = 4;
const ATTEMPTS_MAX: u32 = 5;
const CLIENTS_MAX: usize = 4;
/// Bytes of a file one tool call returns to the model.
const TOOL_OUTPUT_MAX: usize = 2048;
const ERROR_BODY_MAX: usize = 2048;
const TOOL_INPUT_MAX: usize = 16 * 1024;
/// Tool calls acted on per model turn, and content blocks kept per answer.
const TOOL_CALLS_MAX: usize = 8;
const BLOCKS_MAX: usize = 64;
/// Deepest JSON nesting the adapter parses: well inside what the parser itself
/// allows (128), because this program's stack is 64 KiB and a provider decides
/// what it sends.
const JSON_DEPTH_MAX: usize = 32;
/// Answer text per requested token: no real token is longer, so more text than
/// this means the provider is past `max_tokens` whatever it reports.
const BYTES_PER_TOKEN_MAX: usize = 32;
/// Time the broker has to answer one tool request.
const BROKER_MS: u64 = 5000;
/// Time allowed for delivering the last event of an ask, deadline or not.
const FINAL_MS: u64 = 1000;

/// Why an ask failed, and whether trying again could help.
pub struct Fail {
    pub code: u32,
    pub text: String,
    pub retry: bool,
}

impl Fail {
    pub fn new(code: u32, text: &str) -> Fail {
        Fail { code, text: String::from(text), retry: false }
    }

    /// A provider too busy to answer: worth another attempt, if nothing has been
    /// passed on to the client yet.
    fn busy(code: u32, text: &str) -> Fail {
        Fail { code, text: String::from(text), retry: true }
    }
}

fn left(deadline: u64) -> Result<u64, Fail> {
    match deadline.saturating_sub(sys::ticks_ms()) {
        0 => Err(Fail::new(fail::TIMEOUT, "no answer within the time allowed")),
        n => Ok(n),
    }
}

/// Deliver one event to a client that may be slow to read, until `deadline`.
fn emit(client: Handle, e: &Event, deadline: u64) -> Result<(), Fail> {
    loop {
        match sys::send(client, e.as_bytes(), None) {
            Ok(()) => return Ok(()),
            Err(Error::WouldBlock) if sys::ticks_ms() < deadline => sys::sleep_ms(2),
            Err(Error::WouldBlock) => {
                return Err(Fail::new(fail::TIMEOUT, "the client stopped reading the answer"));
            }
            Err(e) => return Err(Fail::new(fail::INVALID, &format!("the client is gone ({e})"))),
        }
    }
}

/// Parse JSON from the provider, refusing nesting deeper than [`JSON_DEPTH_MAX`]
/// before the parser recurses into it.
fn parse_json(data: &[u8]) -> Option<Value> {
    let (mut depth, mut in_string, mut escaped) = (0usize, false, false);
    for &b in data {
        if in_string {
            match (escaped, b) {
                (true, _) => escaped = false,
                (false, b'\\') => escaped = true,
                (false, b'"') => in_string = false,
                _ => {}
            }
            continue;
        }
        match b {
            b'"' => in_string = true,
            b'[' | b'{' => {
                depth += 1;
                if depth > JSON_DEPTH_MAX {
                    return None;
                }
            }
            b']' | b'}' => depth = depth.saturating_sub(1),
            _ => {}
        }
    }
    serde_json::from_slice(data).ok()
}

fn as_bytes<T>(v: &T) -> &[u8] {
    // SAFETY: `T` is a `repr(C)` plain-data message of the broker protocol.
    unsafe { core::slice::from_raw_parts(v as *const T as *const u8, core::mem::size_of::<T>()) }
}

/// The largest prefix of `s` of at most `max` bytes that ends on a character.
fn cut(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// The tools a model may call. Each goes through the Tool Broker, which decides.
fn tool_definitions() -> Value {
    json!([{
        "name": "read_file",
        "description": "Read a file in the user's workspace, /spaceos/ws. Anything outside it is refused.",
        "input_schema": {
            "type": "object",
            "properties": {"path": {"type": "string", "description": "Absolute path of the file"}},
            "required": ["path"]
        }
    }])
}

enum Block {
    Text(String),
    Tool { id: String, name: String, input: String },
    Other,
}

/// One answer as it streams in.
#[derive(Default)]
struct Answer {
    input: u64,
    output: u64,
    blocks: Vec<Block>,
    stop_reason: String,
    stopped: bool,
    /// Answer text received, whatever usage the provider reports.
    text_bytes: usize,
    /// Text has been passed on to the client: this request can no longer be tried
    /// again without the client seeing it twice.
    streamed: bool,
}

/// One ask's tally, reported with its last event.
#[derive(Default)]
struct Tally {
    input: u64,
    output: u64,
    cost: u64,
    attempts: u32,
    rounds: u32,
    tools: Vec<String>,
}

struct Adapter {
    operator: Handle,
    clients: Vec<Handle>,
    trust: Vec<u8>,
    trust_got: usize,
    tls: Option<TlsConfig>,
    credential: Option<String>,
    host: String,
    port: u16,
    session: Option<Session>,
    broker: Option<Handle>,
    budget: u64,
    price_in: u64,
    price_out: u64,
    attempts: u32,
    spent: u64,
    asks: u32,
    refused: u32,
    connections: u32,
    tool_calls: u32,
}

impl Adapter {
    fn new(operator: Handle) -> Adapter {
        Adapter {
            operator,
            clients: Vec::new(),
            trust: Vec::new(),
            trust_got: 0,
            tls: None,
            credential: None,
            host: String::new(),
            port: 0,
            session: None,
            broker: None,
            budget: 0,
            price_in: 0,
            price_out: 0,
            attempts: 1,
            spent: 0,
            asks: 0,
            refused: 0,
            connections: 0,
            tool_calls: 0,
        }
    }

    // ---- operator ----------------------------------------------------------------

    fn stats(&self, reply: &mut OpReply) {
        reply.spent = self.spent;
        reply.budget = self.budget;
        reply.asks = self.asks;
        reply.refused = self.refused;
        reply.connections = self.connections;
        reply.tool_calls = self.tool_calls;
    }

    /// Serve one operator request; the flag says whether to quit.
    fn operator_request(&mut self, o: &Op, carried: Option<Handle>) -> (OpReply, bool) {
        let mut reply = OpReply::default();
        let mut kept = false;
        let outcome: Result<(), Error> = (|| {
            if o.abi_version != ABI_VERSION {
                return Err(Error::Invalid);
            }
            match o.kind {
                op::HELLO => reply.set_text(IDENTITY),
                op::TRUST => {
                    let (total, at, data) = (o.total as usize, o.offset as usize, o.data());
                    if total == 0 || total > TRUST_MAX || at.saturating_add(data.len()) > total {
                        return Err(Error::Invalid);
                    }
                    if at == 0 {
                        self.trust = vec![0; total];
                        self.trust_got = 0;
                        self.tls = None;
                    }
                    // In order, no gaps: the anchor is either whole or not in force.
                    if total != self.trust.len() || at != self.trust_got {
                        return Err(Error::Invalid);
                    }
                    self.trust[at..at + data.len()].copy_from_slice(data);
                    self.trust_got += data.len();
                    if self.trust_got == total {
                        match TlsConfig::with_roots(&[&self.trust]) {
                            Ok(c) => {
                                self.tls = Some(c);
                                println!("[cloud] trust anchor in force ({total} bytes)");
                            }
                            Err(e) => {
                                println!("[cloud] trust anchor refused: {e}");
                                return Err(Error::Invalid);
                            }
                        }
                    }
                }
                op::CREDENTIAL => {
                    // Visible ASCII only: it goes into a header line, and a line break
                    // in it would be a header of the caller's choosing.
                    let key = o.data();
                    if key.len() < 8 || !key.iter().all(|b| (0x21..=0x7e).contains(b)) {
                        return Err(Error::Invalid);
                    }
                    self.credential =
                        Some(String::from(core::str::from_utf8(key).map_err(|_| Error::Invalid)?));
                    println!("[cloud] credential held ({} bytes)", key.len());
                }
                op::ENDPOINT => {
                    let text = core::str::from_utf8(o.data()).map_err(|_| Error::Invalid)?;
                    let (host, port) = text.rsplit_once(':').ok_or(Error::Invalid)?;
                    let port: u16 = port.parse().map_err(|_| Error::Invalid)?;
                    if port == 0 || !dns::valid_name(host) {
                        return Err(Error::Invalid);
                    }
                    self.host = String::from(host);
                    self.port = port;
                    println!("[cloud] provider at {host}:{port}");
                }
                op::NET => {
                    let h = carried.ok_or(Error::Invalid)?;
                    kept = true;
                    if let Some(old) = self.session.replace(Session::new(h)) {
                        sys::handle_close(old.handle()).ok();
                    }
                }
                op::TOOLS => {
                    let h = carried.ok_or(Error::Invalid)?;
                    kept = true;
                    if let Some(old) = self.broker.replace(h) {
                        sys::handle_close(old).ok();
                    }
                }
                op::BUDGET => {
                    if o.attempts == 0 || o.attempts > ATTEMPTS_MAX {
                        return Err(Error::Invalid);
                    }
                    self.budget = o.budget;
                    self.price_in = o.price_in;
                    self.price_out = o.price_out;
                    self.attempts = o.attempts;
                    println!(
                        "[cloud] budget {} µ$ ({} spent); {} µ$ per million tokens in, {} out; {} attempt(s) per request",
                        o.budget, self.spent, o.price_in, o.price_out, o.attempts
                    );
                }
                op::CLIENT => {
                    let h = carried.ok_or(Error::Invalid)?;
                    if self.clients.len() >= CLIENTS_MAX {
                        return Err(Error::Busy);
                    }
                    kept = true;
                    self.clients.push(h);
                }
                op::STATS | op::QUIT => {}
                _ => return Err(Error::NoSys),
            }
            Ok(())
        })();
        if !kept && let Some(h) = carried {
            sys::handle_close(h).ok();
        }
        if let Err(e) = outcome {
            reply = OpReply::error(e);
        }
        self.stats(&mut reply);
        (reply, o.kind == op::QUIT && outcome.is_ok())
    }

    // ---- asks --------------------------------------------------------------------

    fn serve_ask(&mut self, client: Handle, a: &Ask) {
        self.asks += 1;
        let n = self.asks;
        let t0 = sys::ticks_ms();
        let deadline = t0 + u64::from(a.timeout_ms.min(TIMEOUT_MAX));
        let mut tally = Tally::default();
        let result = self.run(client, a, deadline, &mut tally);
        let mut last = match &result {
            Ok(()) => Event::new(event::DONE, 0, "done"),
            Err(f) => Event::new(event::ERROR, f.code, &f.text),
        };
        last.input_tokens = tally.input.min(u32::MAX as u64) as u32;
        last.output_tokens = tally.output.min(u32::MAX as u64) as u32;
        last.cost = tally.cost;
        last.attempts = tally.attempts;
        let _ = emit(client, &last, sys::ticks_ms() + FINAL_MS);
        let outcome = match &result {
            Ok(()) => String::from("done"),
            Err(f) => format!("{}: {}", fail::name(f.code), f.text),
        };
        println!(
            "[cloud] ask {n} ({}): {outcome}; {} ms, {} round(s), {} attempt(s), tools [{}], {} in / {} out tokens, {} µ$; spent {} of {} µ$",
            a.model(),
            sys::ticks_ms() - t0,
            tally.rounds,
            tally.attempts,
            tally.tools.join("; "),
            tally.input,
            tally.output,
            tally.cost,
            self.spent,
            self.budget
        );
    }

    fn run(&mut self, client: Handle, a: &Ask, deadline: u64, tally: &mut Tally) -> Result<(), Fail> {
        if a.abi_version != ABI_VERSION
            || a.model().is_empty()
            || a.max_tokens == 0
            || a.max_tokens > MAX_TOKENS_MAX
            || a.timeout_ms == 0
            || a.timeout_ms > TIMEOUT_MAX
        {
            return Err(Fail::new(fail::INVALID, "malformed ask"));
        }
        // First, before anything else can happen: work that must stay here, stays.
        if a.flags & flags::LOCAL_ONLY != 0 {
            self.refused += 1;
            return Err(Fail::new(
                fail::LOCAL_ONLY,
                "local-only work stays on this machine: nothing was sent",
            ));
        }
        if self.tls.is_none() || self.credential.is_none() || self.session.is_none() || self.host.is_empty() {
            return Err(Fail::new(
                fail::NOT_READY,
                "no trust anchor, credential, endpoint or network session yet",
            ));
        }
        let tools = a.flags & flags::TOOLS != 0;
        if tools && self.broker.is_none() {
            return Err(Fail::new(fail::TOOLS, "tool use was asked for, but no Tool Broker is attached"));
        }
        let mut messages = vec![json!({"role": "user", "content": a.prompt()})];
        for round in 0..TOOL_ROUNDS_MAX {
            let mut body =
                json!({"model": a.model(), "max_tokens": a.max_tokens, "stream": true, "messages": messages});
            if tools {
                body["tools"] = tool_definitions();
            }
            let body = serde_json::to_vec(&body)
                .map_err(|_| Fail::new(fail::INVALID, "the request would not encode"))?;
            // Reserve the worst case -- the whole request as input, every token the
            // answer may have as output -- before a byte is sent.
            let input_estimate = body.len().div_ceil(3) as u64 + 16;
            let worst = cost(input_estimate, u64::from(a.max_tokens), self.price_in, self.price_out);
            let remaining = self.budget.saturating_sub(self.spent);
            if worst > remaining {
                if round == 0 {
                    self.refused += 1;
                }
                return Err(Fail::new(
                    fail::BUDGET,
                    &format!(
                        "this request could cost up to {worst} µ$ and {remaining} µ$ of the budget is left: {}",
                        if round == 0 { "nothing was sent" } else { "stopped before asking again" }
                    ),
                ));
            }
            tally.rounds += 1;
            let answer = self.round(client, a, &body, deadline, tally)?;
            if !(tools && answer.stop_reason == "tool_use") {
                return Ok(());
            }
            let mut calls = Vec::new();
            let mut results = Vec::new();
            for b in &answer.blocks {
                match b {
                    Block::Text(t) if !t.is_empty() => calls.push(json!({"type": "text", "text": t})),
                    Block::Tool { id, name, input } => {
                        let parsed: Option<Value> =
                            if input.is_empty() { Some(json!({})) } else { parse_json(input.as_bytes()) };
                        let (is_error, content) = match &parsed {
                            // Every call gets its result, but only the first few are run.
                            _ if results.len() >= TOOL_CALLS_MAX => {
                                (true, format!("not run: at most {TOOL_CALLS_MAX} tool calls per turn"))
                            }
                            Some(v) => self.run_tool(client, name, v, deadline, tally)?,
                            None => (true, String::from("the tool input was not JSON")),
                        };
                        calls.push(json!({"type": "tool_use", "id": id, "name": name,
                            "input": parsed.unwrap_or(json!({}))}));
                        results.push(json!({"type": "tool_result", "tool_use_id": id,
                            "content": content, "is_error": is_error}));
                    }
                    _ => {}
                }
            }
            if results.is_empty() {
                return Err(Fail::new(
                    fail::PROTOCOL,
                    "the model stopped for tool use without calling a tool",
                ));
            }
            messages.push(json!({"role": "assistant", "content": calls}));
            messages.push(json!({"role": "user", "content": results}));
        }
        Err(Fail::new(
            fail::TOOLS,
            &format!("the model was still calling tools after {TOOL_ROUNDS_MAX} requests"),
        ))
    }

    /// One request, tried again while the provider is busy and nothing has reached
    /// the client, at most the configured number of times.
    fn round(
        &mut self,
        client: Handle,
        a: &Ask,
        body: &[u8],
        deadline: u64,
        tally: &mut Tally,
    ) -> Result<Answer, Fail> {
        let mut attempt = 0;
        loop {
            attempt += 1;
            tally.attempts += 1;
            match self.exchange(client, a, body, attempt, deadline, tally) {
                Ok(answer) => return Ok(answer),
                Err(f) if f.retry && attempt < self.attempts => {
                    let pause = (250u64 << (attempt - 1)).min(2000);
                    if sys::ticks_ms() + pause >= deadline {
                        return Err(Fail::new(
                            fail::TIMEOUT,
                            &format!("{} -- and no time left to try again", f.text),
                        ));
                    }
                    println!("[cloud] attempt {attempt}: {}; trying again in {pause} ms", f.text);
                    sys::sleep_ms(pause);
                }
                Err(mut f) => {
                    if f.retry {
                        f.code = fail::OVERLOADED;
                        f.text = format!("{} on every one of {attempt} attempt(s)", f.text);
                    }
                    return Err(f);
                }
            }
        }
    }

    /// One HTTP request and its answer, streamed to the client as it arrives. What
    /// the provider reports having used is charged whatever happens.
    fn exchange(
        &mut self,
        client: Handle,
        a: &Ask,
        body: &[u8],
        attempt: u32,
        deadline: u64,
        tally: &mut Tally,
    ) -> Result<Answer, Fail> {
        let (Some(cfg), Some(session), Some(key)) = (&self.tls, &self.session, &self.credential) else {
            return Err(Fail::new(fail::NOT_READY, "not configured"));
        };
        let t = left(deadline)?.min(u64::from(u32::MAX)) as u32;
        let tls = TlsStream::connect(session, &self.host, self.port, cfg, t)
            .map_err(|e| http::tls_fail(e, "connecting to the provider"))?;
        let head = format!(
            "POST /v1/messages HTTP/1.1\r\nhost: {}\r\nx-api-key: {key}\r\nanthropic-version: {API_VERSION}\r\n\
             content-type: application/json\r\ncontent-length: {}\r\nx-spacecloud-attempt: {attempt}\r\n\
             connection: close\r\n\r\n",
            self.host,
            body.len()
        );
        self.connections += 1;
        let mut conn = Conn::new(tls, deadline);
        conn.send(&head, body)?;
        let mut head = conn.head()?;
        if head.status != 200 {
            let raw = conn.small_body(&mut head, ERROR_BODY_MAX).unwrap_or_default();
            let detail = parse_json(&raw)
                .and_then(|v| v["error"]["message"].as_str().map(String::from))
                .unwrap_or_default();
            let status = head.status;
            return Err(match status {
                401 | 403 => Fail::new(
                    fail::AUTH,
                    &format!("the provider refused the credential ({status}: {detail})"),
                ),
                429 | 500 | 502 | 503 | 529 => {
                    Fail::busy(fail::OVERLOADED, &format!("the provider is busy ({status}: {detail})"))
                }
                _ => Fail::new(fail::PROVIDER, &format!("the provider answered {status}: {detail}")),
            });
        }
        if !head.content_type.starts_with("text/event-stream") {
            return Err(Fail::new(fail::PROTOCOL, "the answer is not an event stream"));
        }
        let mut parser = sse::Parser::default();
        let mut answer = Answer::default();
        let mut piece = Vec::new();
        let result = loop {
            piece.clear();
            match conn.body(&mut head, &mut piece) {
                Ok(true) => {
                    let fed = parser.feed(&piece, &mut |name, data| {
                        self.on_event(client, a, name, data, &mut answer, deadline)
                    });
                    if let Err(f) = fed {
                        break Err(f);
                    }
                    if answer.stopped {
                        break Ok(());
                    }
                }
                Ok(false) => {
                    break if answer.stopped {
                        Ok(())
                    } else {
                        Err(Fail::new(fail::PROTOCOL, "the answer ended before message_stop"))
                    };
                }
                Err(f) => break Err(f),
            }
        };
        let used = cost(answer.input, answer.output, self.price_in, self.price_out);
        self.spent = self.spent.saturating_add(used);
        tally.input += answer.input;
        tally.output += answer.output;
        tally.cost += used;
        match result {
            Ok(()) => {
                conn.close();
                Ok(answer)
            }
            // Dropping the connection closes it: a stream cut off stays cut off.
            Err(mut f) => {
                if answer.streamed {
                    f.retry = false;
                }
                Err(f)
            }
        }
    }

    /// The provider held to what was reserved for this answer, and to the budget.
    fn guard(&self, a: &Ask, answer: &Answer) -> Result<(), Fail> {
        if answer.output > u64::from(a.max_tokens) {
            return Err(Fail::new(
                fail::OVER_BUDGET,
                &format!(
                    "the provider reported {} output tokens, past the {} reserved: cut off",
                    answer.output, a.max_tokens
                ),
            ));
        }
        let running = cost(answer.input, answer.output, self.price_in, self.price_out);
        let remaining = self.budget.saturating_sub(self.spent);
        if running > remaining {
            return Err(Fail::new(
                fail::OVER_BUDGET,
                &format!("this answer has cost {running} µ$, past the {remaining} µ$ left: cut off"),
            ));
        }
        Ok(())
    }

    fn on_event(
        &mut self,
        client: Handle,
        a: &Ask,
        name: &str,
        data: &str,
        answer: &mut Answer,
        deadline: u64,
    ) -> Result<(), Fail> {
        let v: Value = parse_json(data.as_bytes()).ok_or_else(|| {
            Fail::new(fail::PROTOCOL, &format!("event {name:.32} is not JSON the adapter accepts"))
        })?;
        match name {
            "message_start" => {
                let usage = &v["message"]["usage"];
                answer.input = usage["input_tokens"].as_u64().unwrap_or(0);
                answer.output = usage["output_tokens"].as_u64().unwrap_or(0);
                self.guard(a, answer)?;
            }
            "content_block_start" => {
                if answer.blocks.len() >= BLOCKS_MAX {
                    return Err(Fail::new(fail::PROTOCOL, "more content blocks than one answer may have"));
                }
                let b = &v["content_block"];
                answer.blocks.push(match b["type"].as_str() {
                    Some("text") => Block::Text(String::from(b["text"].as_str().unwrap_or(""))),
                    Some("tool_use") => Block::Tool {
                        id: String::from(b["id"].as_str().unwrap_or("")),
                        name: String::from(b["name"].as_str().unwrap_or("")),
                        input: String::new(),
                    },
                    _ => Block::Other,
                });
            }
            "content_block_delta" => {
                let index = v["index"].as_u64().unwrap_or(u64::MAX) as usize;
                let d = &v["delta"];
                match (d["type"].as_str(), answer.blocks.get_mut(index)) {
                    (Some("text_delta"), Some(Block::Text(t))) => {
                        let text = d["text"].as_str().unwrap_or("");
                        t.push_str(text);
                        answer.text_bytes += text.len();
                        if answer.text_bytes > a.max_tokens as usize * BYTES_PER_TOKEN_MAX + 1024 {
                            return Err(Fail::new(
                                fail::OVER_BUDGET,
                                &format!(
                                    "{} bytes of answer is more than {} tokens can be: cut off",
                                    answer.text_bytes, a.max_tokens
                                ),
                            ));
                        }
                        answer.streamed = true;
                        let mut rest = text;
                        while !rest.is_empty() {
                            let piece = cut(rest, TEXT_MAX);
                            let piece = if piece.is_empty() { rest } else { piece };
                            emit(client, &Event::new(event::DELTA, 0, piece), deadline)?;
                            rest = &rest[piece.len()..];
                        }
                    }
                    (Some("input_json_delta"), Some(Block::Tool { input, .. })) => {
                        let part = d["partial_json"].as_str().unwrap_or("");
                        if input.len() + part.len() > TOOL_INPUT_MAX {
                            return Err(Fail::new(fail::PROTOCOL, "a tool call's input is too long"));
                        }
                        input.push_str(part);
                    }
                    _ => {}
                }
            }
            "message_delta" => {
                if let Some(r) = v["delta"]["stop_reason"].as_str() {
                    answer.stop_reason = String::from(r);
                }
                if let Some(out) = v["usage"]["output_tokens"].as_u64() {
                    answer.output = answer.output.max(out);
                }
                self.guard(a, answer)?;
            }
            "message_stop" => answer.stopped = true,
            "error" => {
                let kind = v["error"]["type"].as_str().unwrap_or("error");
                let message = v["error"]["message"].as_str().unwrap_or("");
                return Err(if kind == "overloaded_error" && !answer.streamed {
                    Fail::busy(fail::OVERLOADED, &format!("the provider is overloaded ({message})"))
                } else {
                    Fail::new(fail::PROVIDER, &format!("the provider stopped with {kind}: {message}"))
                });
            }
            // `ping`, and whatever a newer provider sends that this one does not know.
            _ => {}
        }
        Ok(())
    }

    // ---- tools -------------------------------------------------------------------

    fn broker_call(&self, r: &ToolRequest) -> Result<ToolReply, Fail> {
        let b = self.broker.ok_or_else(|| Fail::new(fail::TOOLS, "no Tool Broker is attached"))?;
        sys::send(b, as_bytes(r), None)
            .map_err(|e| Fail::new(fail::TOOLS, &format!("the Tool Broker: {e}")))?;
        sys::wait_any(&[b], BROKER_MS)
            .map_err(|_| Fail::new(fail::TOOLS, "the Tool Broker did not answer"))?;
        let mut buf = [0u8; core::mem::size_of::<ToolReply>()];
        let (n, carried) = sys::recv(b, &mut buf, true)
            .map_err(|e| Fail::new(fail::TOOLS, &format!("the Tool Broker: {e}")))?;
        if let Some(h) = carried {
            sys::handle_close(h).ok();
        }
        if n != buf.len() {
            return Err(Fail::new(fail::TOOLS, "a malformed answer from the Tool Broker"));
        }
        // SAFETY: exact size; every bit pattern is a valid ToolReply.
        Ok(unsafe { core::ptr::read_unaligned(buf.as_ptr() as *const ToolReply) })
    }

    /// Run one tool call through the broker; returns (is_error, content for the
    /// model). The client is told about the call and what the broker decided.
    fn run_tool(
        &mut self,
        client: Handle,
        name: &str,
        input: &Value,
        deadline: u64,
        tally: &mut Tally,
    ) -> Result<(bool, String), Fail> {
        self.tool_calls += 1;
        let path = input["path"].as_str().unwrap_or("");
        let (verdict_code, is_error, content) = if name != "read_file" {
            (verdict::DENIED_TOOL, true, format!("there is no tool named {name}"))
        } else if path.is_empty() || path.len() > agent::PATH_MAX {
            (verdict::DENIED_SCOPE, true, String::from("read_file needs a path of at most 48 bytes"))
        } else {
            let mut data = Vec::new();
            let mut outcome = (verdict::ALLOWED, false, String::new());
            loop {
                let mut r = ToolRequest::new(tool::READ, path);
                r.offset = data.len() as u32;
                r.len = CHUNK_MAX as u32;
                let reply = self.broker_call(&r)?;
                if reply.verdict != verdict::ALLOWED {
                    outcome = match reply.verdict {
                        verdict::DENIED_SCOPE => (
                            reply.verdict,
                            true,
                            format!("refused by the Tool Broker: {path} is outside the workspace"),
                        ),
                        v => {
                            (v, true, format!("the Tool Broker could not read {path}: {:?}", reply.result()))
                        }
                    };
                    break;
                }
                data.extend_from_slice(reply.data());
                if (reply.len as usize) < CHUNK_MAX || data.len() >= TOOL_OUTPUT_MAX {
                    break;
                }
            }
            if !outcome.1 {
                data.truncate(TOOL_OUTPUT_MAX);
                outcome.2 = String::from_utf8_lossy(&data).to_string();
            }
            outcome
        };
        let word = match verdict_code {
            verdict::ALLOWED => "allowed",
            verdict::DENIED_SCOPE => "denied (outside the workspace)",
            verdict::DENIED_TOOL => "denied (no such tool)",
            _ => "failed",
        };
        let summary = format!("{name} {path}: {word}");
        emit(client, &Event::new(event::TOOL, verdict_code, &summary), deadline)?;
        tally.tools.push(summary);
        Ok((is_error, content))
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn space_main() -> i32 {
    libspace::heap::set_pages(HEAP_PAGES);
    println!("[cloud] {IDENTITY}");
    let mut ad = Adapter::new(handle::BOOTSTRAP);
    let mut buf = [0u8; MSG_MAX];
    loop {
        let mut waits = Vec::with_capacity(1 + ad.clients.len());
        waits.push(ad.operator);
        waits.extend_from_slice(&ad.clients);
        let ready = match sys::wait_any(&waits, WAIT_FOREVER) {
            Ok(i) => i,
            Err(e) => {
                println!("[cloud] wait failed: {e}");
                break;
            }
        };
        let from = waits[ready];
        let (n, carried) = match sys::recv(from, &mut buf, true) {
            Ok(m) => m,
            Err(Error::WouldBlock) => continue,
            Err(_) if from == ad.operator => break,
            Err(_) => {
                ad.clients.retain(|&c| c != from);
                sys::handle_close(from).ok();
                continue;
            }
        };
        if from == ad.operator {
            let Some(o) = Op::from_bytes(&buf[..n]) else {
                if let Some(h) = carried {
                    sys::handle_close(h).ok();
                }
                let _ = sys::send(ad.operator, OpReply::error(Error::MsgSize).as_bytes(), None);
                continue;
            };
            let (reply, quit) = ad.operator_request(&o, carried);
            let _ = sys::send(ad.operator, reply.as_bytes(), None);
            if quit {
                break;
            }
        } else {
            if let Some(h) = carried {
                sys::handle_close(h).ok();
            }
            match Ask::from_bytes(&buf[..n]) {
                Some(a) if a.kind == ask::ASK => ad.serve_ask(from, &a),
                _ => {
                    let _ = sys::send(
                        from,
                        Event::new(event::ERROR, fail::INVALID, "not an ask").as_bytes(),
                        None,
                    );
                }
            }
        }
    }
    for h in ad.clients.drain(..).chain(ad.broker.take()).chain(ad.session.take().map(|s| s.handle())) {
        sys::handle_close(h).ok();
    }
    println!(
        "[cloud] closing: {} ask(s), {} refused before sending, {} connection(s), {} tool call(s); spent {} of {} µ$",
        ad.asks, ad.refused, ad.connections, ad.tool_calls, ad.spent, ad.budget
    );
    0
}
