//! `spaceai` – the Space OS AI runtime.
//!
//! Loads a SpaceLM model from the guest disk, verifies it against the manifest,
//! and generates tokens entirely on this machine: every arithmetic operation goes
//! through the Space Compute ABI, so the runtime owns layout and scheduling while
//! the service owns the backend (PRD §4). Nothing here talks to a host.
//!
//! The generated tokens must match the baseline pinned by the host reference
//! implementation, which shares this model's layout and math.
#![no_std]
#![no_main]

extern crate alloc;

use alloc::string::String;
use alloc::vec::Vec;

use libspace::compute::{Buffer, Compute};
use libspace::spaceabi::compute::{BufferRef, Op, op};
use libspace::spaceabi::math::sqrt_f32;
use libspace::spaceabi::model::{HEADER_BYTES, Header, ModelError};
use libspace::{Handle, handle, println, sha256, sys};

const MODEL_PATH: &str = "/spaceos/model.slm";
const MANIFEST_PATH: &str = "/spaceos/manifest.txt";
const BASELINE_PATH: &str = "/spaceos/baseline.txt";
const BAD_MODELS: [&str; 3] = ["/spaceos/badmagic.slm", "/spaceos/baddims.slm", "/spaceos/trunc.slm"];

/// Scratch layout, in `f32` units.
struct Scratch {
    x: u32,
    xn: u32,
    q: u32,
    k: u32,
    v: u32,
    att: u32,
    tmp: u32,
    g: u32,
    u: u32,
    scores: u32,
    probs: u32,
    logits: u32,
    floats: u32,
}

fn scratch_layout(h: &Header) -> Scratch {
    let d = h.d_model;
    let ff = h.d_ff;
    let seq = h.max_seq;
    let mut at = 0u32;
    let mut take = |n: u32| {
        let off = at;
        at += n;
        off
    };
    let s = Scratch {
        x: take(d),
        xn: take(d),
        q: take(d),
        k: take(d),
        v: take(d),
        att: take(d),
        tmp: take(if d > ff { d } else { ff }),
        g: take(ff),
        u: take(ff),
        scores: take(seq),
        probs: take(seq),
        logits: take(h.vocab),
        floats: 0,
    };
    Scratch { floats: at, ..s }
}

fn slice_of(b: &Buffer, offset_floats: u32, len_floats: u32) -> BufferRef {
    BufferRef { id: b.id, _pad: 0, offset: offset_floats as u64 * 4, len: len_floats as u64 * 4 }
}

/// Read a whole file from the guest disk, feeding it to `sink` in 4 KiB chunks.
fn stream(root: Handle, path: &str, mut sink: impl FnMut(&[u8], u64)) -> Result<u64, String> {
    let f = sys::fs_open(root, path).map_err(|e| alloc::format!("open {path}: {e}"))?;
    let mut buf = [0u8; 4096];
    let mut offset = 0u64;
    loop {
        let n = match sys::fs_read(f, offset, &mut buf) {
            Ok(n) => n,
            Err(e) => {
                sys::handle_close(f).ok();
                return Err(alloc::format!("read {path}: {e}"));
            }
        };
        if n == 0 {
            break;
        }
        sink(&buf[..n], offset);
        offset += n as u64;
    }
    sys::handle_close(f).ok();
    Ok(offset)
}

fn read_text(root: Handle, path: &str) -> Result<String, String> {
    let mut out = String::new();
    stream(root, path, |chunk, _| {
        out.push_str(core::str::from_utf8(chunk).unwrap_or(""));
    })?;
    Ok(out)
}

fn field<'a>(text: &'a str, key: &str) -> Option<&'a str> {
    text.lines().find_map(|l| l.strip_prefix(key)?.strip_prefix('=').map(str::trim))
}

