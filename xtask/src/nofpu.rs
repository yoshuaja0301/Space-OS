//! FP/SIMD instructions only where they belong (ADR-0031): x87, MMX, SSE and AVX on
//! x86-64, FP/SIMD on AArch64.
//!
//! Every thread's FP/SIMD registers are its own now, but the kernel and every
//! ordinary program are still built soft-float: the kernel because nothing may touch
//! a thread's registers except their save and load on a switch, the programs because
//! the inference baseline is bit for bit the same on both architectures in software
//! FP. This check makes sure nothing brought such an instruction in anyway (an
//! assembly block, a library that detects the CPU at run time), by decoding every
//! instruction of every executable segment, and holds each binary to its policy:
//!
//! - the kernel: only the instructions that save and load a thread's state
//!   (FXSAVE/FXRSTOR/XSAVE/XRSTOR/XSETBV; on AArch64 FP/SIMD register loads and
//!   stores and FPCR/FPSR access, never arithmetic);
//! - `fpu`, the K03 test of that state: any;
//! - `fault`: exactly the instructions it runs to raise FP exceptions on purpose;
//! - every other program: none.

use iced_x86::{Decoder, DecoderOptions, Formatter, GasFormatter, Instruction, OpKind, Register};
use spaceabi::elf::Elf;

/// Instruction-set extensions that need the switched-off units, as prefixes of the
/// names the decoder gives them (`SSE` covers SSE2 to SSE4.2, `AVX` every AVX-512
/// subset, `FPU` the 287 and 387 variants, and so on).
const FORBIDDEN: &[&str] = &[
    "FPU",
    "MMX",
    "SSE",
    "SSSE3",
    "AVX",
    "FMA",
    "F16C",
    "AES",
    "PCLMULQDQ",
    "SHA",
    "VAES",
    "VPCLMULQDQ",
    "GFNI",
    "FXSR",
    "XSAVE",
    "D3NOW",
    "AMX",
    "KNC",
    "CYRIX_FPU",
    "EMMI",
];

fn vector_or_fpu(r: Register) -> bool {
    r.is_xmm() || r.is_ymm() || r.is_zmm() || r.is_mm() || r.is_st() || r.is_k() || r.is_tmm()
}

fn forbidden(i: &Instruction) -> Option<String> {
    if let Some(f) = i
        .cpuid_features()
        .iter()
        .map(|f| format!("{f:?}"))
        .find(|f| FORBIDDEN.iter().any(|p| f.starts_with(p)))
    {
        return Some(f);
    }
    let registers = (0..i.op_count())
        .filter(|&n| i.op_kind(n) == OpKind::Register)
        .map(|n| i.op_register(n))
        .chain([i.memory_base(), i.memory_index()]);
    for r in registers {
        if vector_or_fpu(r) {
            return Some(format!("{r:?}"));
        }
    }
    None
}

/// What a binary may contain.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Policy {
    /// Not one FP/SIMD instruction.
    None,
    /// Exactly this many.
    Exactly(usize),
    /// As many as it likes: the test of FP/SIMD state.
    Any,
    /// Only saving and loading a thread's state.
    StateOnly,
}

/// x86-64: `fault` raises an SSE exception (LDMXCSR, PXOR, MOVD, DIVSS) and an x87
/// one (FLDCW, FLD1, FLDZ, FDIVP; the FWAIT after them is no FPU instruction to the
/// decoder) on purpose.
fn policy_x86(name: &str) -> Policy {
    match name {
        "spacekernel" => Policy::StateOnly,
        "fpu" => Policy::Any,
        "fault" => Policy::Exactly(8),
        _ => Policy::None,
    }
}

/// AArch64: FP exceptions do not trap there, so `fault` has none.
fn policy_a64(name: &str) -> Policy {
    match name {
        "spacekernel" => Policy::StateOnly,
        "fpu" => Policy::Any,
        _ => Policy::None,
    }
}

/// The x86-64 instructions that save and load FP/SIMD state, and nothing else.
fn state_instruction(i: &Instruction) -> bool {
    use iced_x86::Mnemonic::{Fxrstor64, Fxsave64, Xrstor64, Xsave64, Xsetbv};
    matches!(i.mnemonic(), Fxsave64 | Fxrstor64 | Xsave64 | Xrstor64 | Xsetbv)
}

/// What a binary turned out to contain, for the build's summary line.
pub struct Found {
    pub decoded: usize,
    pub fp: usize,
}

