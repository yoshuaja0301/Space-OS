//! Cloud adapter checks (I01, ADR-0018): `spacecloud` against the lab's mock
//! provider at api.cloud.test (xtask/src/cloud.rs), over TLS, with the Tool Broker
//! between the model's tool calls and the machine. `init` is the operator and the
//! client. The provider is a mock, and every check here is about the adapter: what
//! it sends, what it refuses to send, what it charges, and when it stops.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use libspace::spaceabi::agent::{ABI_VERSION as AGENT_ABI, ToolRequest, tool, verdict};
use libspace::spaceabi::cloud::{self, Ask, Event, OP_DATA_MAX, Op, OpReply, event, fail, flags, op};
use libspace::spaceabi::handle::rights;
use libspace::spaceabi::net::{NetRequest, req};
use libspace::{Handle, println, sys};

use crate::nettest::{self, Lab, op_call};
use crate::{
    as_tool_bytes, audit_contains, broker_hello, broker_quit, broker_reply, read_audit, stream_file,
};

const HOST: &str = "api.cloud.test";
const PORT: u16 = 443;
/// TLS, JSON and the heap: about 1.3 MB of code and a 1 MiB heap.
const CLOUD_QUOTA: u64 = 1536;
const BROKER_QUOTA: u64 = 256;
/// The lab provider's prices (µ$ per million tokens), the budget, and attempts per
/// request.
const PRICE_IN: u64 = 3_000_000;
const PRICE_OUT: u64 = 15_000_000;
const BUDGET: u64 = 50_000;
const ATTEMPTS: u32 = 3;
const CA_PATH: &str = "/spaceos/tls/labca.der";
const KEY_PATH: &str = "/spaceos/cred/cloud.key";

/// The running adapter, its broker, and what the checks hold of them.
pub struct Cloud {
    process: Handle,
    op: Handle,
    client: Handle,
    broker: Handle,
    broker_op: Handle,
    /// The key from the credential store: checks need it to see where it went.
    key: String,
}

fn read_all(path: &str) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    stream_file(path, |chunk| out.extend_from_slice(chunk))?;
    Ok(out)
}

fn cloud_op(ch: Handle, o: &Op, carry: Option<Handle>) -> Result<OpReply, String> {
    sys::send(ch, o.as_bytes(), carry).map_err(|e| format!("send op {}: {e}", o.kind))?;
    sys::wait_any(&[ch], 5000).map_err(|e| format!("op {}: no answer: {e}", o.kind))?;
    let mut buf = [0u8; core::mem::size_of::<OpReply>()];
    let (n, carried) = sys::recv(ch, &mut buf, true).map_err(|e| format!("op {}: {e}", o.kind))?;
    if let Some(h) = carried {
        sys::handle_close(h).ok();
    }
    let reply = OpReply::from_bytes(&buf[..n]).ok_or_else(|| format!("op {}: a {n}-byte answer", o.kind))?;
    reply.result().map_err(|e| format!("op {}: {e}", o.kind))?;
    Ok(reply)
}

fn stats(c: &Cloud) -> Result<OpReply, String> {
    cloud_op(c.op, &Op::new(op::STATS), None)
}