/// Every malformed model must be refused by the header check, with a reason.
fn reject_bad_models(root: Handle) -> Result<(), String> {
    for path in BAD_MODELS {
        let f = sys::fs_open(root, path).map_err(|e| alloc::format!("open {path}: {e}"))?;
        let stat = sys::fs_stat(f).map_err(|e| alloc::format!("stat {path}: {e}"))?;
        let mut head = [0u8; HEADER_BYTES];
        let n = sys::fs_read(f, 0, &mut head).map_err(|e| alloc::format!("read {path}: {e}"))?;
        sys::handle_close(f).ok();
        match Header::parse(&head[..n], stat.size) {
            Err(e) => println!("[ai] rejected {path}: {e}"),
            Ok(_) => return Err(alloc::format!("{path} was accepted but is malformed")),
        }
    }
    // A header that is fine but a file that is empty must also be refused.
    if Header::parse(&[], 0) != Err(ModelError::TooShort) {
        return Err(String::from("empty model was not rejected"));
    }
    Ok(())
}

struct Model {
    header: Header,
    weights: Buffer,
}

/// Verify the model against its manifest while streaming it into a compute buffer.
fn load_model(root: Handle, c: &Compute) -> Result<Model, String> {
    let manifest = read_text(root, MANIFEST_PATH)?;
    let want_hash = field(&manifest, "sha256").ok_or_else(|| String::from("manifest has no sha256"))?;
    let want_size: u64 = field(&manifest, "size")
        .and_then(|v| v.parse().ok())
        .ok_or_else(|| String::from("manifest has no size"))?;

    let f = sys::fs_open(root, MODEL_PATH).map_err(|e| alloc::format!("open model: {e}"))?;
    let stat = sys::fs_stat(f).map_err(|e| alloc::format!("stat model: {e}"))?;
    if stat.size != want_size {
        sys::handle_close(f).ok();
        return Err(alloc::format!("model is {} bytes, manifest says {want_size}", stat.size));
    }
    let mut head = [0u8; HEADER_BYTES];
    let n = sys::fs_read(f, 0, &mut head).map_err(|e| alloc::format!("read header: {e}"))?;
    let header = Header::parse(&head[..n], stat.size).map_err(|e| alloc::format!("model rejected: {e}"))?;
    sys::handle_close(f).ok();

    let weight_bytes = header.weight_floats() * 4;
    println!(
        "[ai] model: {} layers, d_model {}, {} heads x {}, d_ff {}, vocab {}, {} params ({} KiB)",
        header.n_layers,
        header.d_model,
        header.n_heads,
        header.head_dim,
        header.d_ff,
        header.vocab,
        header.weight_floats(),
        weight_bytes / 1024
    );

    // Allocate first, then stream the file straight into the shared buffer while
    // hashing it: the weights are never copied twice and never held on the heap.
    let weights =
        c.buffer_create(weight_bytes as usize).map_err(|e| alloc::format!("weights buffer: {e}"))?;
    let mut hasher = sha256::Sha256::new();
    let read = stream(root, MODEL_PATH, |chunk, offset| {
        hasher.update(chunk);
        if offset + chunk.len() as u64 > HEADER_BYTES as u64 {
            let skip = HEADER_BYTES.saturating_sub(offset as usize);
            let dst_off = (offset as usize + skip).saturating_sub(HEADER_BYTES);
            // SAFETY: inside the mapped weights buffer; bounds checked by the header.
            unsafe {
                let dst = weights.ptr.add(dst_off);
                core::ptr::copy_nonoverlapping(chunk[skip..].as_ptr(), dst, chunk.len() - skip);
            }
        }
    })?;
    if read != stat.size {
        return Err(alloc::format!("model read {read} of {} bytes", stat.size));
    }
    let digest = sha256::to_hex(&hasher.finish());
    let digest = core::str::from_utf8(&digest).unwrap_or("");
    if digest != want_hash {
        return Err(alloc::format!("model checksum {digest} does not match the manifest"));
    }
    println!("[ai] model verified: sha256 {digest}");
    Ok(Model { header, weights })
}

struct Runtime<'a> {
    c: &'a Compute,
    queue: u32,
    h: Header,
    w: Buffer,
    kcache: Buffer,
    vcache: Buffer,
    scratch: Buffer,
    s: Scratch,
}

