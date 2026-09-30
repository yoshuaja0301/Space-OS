//! The lab's cloud model provider -- a mock, and called one everywhere (ADR-0018).
//!
//! It speaks the part of a Messages-style API that `spacecloud` uses: `POST
//! /v1/messages` over HTTP/1.1 and TLS 1.3 (a certificate for api.cloud.test from
//! the lab authority), the API key in `x-api-key`, a JSON body, and the answer
//! streamed as server-sent events in chunked transfer encoding. There is no model:
//! the model name picks a script, each written to exercise one thing the adapter
//! must get right, and every script logs what it saw, so the harness can hold the
//! adapter to the provider's side of the story as well as its own.
//!
//! | model | script |
//! |---|---|
//! | `lab-echo` | a short answer in four pieces, 150 ms apart |
//! | `lab-tool` | asks to read a workspace file, then quotes what it was given |
//! | `lab-exfil` | asks to read the credential file, then says what it was given |
//! | `lab-stall` | starts an answer and never finishes it |
//! | `lab-overloaded` | 529, every time |
//! | `lab-runaway` | streams on past `max_tokens`, reporting ever more tokens |
//! | `lab-hostile` | an event nested 120 levels deep, to exhaust a parser's stack |
//!
//! Any other model is a 404, and a request without the lab's key is a 401 whatever
//! the model.

use std::io::{self, Read, Write};
use std::thread::sleep;
use std::time::{Duration, Instant};

use rustls::{ServerConnection, Stream};
use serde_json::{Value, json};

use super::lab::{Wire, log, server_config};
use super::pki;

const HEAD_MAX: usize = 16 * 1024;
const BODY_MAX: usize = 64 * 1024;
/// The API version the adapter must name.
const API_VERSION: &str = "2023-06-01";
/// The workspace file `lab-tool` asks for, and the credential `lab-exfil` asks for.
const WORKSPACE_FILE: &str = "/spaceos/ws/input.txt";
const CREDENTIAL_FILE: &str = "/spaceos/cred/cloud.key";

type Tls<'a> = Stream<'a, ServerConnection, Wire>;

struct Request {
    method: String,
    path: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl Request {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(n, _)| n == name).map(|(_, v)| v.as_str())
    }
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

fn read_request(tls: &mut Tls<'_>) -> io::Result<Request> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    let head_end = loop {
        if let Some(i) = find(&buf, b"\r\n\r\n") {
            break i;
        }
        if buf.len() > HEAD_MAX {
            return Err(io::Error::other("request head too long"));
        }
        let n = tls.read(&mut chunk)?;
        if n == 0 {
            return Err(io::Error::other("the client closed before sending a request"));
        }
        buf.extend_from_slice(&chunk[..n]);
    };
    let head = std::str::from_utf8(&buf[..head_end]).map_err(io::Error::other)?;
    let mut lines = head.split("\r\n");
    let mut first = lines.next().unwrap_or("").split(' ');
    let method = first.next().unwrap_or("").to_string();
    let path = first.next().unwrap_or("").to_string();
    let headers: Vec<(String, String)> = lines
        .filter_map(|l| l.split_once(':'))
        .map(|(n, v)| (n.trim().to_ascii_lowercase(), v.trim().to_string()))
        .collect();
    let len: usize =
        headers.iter().find(|(n, _)| n == "content-length").and_then(|(_, v)| v.parse().ok()).unwrap_or(0);
    if len > BODY_MAX {
        return Err(io::Error::other(format!("a {len}-byte body")));
    }
    let mut body = buf[head_end + 4..].to_vec();
    while body.len() < len {
        let n = tls.read(&mut chunk)?;
        if n == 0 {
            return Err(io::Error::other("the body ended early"));
        }
        body.extend_from_slice(&chunk[..n]);
    }
    body.truncate(len);
    Ok(Request { method, path, headers, body })
}

fn error_body(kind: &str, message: &str) -> Value {
    json!({"type": "error", "error": {"type": kind, "message": message}})
}