fn judge(name: &str, policy: Policy, found: &[String], disallowed: &[String]) -> Result<(), String> {
    let shown = |v: &[String]| v.iter().take(8).cloned().collect::<Vec<_>>().join("; ");
    match policy {
        Policy::Any => Ok(()),
        Policy::StateOnly if disallowed.is_empty() => Ok(()),
        Policy::StateOnly => Err(format!(
            "{name}: {} FP/SIMD instruction(s) besides saving and loading a thread's state: {}",
            disallowed.len(),
            shown(disallowed)
        )),
        Policy::None if found.is_empty() => Ok(()),
        Policy::Exactly(n) if found.len() == n => Ok(()),
        Policy::None | Policy::Exactly(_) => Err(format!(
            "{name}: {} FP/SIMD instruction(s), expected {}: {}",
            found.len(),
            if let Policy::Exactly(n) = policy { n } else { 0 },
            shown(found)
        )),
    }
}

/// Decode every executable segment of `image`: how many instructions there are,
/// and each one that needs the switched-off units (or is no instruction at all).
fn scan(image: &[u8]) -> Result<(usize, Vec<String>, Vec<String>), String> {
    let elf = Elf::parse(image).map_err(|e| format!("not an executable: {e:?}"))?;
    let mut count = 0;
    let mut found = Vec::new();
    let mut not_state = Vec::new();
    let mut fmt = GasFormatter::new();
    for seg in elf.load_segments() {
        let seg = seg.map_err(|e| format!("{e:?}"))?;
        if !seg.executable() {
            continue;
        }
        let mut d = Decoder::with_ip(64, elf.segment_data(&seg), seg.vaddr, DecoderOptions::NONE);
        let mut i = Instruction::default();
        while d.can_decode() {
            d.decode_out(&mut i);
            count += 1;
            let why = if i.is_invalid() { Some(String::from("not an instruction")) } else { forbidden(&i) };
            if let Some(why) = why {
                let mut text = String::new();
                fmt.format(&i, &mut text);
                let line = format!("{:#x}: {text} ({why})", i.ip());
                if i.is_invalid() || !state_instruction(&i) {
                    not_state.push(line.clone());
                }
                found.push(line);
            }
        }
    }
    Ok((count, found, not_state))
}

/// Check the executable `name` against its policy.
pub fn check(name: &str, image: &[u8]) -> Result<Found, String> {
    let (decoded, found, not_state) = scan(image).map_err(|e| format!("{name}: {e}"))?;
    judge(name, policy_x86(name), &found, &not_state)?;
    Ok(Found { decoded, fp: found.len() })
}

const A64_DATA_PROCESSING: &str = "FP/SIMD data processing";

/// The FP/SIMD instructions of an AArch64 word, if it is one: the scalar FP and
/// Advanced SIMD data-processing group, FP/SIMD register loads and stores, and
/// reads or writes of FPCR and FPSR.
fn a64_forbidden(w: u32) -> Option<&'static str> {
    // op0 (bits 28:25) = x111: data processing, scalar FP and Advanced SIMD.
    if (w >> 25) & 0b0111 == 0b0111 {
        return Some(A64_DATA_PROCESSING);
    }
    // op0 = x1x0: loads and stores; bit 26 (V) set means FP/SIMD registers.
    if w & (1 << 27) != 0 && w & (1 << 25) == 0 && w & (1 << 26) != 0 {
        return Some("FP/SIMD load or store");
    }
    // MRS/MSR of FPCR (S3_3_C4_C4_0) or FPSR (S3_3_C4_C4_1), any register.
    if matches!(w & 0xFFFF_FFE0, 0xD53B_4400 | 0xD53B_4420 | 0xD51B_4400 | 0xD51B_4420) {
        return Some("FPCR/FPSR access");
    }
    None
}

/// [`check`] for an AArch64 executable: every word of every executable segment.
pub fn check_a64(name: &str, image: &[u8]) -> Result<Found, String> {
    let elf = Elf::parse_for(image, spaceabi::elf::EM_AARCH64)
        .map_err(|e| format!("{name}: not an AArch64 executable: {e:?}"))?;
    let mut count = 0;
    let mut found = Vec::new();
    let mut not_state = Vec::new();
    for seg in elf.load_segments() {
        let seg = seg.map_err(|e| format!("{name}: {e:?}"))?;
        if !seg.executable() {
            continue;
        }
        for (i, word) in elf.segment_data(&seg).chunks_exact(4).enumerate() {
            let w = u32::from_le_bytes([word[0], word[1], word[2], word[3]]);
            count += 1;
            if let Some(why) = a64_forbidden(w) {
                let line = format!("{:#x}: {w:#010x} ({why})", seg.vaddr + 4 * i as u64);
                // Saving and loading state moves registers to and from memory and
                // reads or writes FPCR/FPSR; it never computes.
                if why == A64_DATA_PROCESSING {
                    not_state.push(line.clone());
                }
                found.push(line);
            }
        }
    }
    judge(name, policy_a64(name), &found, &not_state)?;
    Ok(Found { decoded: count, fp: found.len() })
}