impl Runtime<'_> {
    fn wref(&self, offset_floats: u64, len_floats: u64) -> BufferRef {
        BufferRef { id: self.w.id, _pad: 0, offset: offset_floats * 4, len: len_floats * 4 }
    }

    fn sref(&self, offset_floats: u32, len_floats: u32) -> BufferRef {
        slice_of(&self.scratch, offset_floats, len_floats)
    }

    fn run(&self, o: Op) -> Result<u64, String> {
        self.c.run(self.queue, o).map_err(|e| alloc::format!("op {}: {e}", o.kind))
    }

    /// One decoding step at `pos`; returns the argmax token of the produced logits.
    fn step(&self, token: u32, pos: u32) -> Result<u32, String> {
        let h = self.h;
        let (d, ff, heads, hd, seq) = (h.d_model, h.d_ff, h.n_heads, h.head_dim, h.max_seq);
        let l = h.layout();
        let s = &self.s;

        self.run(Op {
            kind: op::EMBED,
            dst: self.sref(s.x, d),
            a: self.wref(l.tok_emb(), h.vocab as u64 * d as u64),
            dims: [d, token, 0, 0],
            ..Default::default()
        })?;

        for layer in 0..h.n_layers {
            self.run(Op {
                kind: op::RMSNORM,
                dst: self.sref(s.xn, d),
                a: self.sref(s.x, d),
                b: self.wref(l.attn_norm(layer), d as u64),
                dims: [d, 0, 0, 0],
                ..Default::default()
            })?;
            for (dst, w) in [(s.q, l.wq(layer)), (s.k, l.wk(layer)), (s.v, l.wv(layer))] {
                self.run(Op {
                    kind: op::MATMUL,
                    dst: self.sref(dst, d),
                    a: self.sref(s.xn, d),
                    b: self.wref(w, d as u64 * d as u64),
                    dims: [1, d, d, 0],
                    ..Default::default()
                })?;
            }
            for dst in [s.q, s.k] {
                self.run(Op {
                    kind: op::ROPE,
                    dst: self.sref(dst, d),
                    dims: [heads, hd, pos, 0],
                    ..Default::default()
                })?;
            }

            // Layout work belongs to the runtime: copy this position's K and V into
            // the caches, V transposed so attention is a plain matmul.
            // SAFETY: all three buffers are mapped by this process.
            unsafe {
                let kv = self.scratch.as_f32_mut();
                let kc = self.kcache.as_f32_mut();
                let vc = self.vcache.as_f32_mut();
                for head in 0..heads {
                    let src = (s.k + head * hd) as usize;
                    let kbase = ((layer * heads + head) * seq * hd + pos * hd) as usize;
                    kc[kbase..kbase + hd as usize].copy_from_slice(&kv[src..src + hd as usize]);
                    let vbase = ((layer * heads + head) * hd * seq) as usize;
                    for i in 0..hd as usize {
                        vc[vbase + i * seq as usize + pos as usize] = kv[(s.v + head * hd) as usize + i];
                    }
                }
            }

            let inv = 1.0 / sqrt_f32(hd as f32);
            for head in 0..heads {
                let kbase = (layer * heads + head) * seq * hd;
                self.run(Op {
                    kind: op::MATMUL,
                    dst: self.sref(s.scores, pos + 1),
                    a: self.sref(s.q + head * hd, hd),
                    b: slice_of(&self.kcache, kbase, (pos + 1) * hd),
                    dims: [1, hd, pos + 1, 0],
                    ..Default::default()
                })?;
                self.run(Op {
                    kind: op::SCALE,
                    dst: self.sref(s.scores, pos + 1),
                    a: self.sref(s.scores, pos + 1),
                    dims: [pos + 1, 0, 0, 0],
                    scalar: inv,
                    ..Default::default()
                })?;
                self.run(Op {
                    kind: op::SOFTMAX,
                    dst: self.sref(s.probs, pos + 1),
                    a: self.sref(s.scores, pos + 1),
                    dims: [pos + 1, 0, 0, 0],
                    ..Default::default()
                })?;
                // The probability tail past `pos` stays zero, so summing over the
                // full cache stride gives exactly the same result as summing over
                // `pos + 1` terms - and keeps the cache rows contiguous.
                let vbase = (layer * heads + head) * hd * seq;
                self.run(Op {
                    kind: op::MATMUL,
                    dst: self.sref(s.att + head * hd, hd),
                    a: self.sref(s.probs, seq),
                    b: slice_of(&self.vcache, vbase, hd * seq),
                    dims: [1, seq, hd, 0],
                    ..Default::default()
                })?;
            }
            self.run(Op {
                kind: op::MATMUL,
                dst: self.sref(s.tmp, d),
                a: self.sref(s.att, d),
                b: self.wref(l.wo(layer), d as u64 * d as u64),
                dims: [1, d, d, 0],
                ..Default::default()
            })?;
            self.run(Op {
                kind: op::ADD,
                dst: self.sref(s.x, d),
                a: self.sref(s.x, d),
                b: self.sref(s.tmp, d),
                dims: [d, 0, 0, 0],
                ..Default::default()
            })?;
            self.run(Op {
                kind: op::RMSNORM,
                dst: self.sref(s.xn, d),
                a: self.sref(s.x, d),
                b: self.wref(l.ffn_norm(layer), d as u64),
                dims: [d, 0, 0, 0],
                ..Default::default()
            })?;
            for (dst, w) in [(s.g, l.w1(layer)), (s.u, l.w3(layer))] {
                self.run(Op {
                    kind: op::MATMUL,
                    dst: self.sref(dst, ff),
                    a: self.sref(s.xn, d),
                    b: self.wref(w, ff as u64 * d as u64),
                    dims: [1, d, ff, 0],
                    ..Default::default()
                })?;
            }
            self.run(Op {
                kind: op::SILU_MUL,
                dst: self.sref(s.g, ff),
                a: self.sref(s.g, ff),
                b: self.sref(s.u, ff),
                dims: [ff, 0, 0, 0],
                ..Default::default()
            })?;
            self.run(Op {
                kind: op::MATMUL,
                dst: self.sref(s.tmp, d),
                a: self.sref(s.g, ff),
                b: self.wref(l.w2(layer), d as u64 * ff as u64),
                dims: [1, ff, d, 0],
                ..Default::default()
            })?;
            self.run(Op {
                kind: op::ADD,
                dst: self.sref(s.x, d),
                a: self.sref(s.x, d),
                b: self.sref(s.tmp, d),
                dims: [d, 0, 0, 0],
                ..Default::default()
            })?;
        }
        self.run(Op {
            kind: op::RMSNORM,
            dst: self.sref(s.xn, d),
            a: self.sref(s.x, d),
            b: self.wref(l.final_norm(), d as u64),
            dims: [d, 0, 0, 0],
            ..Default::default()
        })?;
        self.run(Op {
            kind: op::MATMUL,
            dst: self.sref(s.logits, h.vocab),
            a: self.sref(s.xn, d),
            b: self.wref(l.lm_head(), h.vocab as u64 * d as u64),
            dims: [1, d, h.vocab, 0],
            ..Default::default()
        })?;
        let best = self.run(Op {
            kind: op::ARGMAX,
            a: self.sref(s.logits, h.vocab),
            dims: [h.vocab, 0, 0, 0],
            ..Default::default()
        })?;
        Ok(best as u32)
    }
}