fn respond(tls: &mut Tls<'_>, status: u16, reason: &str, body: &Value) -> io::Result<()> {
    let body = body.to_string();
    write!(
        tls,
        "HTTP/1.1 {status} {reason}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    )?;
    tls.flush()
}

/// An answer as server-sent events, one event per chunk of the chunked body.
struct Sse<'a, 'b> {
    tls: &'a mut Tls<'b>,
    events: usize,
}

impl<'a, 'b> Sse<'a, 'b> {
    fn start(tls: &'a mut Tls<'b>) -> io::Result<Sse<'a, 'b>> {
        write!(
            tls,
            "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncache-control: no-cache\r\n\
             transfer-encoding: chunked\r\nconnection: close\r\n\r\n"
        )?;
        tls.flush()?;
        Ok(Sse { tls, events: 0 })
    }

    fn send(&mut self, name: &str, data: Value) -> io::Result<()> {
        let payload = format!("event: {name}\ndata: {data}\n\n");
        write!(self.tls, "{:x}\r\n{payload}\r\n", payload.len())?;
        self.tls.flush()?;
        self.events += 1;
        Ok(())
    }

    fn finish(self) -> io::Result<()> {
        write!(self.tls, "0\r\n\r\n")?;
        self.tls.flush()
    }
}

/// Tokens as this provider counts them: a quarter of the bytes, rounded up.
fn tokens(bytes: usize) -> u64 {
    bytes.div_ceil(4) as u64
}

/// What one answer consists of.
struct Answer<'s> {
    deltas: Vec<String>,
    tool: Option<(&'s str, &'s str, Value)>,
    pace: Duration,
}

/// Stream `answer` as a complete message; returns the output tokens reported.
fn stream(tls: &mut Tls<'_>, model: &str, input_tokens: u64, answer: Answer<'_>) -> io::Result<u64> {
    let mut sse = Sse::start(tls)?;
    sse.send(
        "message_start",
        json!({"type": "message_start", "message": {"id": "msg_lab", "type": "message", "role": "assistant",
            "model": model, "content": [], "stop_reason": null, "stop_sequence": null,
            "usage": {"input_tokens": input_tokens, "output_tokens": 1}}}),
    )?;
    sse.send("ping", json!({"type": "ping"}))?;
    let mut output = 0;
    let mut index = 0;
    if !answer.deltas.is_empty() {
        sse.send(
            "content_block_start",
            json!({"type": "content_block_start", "index": index, "content_block": {"type": "text", "text": ""}}),
        )?;
        for d in &answer.deltas {
            sleep(answer.pace);
            sse.send(
                "content_block_delta",
                json!({"type": "content_block_delta", "index": index, "delta": {"type": "text_delta", "text": d}}),
            )?;
            output += tokens(d.len());
        }
        sse.send("content_block_stop", json!({"type": "content_block_stop", "index": index}))?;
        index += 1;
    }
    let stop = if let Some((id, name, input)) = &answer.tool {
        sse.send(
            "content_block_start",
            json!({"type": "content_block_start", "index": index,
                "content_block": {"type": "tool_use", "id": id, "name": name, "input": {}}}),
        )?;
        // The input arrives as JSON text in pieces, split where a client that
        // parsed each piece on its own would fail.
        let text = input.to_string();
        let (a, b) = text.split_at(text.len() / 2);
        for part in [a, b] {
            sse.send(
                "content_block_delta",
                json!({"type": "content_block_delta", "index": index,
                    "delta": {"type": "input_json_delta", "partial_json": part}}),
            )?;
        }
        sse.send("content_block_stop", json!({"type": "content_block_stop", "index": index}))?;
        output += tokens(text.len());
        "tool_use"
    } else {
        "end_turn"
    };
    sse.send(
        "message_delta",
        json!({"type": "message_delta", "delta": {"stop_reason": stop, "stop_sequence": null},
            "usage": {"output_tokens": output}}),
    )?;
    sse.send("message_stop", json!({"type": "message_stop"}))?;
    sse.finish()?;
    Ok(output)
}

/// The tool result in the last user turn, if that turn is one: (id, text, is_error).
fn tool_result(body: &Value) -> Option<(String, String, bool)> {
    let last = body["messages"].as_array()?.last()?;
    let block = last["content"].as_array()?.iter().find(|b| b["type"] == "tool_result")?;
    let text = match &block["content"] {
        Value::String(s) => s.clone(),
        Value::Array(parts) => parts.iter().filter_map(|p| p["text"].as_str()).collect::<Vec<_>>().join(""),
        _ => String::new(),
    };
    Some((block["tool_use_id"].as_str()?.to_string(), text, block["is_error"].as_bool().unwrap_or(false)))
}

/// Whether the history holds the tool call `id` as the assistant made it: the
/// adapter has to send its model's turn back, not just the result.
fn history_has_call(body: &Value, id: &str) -> bool {
    body["messages"].as_array().is_some_and(|m| {
        m.iter().any(|turn| {
            turn["role"] == "assistant"
                && turn["content"]
                    .as_array()
                    .is_some_and(|c| c.iter().any(|b| b["type"] == "tool_use" && b["id"] == id))
        })
    })
}

fn offered(body: &Value, tool: &str) -> bool {
    body["tools"].as_array().is_some_and(|t| t.iter().any(|x| x["name"] == tool))
}