/// Start the broker and the adapter and configure it, all but the credential.
pub fn start(root: Handle, lab: &Lab) -> Result<Cloud, String> {
    let ca = read_all(CA_PATH)?;
    let key =
        String::from_utf8(read_all(KEY_PATH)?).map_err(|_| String::from("the credential is not text"))?;
    let key = String::from(key.trim());

    // The broker, holding the only file capability, attached to the channel the
    // adapter's tool calls will come through.
    let (broker_op, theirs) = sys::channel_create().map_err(|e| format!("channel: {e}"))?;
    let broker = sys::spawn(root, "bin/spacebroker", BROKER_QUOTA, Some(theirs)).map_err(|e| {
        sys::handle_close(broker_op).ok();
        format!("spawn broker: {e}")
    })?;
    let (op_mine, op_theirs) = match sys::channel_create() {
        Ok(c) => c,
        Err(e) => {
            abandon_broker(broker, broker_op);
            return Err(format!("channel: {e}"));
        }
    };
    let process = match sys::spawn(root, "bin/spacecloud", CLOUD_QUOTA, Some(op_theirs)) {
        Ok(p) => p,
        Err(e) => {
            sys::handle_close(op_mine).ok();
            abandon_broker(broker, broker_op);
            return Err(format!("spawn: {e}"));
        }
    };
    let (client, client_theirs) = match sys::channel_create() {
        Ok(c) => c,
        Err(e) => {
            sys::kill(process).ok();
            sys::wait(process).ok();
            sys::handle_close(process).ok();
            sys::handle_close(op_mine).ok();
            abandon_broker(broker, broker_op);
            return Err(format!("channel: {e}"));
        }
    };
    let c = Cloud { process, op: op_mine, client, broker, broker_op, key };
    let configured = (|| {
        let hello = match cloud_op(c.op, &Op::new(op::HELLO), None) {
            Ok(h) => h,
            Err(e) => {
                sys::handle_close(client_theirs).ok();
                return Err(e);
            }
        };
        println!("[init] cloud adapter: {}", hello.text());
        cloud_op(c.op, &Op::new(op::CLIENT), Some(client_theirs))?;
        let files =
            sys::handle_dup(root, rights::FS | rights::TRANSFER).map_err(|e| format!("dup root: {e}"))?;
        broker_hello(c.broker_op, files)?;
        let (broker_side, adapter_side) = sys::channel_create().map_err(|e| format!("channel: {e}"))?;
        let mut attach = ToolRequest::new(tool::ATTACH, "-");
        attach.abi_version = AGENT_ABI;
        // The broker answers this only once the adapter is done with it.
        if let Err(e) = sys::send(c.broker_op, as_tool_bytes(&attach), Some(broker_side)) {
            sys::handle_close(adapter_side).ok();
            return Err(format!("attach: {e}"));
        }

        for (i, piece) in ca.chunks(OP_DATA_MAX).enumerate() {
            let mut t = Op::with_data(op::TRUST, piece);
            t.offset = (i * OP_DATA_MAX) as u32;
            t.total = ca.len() as u32;
            cloud_op(c.op, &t, None)?;
        }
        cloud_op(c.op, &Op::with_data(op::ENDPOINT, format!("{HOST}:{PORT}").as_bytes()), None)?;
        // A session that reaches the provider's one address and port: nothing else.
        let (session, session_theirs) = sys::channel_create().map_err(|e| format!("channel: {e}"))?;
        let opened = op_call(lab.op, &NetRequest::new(req::SESSION), Some(session_theirs), 2000)
            .and_then(|_| op_call(lab.op, &NetRequest::with_host(req::ALLOW, HOST, PORT), None, 2000));
        if let Err(e) = opened {
            sys::handle_close(session).ok();
            sys::handle_close(adapter_side).ok();
            return Err(e);
        }
        cloud_op(c.op, &Op::new(op::NET), Some(session))?;
        cloud_op(c.op, &Op::new(op::TOOLS), Some(adapter_side))?;
        let mut budget = Op::new(op::BUDGET);
        budget.budget = BUDGET;
        budget.price_in = PRICE_IN;
        budget.price_out = PRICE_OUT;
        budget.attempts = ATTEMPTS;
        cloud_op(c.op, &budget, None)?;
        Ok(())
    })();
    match configured {
        Ok(()) => Ok(c),
        Err(e) => {
            abandon(c);
            Err(e)
        }
    }
}

fn abandon_broker(broker: Handle, broker_op: Handle) {
    sys::kill(broker).ok();
    sys::wait(broker).ok();
    sys::handle_close(broker).ok();
    sys::handle_close(broker_op).ok();
}

/// Stop everything without asking: for a check that failed half-way.
pub fn abandon(c: Cloud) {
    sys::kill(c.process).ok();
    sys::wait(c.process).ok();
    for h in [c.process, c.op, c.client] {
        sys::handle_close(h).ok();
    }
    abandon_broker(c.broker, c.broker_op);
}