fn run(root: Handle, compute_channel: Handle) -> Result<(), String> {
    reject_bad_models(root)?;
    let c = Compute::connect(compute_channel).map_err(|e| alloc::format!("compute connect: {e}"))?;
    let (backend, abi, _limit) = c.device_query().map_err(|e| alloc::format!("device query: {e}"))?;
    println!("[ai] compute backend {backend}, ABI v{abi}");
    let queue = c.queue_create().map_err(|e| alloc::format!("queue: {e}"))?;

    let model = load_model(root, &c)?;
    let h = model.header;
    let s = scratch_layout(&h);
    let cache_floats = h.n_layers as usize * h.n_heads as usize * h.max_seq as usize * h.head_dim as usize;
    let kcache = c.buffer_create(cache_floats * 4).map_err(|e| alloc::format!("k cache: {e}"))?;
    let vcache = c.buffer_create(cache_floats * 4).map_err(|e| alloc::format!("v cache: {e}"))?;
    let scratch = c.buffer_create(s.floats as usize * 4).map_err(|e| alloc::format!("scratch: {e}"))?;
    let working_set = model.weights.len + kcache.len + vcache.len + scratch.len;
    println!("[ai] working set: {} KiB in {} compute buffers", working_set / 1024, 4);

    let baseline_text = read_text(root, BASELINE_PATH)?;
    let prompt_hex =
        field(&baseline_text, "prompt_hex").ok_or_else(|| String::from("baseline has no prompt"))?;
    let mut prompt = Vec::new();
    for pair in prompt_hex.as_bytes().chunks(2) {
        let hex = core::str::from_utf8(pair).map_err(|_| String::from("bad prompt hex"))?;
        prompt.push(u8::from_str_radix(hex, 16).map_err(|_| String::from("bad prompt hex"))? as u32);
    }
    let expect: Vec<u32> = field(&baseline_text, "tokens")
        .ok_or_else(|| String::from("baseline has no tokens"))?
        .split(',')
        .filter_map(|t| t.trim().parse().ok())
        .collect();
    if prompt.is_empty() || expect.is_empty() {
        return Err(String::from("baseline is empty"));
    }
    println!("[ai] baseline: {} prompt bytes, {} tokens to generate", prompt.len(), expect.len());

    let rt = Runtime { c: &c, queue, h, w: model.weights, kcache, vcache, scratch, s };
    let total = prompt.len() + expect.len();
    if total > h.max_seq as usize {
        return Err(alloc::format!("{total} positions exceed the model's context of {}", h.max_seq));
    }

    let start = sys::ticks_ms();
    let mut ttft = 0u64;
    let mut token = prompt[0];
    let mut produced: Vec<u32> = Vec::new();
    for pos in 0..total {
        let best = rt.step(token, pos as u32)?;
        if pos + 1 >= prompt.len() {
            if produced.is_empty() {
                ttft = sys::ticks_ms() - start;
            }
            produced.push(best);
            if produced.len() == expect.len() {
                break;
            }
            token = best;
        } else {
            token = prompt[pos + 1];
        }
    }
    let elapsed = (sys::ticks_ms() - start).max(1);

    for (i, (got, want)) in produced.iter().zip(expect.iter()).enumerate() {
        if got != want {
            return Err(alloc::format!("token {i} is {got}, baseline says {want}"));
        }
    }
    let info = sys::self_info().map_err(|e| alloc::format!("self_info: {e}"))?;
    let generate_ms = elapsed - ttft;
    let tokens_per_1000s = produced.len() as u64 * 1000 * 1000 / generate_ms.max(1);
    println!("[ai] generated {} tokens offline, all matching the pinned baseline", produced.len());
    println!(
        "[ai] metrics: ttft={ttft} ms, total={elapsed} ms, {}.{:03} tokens/s, working set {} KiB, runtime rss {} KiB",
        tokens_per_1000s / 1000,
        tokens_per_1000s % 1000,
        working_set / 1024,
        info.used_pages * 4
    );
    let preview: Vec<u8> =
        produced.iter().take(24).map(|t| if (32..127).contains(t) { *t as u8 } else { b'.' }).collect();
    println!("[ai] first tokens: {:?} ...", core::str::from_utf8(&preview).unwrap_or(""));
    c.shutdown().ok();
    Ok(())
}