/// A model that asks for `path` through `read_file`, then answers with what it got.
fn tool_script(
    tls: &mut Tls<'_>,
    model: &str,
    input: u64,
    body: &Value,
    id: &str,
    path: &str,
) -> io::Result<()> {
    let Some((got_id, text, is_error)) = tool_result(body) else {
        if !offered(body, "read_file") {
            log(&format!("cloud: {model}: no read_file tool was offered"));
            let answer = Answer {
                deltas: vec![String::from("No tool was offered.")],
                tool: None,
                pace: Duration::ZERO,
            };
            return stream(tls, model, input, answer).map(|_| ());
        }
        log(&format!("cloud: {model}: asked to read {path}"));
        let answer = Answer {
            deltas: vec![String::from("I will read the file.")],
            tool: Some((id, "read_file", json!({"path": path}))),
            pace: Duration::ZERO,
        };
        return stream(tls, model, input, answer).map(|_| ());
    };
    if got_id != id || !history_has_call(body, id) {
        log(&format!("cloud: {model}: a tool result for {got_id} without the call it answers"));
        return respond(
            tls,
            400,
            "Bad Request",
            &error_body("invalid_request_error", "tool_result without tool_use"),
        );
    }
    let reply = if model == "lab-exfil" {
        if is_error {
            log(&format!("cloud: lab-exfil: the tool was refused: {text}"));
            format!("The tool refused: {text}")
        } else {
            log(&format!("cloud: lab-exfil: THE CREDENTIAL LEAKED ({} bytes)", text.len()));
            format!("The tool said: {text}")
        }
    } else {
        log(&format!(
            "cloud: {model}: tool_result for {id}: {} bytes{}",
            text.len(),
            if is_error { ", marked as an error" } else { "" }
        ));
        format!("The file begins: {}", text.lines().next().unwrap_or(""))
    };
    let answer = Answer { deltas: vec![reply], tool: None, pace: Duration::ZERO };
    stream(tls, model, input, answer).map(|_| ())
}

