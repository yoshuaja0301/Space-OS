//! No x87, MMX, SSE or AVX instruction anywhere the machine runs our code
//! (ADR-0017), and on AArch64 no FP/SIMD instruction (ADR-0028).
//!
//! The kernel keeps no floating-point or vector state per thread and switches those
//! units off (CR0.EM set; CR4.OSFXSR, OSXMMEXCPT and OSXSAVE clear), so such an
//! instruction faults: in a program it kills the program, in the kernel it is a
//! panic. Both targets are soft-float, so the compiler never picks one, and the
//! cryptography is forced onto its software paths -- this check makes sure nothing
//! brought one in anyway (an assembly block, a library that detects the CPU at run
//! time), by decoding every instruction of every executable segment.

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

/// Programs that use the switched-off units on purpose, and exactly how many such
/// instructions each contains: `fault` runs one x87 and one SSE instruction to prove
/// the kernel stops the program rather than crashing (K02).
const ON_PURPOSE: &[(&str, usize)] = &[("fault", 2)];

/// Decode every executable segment of `image`: how many instructions there are,
/// and each one that needs the switched-off units (or is no instruction at all).
fn scan(image: &[u8]) -> Result<(usize, Vec<String>), String> {
    let elf = Elf::parse(image).map_err(|e| format!("not an executable: {e:?}"))?;
    let mut count = 0;
    let mut found = Vec::new();
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
                found.push(format!("{:#x}: {text} ({why})", i.ip()));
            }
        }
    }
    Ok((count, found))
}

/// Check the executable `name`: returns how many instructions it has.
pub fn check(name: &str, image: &[u8]) -> Result<usize, String> {
    let (count, found) = scan(image).map_err(|e| format!("{name}: {e}"))?;
    let expected = ON_PURPOSE.iter().find(|(p, _)| *p == name).map_or(0, |(_, n)| *n);
    if found.len() == expected {
        return Ok(count);
    }
    let shown = found.iter().take(8).cloned().collect::<Vec<_>>().join("; ");
    Err(format!(
        "{name}: {} instruction(s) that need the FPU or vector units, which are off (expected {expected}): {shown}",
        found.len()
    ))
}

/// The FP/SIMD instructions of an AArch64 word, if it is one: the scalar FP and
/// Advanced SIMD data-processing group, FP/SIMD register loads and stores, and
/// reads or writes of FPCR and FPSR. With `CPACR_EL1.FPEN = 0` each of them traps.
fn a64_forbidden(w: u32) -> Option<&'static str> {
    // op0 (bits 28:25) = x111: data processing, scalar FP and Advanced SIMD.
    if (w >> 25) & 0b0111 == 0b0111 {
        return Some("FP/SIMD data processing");
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
pub fn check_a64(name: &str, image: &[u8]) -> Result<usize, String> {
    let elf = Elf::parse_for(image, spaceabi::elf::EM_AARCH64)
        .map_err(|e| format!("{name}: not an AArch64 executable: {e:?}"))?;
    let mut count = 0;
    let mut found = Vec::new();
    for seg in elf.load_segments() {
        let seg = seg.map_err(|e| format!("{name}: {e:?}"))?;
        if !seg.executable() {
            continue;
        }
        for (i, word) in elf.segment_data(&seg).chunks_exact(4).enumerate() {
            let w = u32::from_le_bytes([word[0], word[1], word[2], word[3]]);
            count += 1;
            if let Some(why) = a64_forbidden(w) {
                found.push(format!("{:#x}: {w:#010x} ({why})", seg.vaddr + 4 * i as u64));
            }
        }
    }
    let expected = ON_PURPOSE.iter().find(|(p, _)| *p == name).map_or(0, |(_, n)| *n);
    if found.len() == expected {
        return Ok(count);
    }
    let shown = found.iter().take(8).cloned().collect::<Vec<_>>().join("; ");
    Err(format!(
        "{name}: {} FP/SIMD instruction(s), and the unit is trapped (expected {expected}): {shown}",
        found.len()
    ))
}