#[unsafe(no_mangle)]
pub extern "C" fn space_main() -> i32 {
    println!("[ai] Space OS AI runtime starting");
    // The parent hands over two capabilities: a compute connection and read access
    // to the file system.
    let mut buf = [0u8; 32];
    let mut compute_channel = None;
    let mut root = None;
    for _ in 0..2 {
        match sys::recv(handle::BOOTSTRAP, &mut buf, false) {
            Ok((n, Some(h))) => match core::str::from_utf8(&buf[..n]).unwrap_or("") {
                "compute" => compute_channel = Some(h),
                "fs" => root = Some(h),
                other => {
                    println!("[ai] unexpected capability '{other}'");
                    return 2;
                }
            },
            Ok((n, None)) => {
                println!("[ai] message without a capability ({n} bytes)");
                return 2;
            }
            Err(e) => {
                println!("[ai] bootstrap failed: {e:?}");
                return 2;
            }
        }
    }
    let (Some(compute_channel), Some(root)) = (compute_channel, root) else {
        println!("[ai] missing capabilities");
        return 2;
    };
    match run(root, compute_channel) {
        Ok(()) => {
            println!("[ai] result=PASS");
            0
        }
        Err(e) => {
            println!("[ai] result=FAIL {e}");
            1
        }
    }
}
