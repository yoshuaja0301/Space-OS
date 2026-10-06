//! SpaceLM v0: the model container Space OS loads for native inference.
//!
//! A deliberately small, fully specified format. The header fixes every dimension,
//! the tensor order is fixed, and all weights are little-endian `f32`, so the file
//! can be mapped into a compute buffer and used without a parser in the hot path.
//! Both the host tool that produces a model and the guest runtime that consumes it
//! use the layout below, so neither can drift from the other.

pub const MAGIC: [u8; 4] = *b"SLM0";
pub const VERSION: u32 = 0;
/// Decoder-only transformer with RMSNorm, rotary embeddings and a SwiGLU feed
/// forward - the shape the first CPU backend targets (PRD §4).
pub const ARCH_SPACELM: u32 = 1;

/// Limits the runtime enforces before it allocates anything.
pub const MAX_LAYERS: u32 = 8;
pub const MAX_D_MODEL: u32 = 512;
pub const MAX_VOCAB: u32 = 4096;
pub const MAX_SEQ: u32 = 1024;
pub const MAX_D_FF: u32 = 2048;

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Header {
    pub magic: [u8; 4],
    pub version: u32,
    pub arch: u32,
    pub n_layers: u32,
    pub d_model: u32,
    pub n_heads: u32,
    pub head_dim: u32,
    pub d_ff: u32,
    pub vocab: u32,
    pub max_seq: u32,
    pub rope_theta: f32,
    pub reserved: [u32; 5],
}

pub const HEADER_BYTES: usize = core::mem::size_of::<Header>();

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ModelError {
    TooShort,
    BadMagic,
    BadVersion,
    BadArch,
    BadDims,
    BadSize,
}

impl core::fmt::Display for ModelError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(match self {
            ModelError::TooShort => "file shorter than the header",
            ModelError::BadMagic => "not a SpaceLM model",
            ModelError::BadVersion => "unsupported model version",
            ModelError::BadArch => "unsupported architecture",
            ModelError::BadDims => "dimensions out of range",
            ModelError::BadSize => "file size does not match the declared tensors",
        })
    }
}

impl Header {
    /// Parse and validate a header, rejecting anything the runtime would not be
    /// able to allocate or index safely.
    pub fn parse(bytes: &[u8], file_len: u64) -> Result<Header, ModelError> {
        if bytes.len() < HEADER_BYTES {
            return Err(ModelError::TooShort);
        }
        // SAFETY: `Header` is `repr(C)` plain data and the slice is long enough.
        let h: Header = unsafe { core::ptr::read_unaligned(bytes.as_ptr() as *const Header) };
        if h.magic != MAGIC {
            return Err(ModelError::BadMagic);
        }
        if h.version != VERSION {
            return Err(ModelError::BadVersion);
        }
        if h.arch != ARCH_SPACELM {
            return Err(ModelError::BadArch);
        }
        if h.n_layers == 0
            || h.n_layers > MAX_LAYERS
            || h.d_model == 0
            || h.d_model > MAX_D_MODEL
            || h.n_heads == 0
            || h.head_dim == 0
            || h.n_heads * h.head_dim != h.d_model
            || h.d_ff == 0
            || h.d_ff > MAX_D_FF
            || h.vocab < 2
            || h.vocab > MAX_VOCAB
            || h.max_seq == 0
            || h.max_seq > MAX_SEQ
            || !(h.rope_theta.is_finite() && h.rope_theta > 1.0)
        {
            return Err(ModelError::BadDims);
        }
        let expect = HEADER_BYTES as u64 + h.weight_floats() * 4;
        if expect != file_len {
            return Err(ModelError::BadSize);
        }
        Ok(h)
    }

    /// Number of `f32` weights after the header.
    pub const fn weight_floats(&self) -> u64 {
        let d = self.d_model as u64;
        let ff = self.d_ff as u64;
        let per_layer = d          // attn_norm
            + 4 * d * d            // wq, wk, wv, wo
            + d                    // ffn_norm
            + 2 * ff * d           // w1, w3
            + d * ff; // w2
        self.vocab as u64 * d + self.n_layers as u64 * per_layer + d + self.vocab as u64 * d
    }

    pub const fn layout(&self) -> Layout {
        Layout { h: *self }
    }
}

/// Offsets of every tensor, in `f32` units from the start of the weight block.
#[derive(Clone, Copy, Debug)]
pub struct Layout {
    h: Header,
}

impl Layout {
    const fn d(&self) -> u64 {
        self.h.d_model as u64
    }
    const fn ff(&self) -> u64 {
        self.h.d_ff as u64
    }

    pub const fn tok_emb(&self) -> u64 {
        0
    }

    const fn per_layer(&self) -> u64 {
        self.d() + 4 * self.d() * self.d() + self.d() + 2 * self.ff() * self.d() + self.d() * self.ff()
    }

    const fn layer_base(&self, layer: u32) -> u64 {
        self.h.vocab as u64 * self.d() + layer as u64 * self.per_layer()
    }

    pub const fn attn_norm(&self, layer: u32) -> u64 {
        self.layer_base(layer)
    }
    pub const fn wq(&self, layer: u32) -> u64 {
        self.attn_norm(layer) + self.d()
    }
    pub const fn wk(&self, layer: u32) -> u64 {
        self.wq(layer) + self.d() * self.d()
    }
    pub const fn wv(&self, layer: u32) -> u64 {
        self.wk(layer) + self.d() * self.d()
    }
    pub const fn wo(&self, layer: u32) -> u64 {
        self.wv(layer) + self.d() * self.d()
    }
    pub const fn ffn_norm(&self, layer: u32) -> u64 {
        self.wo(layer) + self.d() * self.d()
    }
    pub const fn w1(&self, layer: u32) -> u64 {
        self.ffn_norm(layer) + self.d()
    }
    pub const fn w3(&self, layer: u32) -> u64 {
        self.w1(layer) + self.ff() * self.d()
    }
    pub const fn w2(&self, layer: u32) -> u64 {
        self.w3(layer) + self.ff() * self.d()
    }
    pub const fn final_norm(&self) -> u64 {
        self.layer_base(self.h.n_layers)
    }
    pub const fn lm_head(&self) -> u64 {
        self.final_norm() + self.d()
    }
}
