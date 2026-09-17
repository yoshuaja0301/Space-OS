//! `spaceagent` – the agent runtime (requirement G01).
//!
//! It is handed one channel to the Tool Broker and nothing else. It has no root
//! capability, so `fs_open` on its own bootstrap handle is refused by the kernel
//! before the broker is even consulted - the first thing this program does is show
//! that, because "the agent cannot reach outside" should be a property of the
//! system, not a claim about the agent's code.
//!
//! The task itself is deliberately small and deterministic: read the instruction
//! file, apply it to the input, write the result back, and ask for the check to be
//! run. Then it tries two things it is not allowed to do, so the audit log has
//! refusals in it as well as successes.
#![no_std]
#![no_main]

extern crate alloc;

use alloc::vec::Vec;

use libspace::spaceabi::agent::{
    ABI_VERSION, CHECK_VERIFY, CHUNK_MAX, ToolReply, ToolRequest, tool, verdict,
};
use libspace::spaceabi::error::Error;
use libspace::{handle, println, sys};

const WORKSPACE: &str = "/spaceos/ws";
const TASK: &str = "/spaceos/ws/task.txt";
const INPUT: &str = "/spaceos/ws/input.txt";
const OUTPUT: &str = "/spaceos/ws/output.txt";
/// Two paths the agent has no business touching.
const OUTSIDE_READ: &str = "/spaceos/manifest.txt";
const OUTSIDE_WRITE: &str = "/spaceos/ws/../stolen.txt";

fn as_bytes<T>(v: &T) -> &[u8] {
    // SAFETY: `T` is a `repr(C)` plain-data message.
    unsafe { core::slice::from_raw_parts(v as *const T as *const u8, core::mem::size_of::<T>()) }
}

fn call(r: &ToolRequest) -> Result<ToolReply, Error> {
    sys::send(handle::BOOTSTRAP, as_bytes(r), None)?;
    let mut buf = [0u8; core::mem::size_of::<ToolReply>()];
    let (n, transferred) = sys::recv(handle::BOOTSTRAP, &mut buf, false)?;
    if let Some(h) = transferred {
        sys::handle_close(h).ok();
    }
    if n != buf.len() {
        return Err(Error::MsgSize);
    }
    // SAFETY: the broker replies with exactly one `ToolReply`.
    Ok(unsafe { core::ptr::read_unaligned(buf.as_ptr() as *const ToolReply) })
}

/// Read a whole file through the broker, one chunk at a time.
fn read_all(path: &str) -> Result<Vec<u8>, Error> {
    let mut out = Vec::new();
    let mut offset = 0u32;
    loop {
        let mut r = ToolRequest::new(tool::READ, path);
        r.offset = offset;
        r.len = CHUNK_MAX as u32;
        let reply = call(&r)?;
        reply.result()?;
        let chunk = reply.data();
        if chunk.is_empty() {
            return Ok(out);
        }
        out.try_reserve(chunk.len()).map_err(|_| Error::NoMemory)?;
        out.extend_from_slice(chunk);
        offset += chunk.len() as u32;
        if out.len() > 64 * 1024 {
            return Err(Error::NoMemory);
        }
    }
}

fn write_all(path: &str, data: &[u8]) -> Result<(), Error> {
    let mut offset = 0usize;
    while offset < data.len() {
        let n = (data.len() - offset).min(CHUNK_MAX);
        let mut r = ToolRequest::new(tool::WRITE, path);
        r.offset = offset as u32;
        r.set_data(&data[offset..offset + n]);
        call(&r)?.result()?;
        offset += n;
    }
    Ok(())
}

/// The whole "reasoning" of this agent: the rule named by the task file.
fn apply(rule: &str, input: &[u8]) -> Option<Vec<u8>> {
    let (from, to) = rule.split_once("=>")?;
    let (from, to) = (from.trim(), to.trim());
    if from.is_empty() {
        return None;
    }
    let mut out = Vec::new();
    let mut i = 0usize;
    while i < input.len() {
        if input[i..].starts_with(from.as_bytes()) {
            out.extend_from_slice(to.as_bytes());
            i += from.len();
        } else {
            out.push(input[i]);
            i += 1;
        }
    }
    Some(out)
}