/// Everything one ask produced.
struct Outcome {
    text: String,
    tools: Vec<(u32, String)>,
    deltas: usize,
    first_delta_ms: Option<u64>,
    end_ms: u64,
    last: Event,
}

impl Outcome {
    fn describe(&self) -> String {
        match self.last.kind {
            event::DONE => format!("done: {:?}", self.text),
            _ => format!("{}: {}", fail::name(self.last.code), self.last.text()),
        }
    }

    fn expect_done(&self) -> Result<(), String> {
        if self.last.kind == event::DONE {
            Ok(())
        } else {
            Err(format!("the ask failed: {}", self.describe()))
        }
    }

    fn expect_fail(&self, code: u32) -> Result<(), String> {
        if self.last.kind == event::ERROR && self.last.code == code {
            Ok(())
        } else {
            Err(format!("expected {}, the ask ended {}", fail::name(code), self.describe()))
        }
    }
}

fn ask(c: &Cloud, a: &Ask, wait_ms: u64) -> Result<Outcome, String> {
    let t0 = sys::ticks_ms();
    sys::send(c.client, a.as_bytes(), None).map_err(|e| format!("send ask: {e}"))?;
    let mut o = Outcome {
        text: String::new(),
        tools: Vec::new(),
        deltas: 0,
        first_delta_ms: None,
        end_ms: 0,
        last: Event::new(0, 0, ""),
    };
    let mut buf = [0u8; core::mem::size_of::<Event>()];
    loop {
        let left = (t0 + wait_ms).saturating_sub(sys::ticks_ms());
        if left == 0 {
            return Err(format!("the answer to {} did not end within {wait_ms} ms", a.model()));
        }
        sys::wait_any(&[c.client], left).map_err(|e| format!("waiting for the answer: {e}"))?;
        let (n, carried) = match sys::recv(c.client, &mut buf, true) {
            Ok(m) => m,
            Err(libspace::spaceabi::error::Error::WouldBlock) => continue,
            Err(e) => return Err(format!("reading the answer: {e}")),
        };
        if let Some(h) = carried {
            sys::handle_close(h).ok();
        }
        let e = Event::from_bytes(&buf[..n]).ok_or_else(|| format!("a {n}-byte event"))?;
        match e.kind {
            event::DELTA => {
                o.first_delta_ms.get_or_insert(sys::ticks_ms() - t0);
                o.deltas += 1;
                o.text.push_str(e.text());
            }
            event::TOOL => o.tools.push((e.code, String::from(e.text()))),
            event::DONE | event::ERROR => {
                o.end_ms = sys::ticks_ms() - t0;
                o.last = e;
                return Ok(o);
            }
            k => return Err(format!("an event of kind {k}")),
        }
    }
}

/// Before a credential: not ready. A wrong one: refused by the provider, and not
/// retried. The right one: an answer.
pub fn auth(c: &Cloud) -> Result<(), String> {
    ask(c, &Ask::new("lab-echo", "hello", 0, 64, 5000), 8000)?.expect_fail(fail::NOT_READY)?;
    let mut wrong = c.key.clone();
    let last = wrong.pop().unwrap_or('0');
    wrong.push(if last == '0' { '1' } else { '0' });
    cloud_op(c.op, &Op::with_data(op::CREDENTIAL, wrong.as_bytes()), None)?;
    let refused = ask(c, &Ask::new("lab-echo", "hello", 0, 64, 10_000), 15_000)?;
    refused.expect_fail(fail::AUTH)?;
    println!("[init] cloud: with a wrong key: {}", refused.describe());
    if refused.last.attempts != 1 {
        return Err(format!("a refused credential was tried {} times", refused.last.attempts));
    }
    cloud_op(c.op, &Op::with_data(op::CREDENTIAL, c.key.as_bytes()), None)?;
    let answered = ask(c, &Ask::new("lab-echo", "hello", 0, 64, 10_000), 15_000)?;
    answered.expect_done()?;
    if !answered.text.starts_with("You said: hello") {
        return Err(format!("the answer was {:?}", answered.text));
    }
    Ok(())
}