fn serve_request(tls: &mut Tls<'_>, req: &Request) -> io::Result<()> {
    if req.method != "POST" || req.path != "/v1/messages" {
        log(&format!("cloud: 404: {} {}", req.method, req.path));
        return respond(tls, 404, "Not Found", &error_body("not_found_error", "no such endpoint"));
    }
    let body: Value = match serde_json::from_slice(&req.body) {
        Ok(v) => v,
        Err(e) => {
            log(&format!("cloud: 400: the body is not JSON: {e}"));
            return respond(
                tls,
                400,
                "Bad Request",
                &error_body("invalid_request_error", "body is not JSON"),
            );
        }
    };
    let model = body["model"].as_str().unwrap_or("").to_string();
    let key = pki::api_key().map_err(io::Error::other)?;
    if req.header("x-api-key") != Some(key.as_str()) {
        log(&format!("cloud: 401: {model}: the key presented is not the lab's"));
        return respond(tls, 401, "Unauthorized", &error_body("authentication_error", "invalid x-api-key"));
    }
    if req.header("anthropic-version") != Some(API_VERSION)
        || req.header("content-type") != Some("application/json")
        || body["stream"] != true
    {
        log(&format!("cloud: 400: {model}: missing version, content type or stream"));
        return respond(tls, 400, "Bad Request", &error_body("invalid_request_error", "malformed request"));
    }
    let max_tokens = body["max_tokens"].as_u64().unwrap_or(0);
    let input = tokens(req.body.len());
    let attempt = req.header("x-spacecloud-attempt").unwrap_or("?").to_string();
    match model.as_str() {
        "lab-echo" => {
            let prompt = body["messages"]
                .as_array()
                .and_then(|m| m.last())
                .and_then(|t| t["content"].as_str())
                .unwrap_or("")
                .to_string();
            let answer = Answer {
                deltas: vec![
                    String::from("You said: "),
                    prompt,
                    String::from(" -- this answer came"),
                    String::from(" in four pieces."),
                ],
                tool: None,
                pace: Duration::from_millis(150),
            };
            let output = stream(tls, &model, input, answer)?;
            log(&format!("cloud: lab-echo: streamed 4 pieces, {input} in / {output} out tokens"));
            Ok(())
        }
        "lab-tool" => tool_script(tls, &model, input, &body, "toolu_lab_1", WORKSPACE_FILE),
        "lab-exfil" => tool_script(tls, &model, input, &body, "toolu_lab_2", CREDENTIAL_FILE),
        "lab-stall" => {
            let mut sse = Sse::start(tls)?;
            sse.send(
                "message_start",
                json!({"type": "message_start", "message": {"id": "msg_lab", "type": "message",
                    "role": "assistant", "model": model, "content": [], "stop_reason": null,
                    "usage": {"input_tokens": input, "output_tokens": 1}}}),
            )?;
            sse.send("ping", json!({"type": "ping"}))?;
            let t0 = Instant::now();
            // Wait for the client to give up: its end of the connection closing is the
            // only way this answer ends.
            let mut buf = [0u8; 256];
            loop {
                match sse.tls.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {}
                }
            }
            log(&format!(
                "cloud: lab-stall: the client closed the connection after {} ms",
                t0.elapsed().as_millis()
            ));
            Ok(())
        }
        "lab-overloaded" => {
            log(&format!("cloud: lab-overloaded: attempt {attempt} answered 529"));
            respond(tls, 529, "Overloaded", &error_body("overloaded_error", "Overloaded"))
        }
        "lab-runaway" => {
            let mut sse = Sse::start(tls)?;
            let mut rounds = 0;
            let result = (|| -> io::Result<()> {
                sse.send(
                    "message_start",
                    json!({"type": "message_start", "message": {"id": "msg_lab", "type": "message",
                        "role": "assistant", "model": model, "content": [], "stop_reason": null,
                        "usage": {"input_tokens": input, "output_tokens": 1}}}),
                )?;
                sse.send(
                    "content_block_start",
                    json!({"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}}),
                )?;
                for i in 1..=30u64 {
                    sleep(Duration::from_millis(50));
                    sse.send(
                        "content_block_delta",
                        json!({"type": "content_block_delta", "index": 0,
                            "delta": {"type": "text_delta", "text": "and more "}}),
                    )?;
                    // Ever more output, far past max_tokens: a provider that does not
                    // stop, and says so.
                    sse.send(
                        "message_delta",
                        json!({"type": "message_delta", "delta": {"stop_reason": null},
                            "usage": {"output_tokens": 400 * i}}),
                    )?;
                    rounds = i;
                }
                Ok(())
            })();
            match result {
                Err(_) => {
                    log(&format!(
                        "cloud: lab-runaway: the client closed the connection after {rounds} rounds \
                         ({} tokens reported, max_tokens {max_tokens})",
                        400 * rounds
                    ));
                    Ok(())
                }
                Ok(()) => {
                    log("cloud: lab-runaway: streamed all 30 rounds; nobody stopped it");
                    sse.send("message_stop", json!({"type": "message_stop"}))?;
                    sse.finish()
                }
            }
        }
        "lab-hostile" => {
            let mut sse = Sse::start(tls)?;
            sse.send(
                "message_start",
                json!({"type": "message_start", "message": {"id": "msg_lab", "type": "message",
                    "role": "assistant", "model": model, "content": [], "stop_reason": null,
                    "usage": {"input_tokens": input, "output_tokens": 1}}}),
            )?;
            // Valid JSON, and just inside the nesting a stock parser accepts.
            let depth = 120;
            let payload =
                format!("event: content_block_start\ndata: {}{}\n\n", "[".repeat(depth), "]".repeat(depth));
            write!(sse.tls, "{:x}\r\n{payload}\r\n", payload.len())?;
            sse.tls.flush()?;
            log(&format!("cloud: lab-hostile: sent an event nested {depth} levels deep"));
            let mut buf = [0u8; 256];
            while let Ok(n) = sse.tls.read(&mut buf) {
                if n == 0 {
                    break;
                }
            }
            log("cloud: lab-hostile: the client closed the connection");
            Ok(())
        }
        other => {
            log(&format!("cloud: 404: model {other}"));
            respond(tls, 404, "Not Found", &error_body("not_found_error", "no such model"))
        }
    }
}

/// `xtask lab cloud`: one connection, one request.
pub fn serve() -> io::Result<()> {
    let mut conn = ServerConnection::new(server_config("cloud", None)?).map_err(io::Error::other)?;
    let mut wire = Wire::plain();
    while conn.is_handshaking() {
        if let Err(e) = conn.complete_io(&mut wire) {
            log(&format!("cloud: the handshake ended: {e}"));
            return Ok(());
        }
    }
    let mut tls = Stream::new(&mut conn, &mut wire);
    let req = match read_request(&mut tls) {
        Ok(r) => r,
        Err(e) => {
            log(&format!("cloud: no request: {e}"));
            return Ok(());
        }
    };
    serve_request(&mut tls, &req)?;
    conn.send_close_notify();
    while conn.wants_write() {
        if conn.write_tls(&mut wire).is_err() {
            break;
        }
    }
    Ok(())
}
