//! Host-side reference implementation of SpaceLM inference.
//!
//! It produces the model (deterministic weights from a pinned seed) and the token
//! sequence the guest must reproduce exactly. The arithmetic uses the same shared
//! `spaceabi::math` helpers and the same operation order as the guest's compute
//! backend, so the baseline is a real check of the guest pipeline rather than a
//! comparison of two different numeric libraries.

use spaceabi::math::{cos_f32, exp_f32, powf_f32, silu_f32, sin_f32, sqrt_f32};
use spaceabi::model::{ARCH_SPACELM, HEADER_BYTES, Header, MAGIC, VERSION};

/// Pinned configuration of the reference model.
pub const SEED: u64 = 0x5c4e_3a6b_11ff_c7db;
pub const PROMPT: &[u8] = b"Space OS: ";
pub const GENERATE: usize = 128;
pub const ROPE_THETA: f32 = 10000.0;

pub fn config() -> Header {
    Header {
        magic: MAGIC,
        version: VERSION,
        arch: ARCH_SPACELM,
        n_layers: 2,
        d_model: 64,
        n_heads: 4,
        head_dim: 16,
        d_ff: 128,
        vocab: 256,
        max_seq: 256,
        rope_theta: ROPE_THETA,
        reserved: [0; 5],
    }
}

struct Rng(u64);

impl Rng {
    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }

    /// Uniform in `[-scale, scale)`, deterministic and identical on any host.
    fn next_f32(&mut self, scale: f32) -> f32 {
        let bits = (self.next_u64() >> 40) as u32; // 24 bits
        let unit = bits as f32 / 16_777_216.0; // [0, 1)
        (unit * 2.0 - 1.0) * scale
    }
}

/// Deterministic weights for the reference model.
pub fn weights(h: &Header) -> Vec<f32> {
    weights_seeded(h, SEED)
}

/// Weights from an explicit seed (used by `cargo xtask seedsearch` to choose the
/// pinned [`SEED`]: an untrained model can fall into a single repeated token, which
/// would make the baseline a weak check).
pub fn weights_seeded(h: &Header, seed: u64) -> Vec<f32> {
    let mut rng = Rng(seed);
    let l = h.layout();
    let total = h.weight_floats() as usize;
    let mut w = vec![0.0f32; total];
    let d = h.d_model as usize;
    let ff = h.d_ff as usize;
    let scale = 1.0 / sqrt_f32(d as f32);

    for v in w.iter_mut() {
        *v = rng.next_f32(scale);
    }
    // Norm weights sit around 1.0, as in a trained model.
    let mut set_norm = |off: u64, n: usize, rng: &mut Rng| {
        for i in 0..n {
            w[off as usize + i] = 1.0 + rng.next_f32(0.05);
        }
    };
    for layer in 0..h.n_layers {
        set_norm(l.attn_norm(layer), d, &mut rng);
        set_norm(l.ffn_norm(layer), d, &mut rng);
    }
    set_norm(l.final_norm(), d, &mut rng);
    let _ = ff;
    w
}