/// The answer arrives in pieces as the provider writes them -- 150 ms apart -- and
/// what it cost is what the provider reported, at the configured prices.
pub fn streaming(c: &Cloud) -> Result<(), String> {
    let before = stats(c)?.spent;
    let prompt = "stream this back";
    let o = ask(c, &Ask::new("lab-echo", prompt, 0, 256, 10_000), 15_000)?;
    o.expect_done()?;
    let want = format!("You said: {prompt} -- this answer came in four pieces.");
    if o.text != want {
        return Err(format!("the answer was {:?}, expected {want:?}", o.text));
    }
    let first = o.first_delta_ms.unwrap_or(o.end_ms);
    println!(
        "[init] cloud: {} pieces, the first after {first} ms and the end after {} ms; {} in / {} out tokens, {} µ$",
        o.deltas, o.end_ms, o.last.input_tokens, o.last.output_tokens, o.last.cost
    );
    if o.deltas < 4 {
        return Err(format!("{} pieces arrived; the provider sent 4", o.deltas));
    }
    // An answer held back until it is complete arrives all at once: 0 ms between the
    // first piece and the end. Streamed, the pieces are three gaps of 150 ms apart.
    // One gap is the line: a busy host can hold the guest still long enough to bunch
    // two pieces together (286 ms once, on a machine sharing its CPUs with three
    // others), but not to fold three gaps into less than one.
    if o.end_ms - first < 150 {
        return Err(format!(
            "the first piece came only {} ms before the end: the answer was held back",
            o.end_ms - first
        ));
    }
    let (inp, out) = (u64::from(o.last.input_tokens), u64::from(o.last.output_tokens));
    if inp == 0 || out == 0 || o.last.cost != cloud::cost(inp, out, PRICE_IN, PRICE_OUT) {
        return Err(format!("{inp} in / {out} out tokens were charged {} µ$", o.last.cost));
    }
    let after = stats(c)?.spent;
    if after != before + o.last.cost {
        return Err(format!("spend went {before} -> {after} µ$ for an answer that cost {} µ$", o.last.cost));
    }
    Ok(())
}

/// The model reads a workspace file through the broker and answers with it; with no
/// tools offered it can call none.
pub fn tool_use(c: &Cloud) -> Result<(), String> {
    let o = ask(c, &Ask::new("lab-tool", "what does my input file say?", flags::TOOLS, 256, 15_000), 20_000)?;
    o.expect_done()?;
    println!("[init] cloud: tool calls {:?}; answer {:?}", o.tools, o.text);
    if o.tools != [(verdict::ALLOWED, String::from("read_file /spaceos/ws/input.txt: allowed"))] {
        return Err(format!("tool calls were {:?}", o.tools));
    }
    if !o.text.contains("The file begins: space-os workspace") {
        return Err(format!("the answer does not quote the file: {:?}", o.text));
    }
    if o.last.attempts != 2 {
        return Err(format!("{} requests for one tool call and its answer", o.last.attempts));
    }
    let none = ask(c, &Ask::new("lab-tool", "what does my input file say?", 0, 256, 10_000), 15_000)?;
    none.expect_done()?;
    if !none.tools.is_empty() || none.text != "No tool was offered." {
        return Err(format!("without tools: calls {:?}, answer {:?}", none.tools, none.text));
    }
    Ok(())
}

