//! No x87, MMX, SSE or AVX instruction anywhere the machine runs our code
//! (ADR-0017).
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