/// Serialise header + weights, exactly what the guest reads from disk.
pub fn serialise(h: &Header, w: &[f32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(HEADER_BYTES + w.len() * 4);
    // SAFETY: `Header` is `repr(C)` plain data.
    let bytes = unsafe { std::slice::from_raw_parts(h as *const Header as *const u8, HEADER_BYTES) };
    out.extend_from_slice(bytes);
    for v in w {
        out.extend_from_slice(&v.to_le_bytes());
    }
    out
}

fn rmsnorm(dst: &mut [f32], x: &[f32], weight: &[f32], n: usize) {
    let sum: f32 = x[..n].iter().map(|v| v * v).sum();
    let scale = 1.0 / sqrt_f32(sum / n as f32 + 1e-5);
    for i in 0..n {
        dst[i] = x[i] * scale * weight[i];
    }
}

/// `dst[0..n] = a[0..k] . b[c][0..k]`, the same accumulation order as the service.
fn matvec(dst: &mut [f32], a: &[f32], b: &[f32], k: usize, n: usize) {
    for c in 0..n {
        let mut sum = 0.0f32;
        for i in 0..k {
            sum += a[i] * b[c * k + i];
        }
        dst[c] = sum;
    }
}

fn softmax(dst: &mut [f32], a: &[f32], n: usize) {
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
}

fn rope(x: &mut [f32], heads: usize, head_dim: usize, pos: usize) {
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
}

/// Greedy generation: the prompt is prefilled, then `GENERATE` tokens are produced.
/// Returns the generated tokens.
pub fn generate(h: &Header, w: &[f32], prompt: &[u8], n_new: usize) -> Vec<u32> {
    let l = h.layout();
    let d = h.d_model as usize;
    let ff = h.d_ff as usize;
    let heads = h.n_heads as usize;
    let hd = h.head_dim as usize;
    let vocab = h.vocab as usize;
    let max_seq = h.max_seq as usize;

    let mut k_cache = vec![0.0f32; h.n_layers as usize * heads * max_seq * hd];
    let mut v_cache_t = vec![0.0f32; h.n_layers as usize * heads * hd * max_seq];
    let mut x = vec![0.0f32; d];
    let mut xn = vec![0.0f32; d];
    let mut q = vec![0.0f32; d];
    let mut kv = vec![0.0f32; d];
    let mut vv = vec![0.0f32; d];
    let mut att = vec![0.0f32; d];
    let mut tmp = vec![0.0f32; d.max(ff)];
    let mut g = vec![0.0f32; ff];
    let mut u = vec![0.0f32; ff];
    let mut scores = vec![0.0f32; max_seq];
    let mut probs = vec![0.0f32; max_seq];
    let mut logits = vec![0.0f32; vocab];

    let mut out = Vec::with_capacity(n_new);
    let mut token = prompt[0] as u32;
    let total = prompt.len() + n_new;
    for pos in 0..total {
        x[..d].copy_from_slice(&w[l.tok_emb() as usize + token as usize * d..][..d]);
        for layer in 0..h.n_layers {
            rmsnorm(&mut xn, &x, &w[l.attn_norm(layer) as usize..], d);
            matvec(&mut q, &xn, &w[l.wq(layer) as usize..], d, d);
            matvec(&mut kv, &xn, &w[l.wk(layer) as usize..], d, d);
            matvec(&mut vv, &xn, &w[l.wv(layer) as usize..], d, d);
            rope(&mut q, heads, hd, pos);
            rope(&mut kv, heads, hd, pos);
            for head in 0..heads {
                let kbase = ((layer as usize * heads) + head) * max_seq * hd + pos * hd;
                k_cache[kbase..kbase + hd].copy_from_slice(&kv[head * hd..head * hd + hd]);
                let vbase = ((layer as usize * heads) + head) * hd * max_seq;
                for i in 0..hd {
                    v_cache_t[vbase + i * max_seq + pos] = vv[head * hd + i];
                }
            }
            let inv = 1.0 / sqrt_f32(hd as f32);
            for head in 0..heads {
                let kbase = ((layer as usize * heads) + head) * max_seq * hd;
                matvec(&mut scores, &q[head * hd..], &k_cache[kbase..], hd, pos + 1);
                for v in scores[..pos + 1].iter_mut() {
                    *v *= inv;
                }
                softmax(&mut probs, &scores, pos + 1);
                let vbase = ((layer as usize * heads) + head) * hd * max_seq;
                // `v_cache_t` rows are `max_seq` long; the matvec must stride by that.
                for c in 0..hd {
                    let mut sum = 0.0f32;
                    for i in 0..pos + 1 {
                        sum += probs[i] * v_cache_t[vbase + c * max_seq + i];
                    }
                    att[head * hd + c] = sum;
                }
            }
            matvec(&mut tmp, &att, &w[l.wo(layer) as usize..], d, d);
            for i in 0..d {
                x[i] += tmp[i];
            }
            rmsnorm(&mut xn, &x, &w[l.ffn_norm(layer) as usize..], d);
            matvec(&mut g, &xn, &w[l.w1(layer) as usize..], d, ff);
            matvec(&mut u, &xn, &w[l.w3(layer) as usize..], d, ff);
            for i in 0..ff {
                g[i] = silu_f32(g[i]) * u[i];
            }
            matvec(&mut tmp, &g, &w[l.w2(layer) as usize..], ff, d);
            for i in 0..d {
                x[i] += tmp[i];
            }
        }
        rmsnorm(&mut xn, &x, &w[l.final_norm() as usize..], d);
        matvec(&mut logits, &xn, &w[l.lm_head() as usize..], d, vocab);
        let mut best = 0usize;
        for i in 1..vocab {
            if logits[i] > logits[best] {
                best = i;
            }
        }
        let next = if pos + 1 < prompt.len() { prompt[pos + 1] as u32 } else { best as u32 };
        if pos + 1 >= prompt.len() {
            out.push(best as u32);
            if out.len() == n_new {
                break;
            }
        }
        token = next;
    }
    out
}