/// A model that tries to read the credential through a tool is refused by the
/// broker, and what reaches the provider is the refusal -- not the key.
pub fn tool_refused(c: &Cloud) -> Result<(), String> {
    let o = ask(c, &Ask::new("lab-exfil", "what is in the key file?", flags::TOOLS, 256, 15_000), 20_000)?;
    o.expect_done()?;
    println!("[init] cloud: tool calls {:?}; answer {:?}", o.tools, o.text);
    let want = (
        verdict::DENIED_SCOPE,
        String::from("read_file /spaceos/cred/cloud.key: denied (outside the workspace)"),
    );
    if o.tools != [want] {
        return Err(format!("tool calls were {:?}", o.tools));
    }
    if o.text.contains(c.key.as_str()) {
        return Err(String::from("the credential came back in the model's answer"));
    }
    if !o.text.contains("refused by the Tool Broker") {
        return Err(format!("the model was not told the call was refused: {:?}", o.text));
    }
    Ok(())
}

/// A provider that stops mid-answer is given up on at the deadline, and the adapter
/// serves the next ask.
pub fn timeout(c: &Cloud) -> Result<(), String> {
    let o = ask(c, &Ask::new("lab-stall", "wait for me", 0, 64, 1500), 8000)?;
    o.expect_fail(fail::TIMEOUT)?;
    println!("[init] cloud: a stalled answer ended after {} ms: {}", o.end_ms, o.describe());
    if !(1400..4000).contains(&o.end_ms) {
        return Err(format!("a 1500 ms deadline ended the ask after {} ms", o.end_ms));
    }
    ask(c, &Ask::new("lab-echo", "still there?", 0, 64, 10_000), 15_000)?.expect_done()
}

/// A provider sending JSON nested deep enough to exhaust a parser's stack gets a
/// protocol error back, and the adapter lives on to answer the next ask.
pub fn hostile(c: &Cloud) -> Result<(), String> {
    let o = ask(c, &Ask::new("lab-hostile", "anything", 0, 64, 10_000), 15_000)?;
    o.expect_fail(fail::PROTOCOL)?;
    println!("[init] cloud: {}", o.describe());
    ask(c, &Ask::new("lab-echo", "still alive?", 0, 64, 10_000), 15_000)?.expect_done()
}

/// A provider that is always overloaded is tried the configured number of times,
/// and no more.
pub fn retries(c: &Cloud) -> Result<(), String> {
    let before = stats(c)?.connections;
    let o = ask(c, &Ask::new("lab-overloaded", "anyone there?", 0, 64, 15_000), 20_000)?;
    o.expect_fail(fail::OVERLOADED)?;
    let after = stats(c)?.connections;
    println!("[init] cloud: {} attempts, {} connections: {}", o.last.attempts, after - before, o.describe());
    if o.last.attempts != ATTEMPTS || after - before != ATTEMPTS {
        return Err(format!(
            "{} attempts over {} connections, expected {ATTEMPTS}",
            o.last.attempts,
            after - before
        ));
    }
    Ok(())
}

/// Frames the device sent while `f` ran, after letting earlier traffic settle.
/// An ask whose worst case does not fit the budget left is refused before anything
/// is sent: no connection, not one frame.
pub fn budget_refusal(c: &Cloud, lab: &Lab) -> Result<(), String> {
    // 4000 tokens at $15 per million could cost 60000 µ$: more than the whole budget.
    let ((o, before, after), _) = nettest::sends_nothing(lab, || {
        let before = stats(c)?;
        let o = ask(c, &Ask::new("lab-budget", "an expensive question", 0, 4000, 5000), 8000)?;
        Ok((o, before, stats(c)?))
    })
    .map_err(|e| format!("a refused ask: {e}"))?;
    o.expect_fail(fail::BUDGET)?;
    println!("[init] cloud: {}; no frame sent meanwhile", o.describe());
    if after.connections != before.connections || o.last.attempts != 0 {
        return Err(format!("{} connection(s) for a refused ask", after.connections - before.connections));
    }
    if after.spent != before.spent || after.refused != before.refused + 1 {
        return Err(format!(
            "spend {} -> {}, refusals {} -> {}",
            before.spent, after.spent, before.refused, after.refused
        ));
    }
    Ok(())
}