fn run() -> Result<(), Error> {
    // The kernel, not the broker, is what stops the agent reaching the disk: its
    // bootstrap handle is a channel, so the file syscalls refuse it outright.
    match sys::fs_open(handle::BOOTSTRAP, INPUT) {
        Err(Error::Denied) => println!("[agent] no file capability of my own, as expected"),
        other => {
            println!("[agent] the agent could open a file directly: {other:?}");
            return Err(Error::Denied);
        }
    }

    let mut hello = ToolRequest::new(tool::HELLO, "-");
    hello.abi_version = ABI_VERSION;
    call(&hello)?.result()?;

    let listing = call(&ToolRequest::new(tool::LIST, WORKSPACE))?;
    listing.result()?;
    println!("[agent] workspace: {}", listing.text());

    let rule_bytes = read_all(TASK)?;
    let rule = core::str::from_utf8(&rule_bytes).map_err(|_| Error::Invalid)?;
    let rule = rule.lines().find(|l| l.contains("=>")).unwrap_or("").trim();
    println!("[agent] task: {rule}");

    let input = read_all(INPUT)?;
    let patched = apply(rule, &input).ok_or(Error::Invalid)?;
    println!("[agent] patched {} bytes into {} bytes", input.len(), patched.len());
    write_all(OUTPUT, &patched)?;

    // Read it back through the broker: the overlay is the view the agent works in.
    let readback = read_all(OUTPUT)?;
    if readback != patched {
        println!("[agent] read-back does not match what was written");
        return Err(Error::Fault);
    }

    let check = call(&ToolRequest::new(tool::CHECK, CHECK_VERIFY))?;
    check.result()?;
    if check.value != 1 {
        println!("[agent] check failed: the patch does not match what was expected");
        return Err(Error::Invalid);
    }
    println!("[agent] check '{CHECK_VERIFY}' passed");

    // Two things the agent must not be able to do. Both are expected to come back
    // refused, and both are expected to appear in the audit log.
    let mut outside = ToolRequest::new(tool::READ, OUTSIDE_READ);
    outside.len = 16;
    let denied = call(&outside)?;
    if denied.verdict != verdict::DENIED_SCOPE {
        println!("[agent] reading outside the workspace was not refused");
        return Err(Error::Denied);
    }
    let mut escape = ToolRequest::new(tool::WRITE, OUTSIDE_WRITE);
    escape.set_data(b"x");
    let denied = call(&escape)?;
    if denied.verdict != verdict::DENIED_SCOPE {
        println!("[agent] escaping the workspace with '..' was not refused");
        return Err(Error::Denied);
    }
    // The audit log is the operator's, not the agent's.
    let audit = call(&ToolRequest::new(tool::AUDIT, "-"))?;
    if audit.verdict != verdict::DENIED_TOOL {
        println!("[agent] the agent could read the audit log");
        return Err(Error::Denied);
    }
    println!("[agent] out-of-scope read, escape and audit access all refused");
    Ok(())
}

#[unsafe(no_mangle)]
pub extern "C" fn space_main() -> i32 {
    println!("[agent] Space OS agent runtime, ABI v{ABI_VERSION}");
    let outcome = run();
    let done = call(&ToolRequest::new(tool::DONE, "-"));
    match (outcome, done) {
        (Ok(()), Ok(reply)) => {
            println!("[agent] result=PASS ({} refusals recorded by the broker)", reply.value);
            0
        }
        (Ok(()), Err(e)) => {
            println!("[agent] result=FAIL could not report completion: {e}");
            1
        }
        (Err(e), _) => {
            println!("[agent] result=FAIL {e}");
            1
        }
    }
}