/// A provider that streams on past the tokens reserved is cut off at once, and only
/// what it reported using is charged.
pub fn over_budget(c: &Cloud) -> Result<(), String> {
    let before = stats(c)?.spent;
    let o = ask(c, &Ask::new("lab-runaway", "go on", 0, 100, 15_000), 20_000)?;
    o.expect_fail(fail::OVER_BUDGET)?;
    let after = stats(c)?.spent;
    println!("[init] cloud: {} after {} ms; charged {} µ$", o.describe(), o.end_ms, o.last.cost);
    let (inp, out) = (u64::from(o.last.input_tokens), u64::from(o.last.output_tokens));
    if out != 400
        || o.last.cost != cloud::cost(inp, out, PRICE_IN, PRICE_OUT)
        || after != before + o.last.cost
    {
        return Err(format!(
            "cut at {out} output tokens, charged {} µ$, spend {before} -> {after}",
            o.last.cost
        ));
    }
    Ok(())
}

/// Local-only work is refused before it could leave: nothing sent, whatever else
/// the ask says.
pub fn local_only(c: &Cloud, lab: &Lab) -> Result<(), String> {
    let a = Ask::new("lab-local", "my private notes", flags::LOCAL_ONLY | flags::TOOLS, 64, 5000);
    let ((o, before, after), _) = nettest::sends_nothing(lab, || {
        let before = stats(c)?;
        let o = ask(c, &a, 8000)?;
        Ok((o, before, stats(c)?))
    })
    .map_err(|e| format!("local-only work: {e}"))?;
    o.expect_fail(fail::LOCAL_ONLY)?;
    println!("[init] cloud: {}; no frame sent meanwhile", o.describe());
    if after.connections != before.connections || after.refused != before.refused + 1 {
        return Err(format!("{} connection(s) for local-only work", after.connections - before.connections));
    }
    Ok(())
}

/// The adapter quits cleanly, and the broker's audit holds both tool calls with
/// the verdicts the model was told.
pub fn finish(c: Cloud) -> Result<(), String> {
    let last = cloud_op(c.op, &Op::new(op::QUIT), None);
    // An adapter that did not answer QUIT may never exit on its own.
    if last.is_err() {
        sys::kill(c.process).ok();
    }
    let st = sys::wait(c.process);
    for h in [c.process, c.op, c.client] {
        sys::handle_close(h).ok();
    }
    let checked = (|| {
        let last = last?;
        println!(
            "[init] cloud: {} asks, {} refused before sending, {} connections, {} tool calls; spent {} of {} µ$",
            last.asks, last.refused, last.connections, last.tool_calls, last.spent, last.budget
        );
        match st {
            Ok(st) if st.is_exited_with(0) => {}
            other => return Err(format!("the adapter ended with {other:?}")),
        }
        if last.spent > last.budget {
            return Err(format!("{} µ$ spent of a {} µ$ budget", last.spent, last.budget));
        }
        // The adapter is gone, so the broker has finished serving it.
        broker_reply(c.broker_op)?.result().map_err(|e| format!("attach: {e}"))?;
        let (entries, allowed, denied) = read_audit(c.broker_op)?;
        println!("[init] cloud: broker audit: {entries} entries, {allowed} allowed, {denied} refused");
        if !audit_contains(c.broker_op, entries, verdict::ALLOWED, "read /spaceos/ws/input.txt")? {
            return Err(String::from("the audit does not record the workspace read"));
        }
        if !audit_contains(c.broker_op, entries, verdict::DENIED_SCOPE, "read /spaceos/cred/cloud.key")? {
            return Err(String::from("the audit does not record the refused read of the credential"));
        }
        broker_quit(c.broker_op)
    })();
    // A broker that was not told to quit waits for its operator forever: a failed
    // check must end in a report, not in a run that hangs until the harness gives up.
    if checked.is_err() {
        sys::kill(c.broker).ok();
    }
    let broker_end = sys::wait(c.broker);
    sys::handle_close(c.broker).ok();
    sys::handle_close(c.broker_op).ok();
    checked?;
    match broker_end {
        Ok(st) if st.is_exited_with(0) => Ok(()),
        other => Err(format!("the broker ended with {other:?}")),
    }
}
