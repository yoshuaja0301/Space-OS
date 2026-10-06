//! FP/SIMD registers belong to the process that wrote them (PRD v0.2 K03, ADR-0031).
//!
//! `pattern <seed>` fills every vector register, the FP control and status registers
//! and, on x86-64, the top of the x87 stack with values made from the seed. For
//! 400 ms it then alternates between spinning, so the timer takes the CPU away, and
//! short sleeps, so it gives the CPU up -- checking after every round that each
//! register still holds exactly what it put there. Several run at once with
//! different seeds: a register loaded from the wrong area, or not loaded at all,
//! shows up as another seed's bytes or as zeros. `fresh` checks, before anything
//! touches them, that a new process starts with every register in its initial state:
//! nothing left by whoever ran on the CPU before.
//!
//! This is the one program in user space built to use the units (the build gate
//! allows it, ADR-0031); everything else stays soft-float.
#![no_std]
#![no_main]

use libspace::{handle, println, sys};

/// How long a pattern run lasts.
const RUN_MS: u64 = 400;
/// Spin iterations between checks: long enough that the timer preempts some rounds.
const SPIN: u64 = 200_000;

#[unsafe(no_mangle)]
pub extern "C" fn space_main() -> i32 {
    let mut buf = [0u8; 64];
    let Ok((len, _)) = sys::recv(handle::BOOTSTRAP, &mut buf, false) else { return 3 };
    let mode = core::str::from_utf8(&buf[..len]).unwrap_or("?");
    if mode == "fresh" {
        return fresh();
    }
    match mode.split_once(' ') {
        Some(("pattern", seed)) => match seed.parse::<u8>() {
            Ok(seed) => pattern(seed),
            Err(_) => 3,
        },
        _ => {
            println!("[fpu] unknown mode {mode:?}");
            3
        }
    }
}

/// The bytes register `r` holds for `seed`: no two seeds, and no two registers,
/// alike, and never all zero.
fn pattern_bytes(seed: u8, out: &mut [u8; 512]) {
    for (i, b) in out.iter_mut().enumerate() {
        *b = (i as u8).wrapping_mul(29).wrapping_add(seed.wrapping_mul(101)).wrapping_add((i >> 4) as u8) | 1;
    }
}

fn spin(n: u64) {
    let mut x = n;
    for _ in 0..n {
        x = x.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        core::hint::black_box(x);
    }
}

fn pattern(seed: u8) -> i32 {
    let mut want = [0u8; 512];
    pattern_bytes(seed, &mut want);
    let regs = arch::Regs::detect();
    let used = regs.bytes();
    regs.load(&want);
    arch::set_controls(seed);
    // What the CPU kept of what was asked: a bit it does not implement reads as 0.
    let controls = arch::controls();
    arch::x87_push(seed);
    let start = sys::ticks_ms();
    let mut checks = 0u32;
    let mut round = 0u32;
    while sys::ticks_ms().wrapping_sub(start) < RUN_MS {
        round += 1;
        if round.is_multiple_of(4) {
            sys::sleep_ms(1);
        } else {
            spin(SPIN);
        }
        let mut got = [0u8; 512];
        regs.store(&mut got);
        if got[..used] != want[..used] {
            let at = (0..used).find(|&i| got[i] != want[i]).unwrap_or(0);
            println!(
                "[fpu] pattern {seed}: {} {} changed after {checks} checks: byte {} is {:#04x}, not {:#04x}",
                regs.register_name(),
                at / regs.register_bytes(),
                at % regs.register_bytes(),
                got[at],
                want[at]
            );
            return 1;
        }
        let now = arch::controls();
        if now != controls {
            println!(
                "[fpu] pattern {seed}: control/status registers changed after {checks} checks: {now:x?}, not {controls:x?}"
            );
            return 1;
        }
        if let Some(bad) = arch::x87_check(seed) {
            println!("[fpu] pattern {seed}: {bad} after {checks} checks");
            return 1;
        }
        checks += 1;
    }
    println!("[fpu] pattern {seed}: {} held through {checks} checks in {RUN_MS} ms", regs.describe());
    0
}

fn fresh() -> i32 {
    match arch::initial_state() {
        Ok(what) => {
            println!("[fpu] fresh: {what} in the initial state");
            0
        }
        Err(e) => {
            println!("[fpu] fresh: {e}");
            2
        }
    }
}

#[cfg(target_arch = "x86_64")]
mod arch {
    use core::arch::asm;

    /// The vector registers the CPU has for user space: XMM0-15, or YMM0-15 when
    /// the kernel switched AVX on (XCR0) and the CPU has it.
    pub struct Regs {
        avx: bool,
    }

    impl Regs {
        pub fn detect() -> Regs {
            let l1 = core::arch::x86_64::__cpuid(1);
            if l1.ecx & (1 << 27) == 0 || l1.ecx & (1 << 28) == 0 {
                return Regs { avx: false };
            }
            let xcr0: u32;
            // SAFETY: OSXSAVE is set (CPUID above), so XGETBV is allowed in ring 3.
            unsafe { asm!("xgetbv", in("ecx") 0, out("eax") xcr0, out("edx") _, options(nomem, nostack)) };
            Regs { avx: xcr0 & 0b110 == 0b110 }
        }

        pub fn register_bytes(&self) -> usize {
            if self.avx { 32 } else { 16 }
        }

        pub fn bytes(&self) -> usize {
            16 * self.register_bytes()
        }

        pub fn register_name(&self) -> &'static str {
            if self.avx { "YMM" } else { "XMM" }
        }

        pub fn describe(&self) -> &'static str {
            if self.avx {
                "YMM0-15 (AVX), MXCSR, the x87 control word and the x87 stack"
            } else {
                "XMM0-15, MXCSR, the x87 control word and the x87 stack"
            }
        }

        pub fn load(&self, v: &[u8; 512]) {
            let p = v.as_ptr();
            // SAFETY: reads 256 or 512 bytes of `v`; only vector registers change.
            unsafe {
                if self.avx {
                    asm!(
                        "vmovdqu ymm0, [{p}]", "vmovdqu ymm1, [{p} + 32]", "vmovdqu ymm2, [{p} + 64]",
                        "vmovdqu ymm3, [{p} + 96]", "vmovdqu ymm4, [{p} + 128]", "vmovdqu ymm5, [{p} + 160]",
                        "vmovdqu ymm6, [{p} + 192]", "vmovdqu ymm7, [{p} + 224]", "vmovdqu ymm8, [{p} + 256]",
                        "vmovdqu ymm9, [{p} + 288]", "vmovdqu ymm10, [{p} + 320]", "vmovdqu ymm11, [{p} + 352]",
                        "vmovdqu ymm12, [{p} + 384]", "vmovdqu ymm13, [{p} + 416]", "vmovdqu ymm14, [{p} + 448]",
                        "vmovdqu ymm15, [{p} + 480]",
                        p = in(reg) p, options(nostack, readonly)
                    );
                } else {
                    asm!(
                        "movdqu xmm0, [{p}]", "movdqu xmm1, [{p} + 16]", "movdqu xmm2, [{p} + 32]",
                        "movdqu xmm3, [{p} + 48]", "movdqu xmm4, [{p} + 64]", "movdqu xmm5, [{p} + 80]",
                        "movdqu xmm6, [{p} + 96]", "movdqu xmm7, [{p} + 112]", "movdqu xmm8, [{p} + 128]",
                        "movdqu xmm9, [{p} + 144]", "movdqu xmm10, [{p} + 160]", "movdqu xmm11, [{p} + 176]",
                        "movdqu xmm12, [{p} + 192]", "movdqu xmm13, [{p} + 208]", "movdqu xmm14, [{p} + 224]",
                        "movdqu xmm15, [{p} + 240]",
                        p = in(reg) p, options(nostack, readonly)
                    );
                }
            }
        }

        pub fn store(&self, v: &mut [u8; 512]) {
            let p = v.as_mut_ptr();
            // SAFETY: writes 256 or 512 bytes of `v`.
            unsafe {
                if self.avx {
                    asm!(
                        "vmovdqu [{p}], ymm0", "vmovdqu [{p} + 32], ymm1", "vmovdqu [{p} + 64], ymm2",
                        "vmovdqu [{p} + 96], ymm3", "vmovdqu [{p} + 128], ymm4", "vmovdqu [{p} + 160], ymm5",
                        "vmovdqu [{p} + 192], ymm6", "vmovdqu [{p} + 224], ymm7", "vmovdqu [{p} + 256], ymm8",
                        "vmovdqu [{p} + 288], ymm9", "vmovdqu [{p} + 320], ymm10", "vmovdqu [{p} + 352], ymm11",
                        "vmovdqu [{p} + 384], ymm12", "vmovdqu [{p} + 416], ymm13", "vmovdqu [{p} + 448], ymm14",
                        "vmovdqu [{p} + 480], ymm15",
                        p = in(reg) p, options(nostack)
                    );
                } else {
                    asm!(
                        "movdqu [{p}], xmm0", "movdqu [{p} + 16], xmm1", "movdqu [{p} + 32], xmm2",
                        "movdqu [{p} + 48], xmm3", "movdqu [{p} + 64], xmm4", "movdqu [{p} + 80], xmm5",
                        "movdqu [{p} + 96], xmm6", "movdqu [{p} + 112], xmm7", "movdqu [{p} + 128], xmm8",
                        "movdqu [{p} + 144], xmm9", "movdqu [{p} + 160], xmm10", "movdqu [{p} + 176], xmm11",
                        "movdqu [{p} + 192], xmm12", "movdqu [{p} + 208], xmm13", "movdqu [{p} + 224], xmm14",
                        "movdqu [{p} + 240], xmm15",
                        p = in(reg) p, options(nostack)
                    );
                }
            }
        }
    }

    /// MXCSR with every exception masked, its rounding mode and flush-to-zero from
    /// the seed; the x87 control word with every exception masked and its rounding
    /// mode from the seed too.
    pub fn set_controls(seed: u8) {
        let s = u32::from(seed);
        let mxcsr: u32 = 0x1F80 | ((s & 3) << 13) | (((s >> 2) & 1) << 15) | (s & 1);
        let fcw: u16 = (0x037F & !(3 << 10)) | ((u16::from(seed).wrapping_add(1) & 3) << 10);
        // SAFETY: only defined bits of MXCSR, and every exception stays masked.
        unsafe {
            asm!("ldmxcsr [{m}]", "fldcw [{c}]", m = in(reg) &mxcsr, c = in(reg) &fcw, options(nostack, readonly))
        };
    }

    /// `(MXCSR, x87 control word)`.
    pub fn controls() -> (u32, u16) {
        let mut mxcsr = 0u32;
        let mut fcw = 0u16;
        // SAFETY: stores into the two locals.
        unsafe {
            asm!("stmxcsr [{m}]", "fnstcw [{c}]", m = in(reg) &mut mxcsr, c = in(reg) &mut fcw, options(nostack))
        };
        (mxcsr, fcw)
    }

    fn x87_bits(seed: u8) -> u64 {
        // 2.0 plus a seed-sized nudge: a double the x87 stack holds exactly.
        0x4000_0000_0000_0000 | (u64::from(seed) << 20) | 0x5A5A
    }

    pub fn x87_push(seed: u8) {
        let v = x87_bits(seed);
        // SAFETY: loads a double onto the x87 stack (the stack was empty).
        unsafe { asm!("fld qword ptr [{p}]", p = in(reg) &v, options(nostack, readonly)) };
    }

    /// `None` when the top of the x87 stack still holds the seed's value.
    pub fn x87_check(seed: u8) -> Option<&'static str> {
        let mut v = 0u64;
        // SAFETY: stores the top of the x87 stack without popping it.
        unsafe { asm!("fst qword ptr [{p}]", p = in(reg) &mut v, options(nostack)) };
        (v != x87_bits(seed)).then_some("the top of the x87 stack changed")
    }

    #[repr(C, align(64))]
    struct Area([u8; 512]);

    /// Every register in the initial state: x87 control word 0x37F, status 0, tag
    /// word "all empty", MXCSR 0x1F80, ST0-7 and XMM0-15 zero -- and, with AVX, the
    /// upper halves of YMM0-15 zero.
    pub fn initial_state() -> Result<&'static str, alloc_free::Text> {
        let regs = Regs::detect();
        let mut area = Area([0; 512]);
        // SAFETY: a 512-byte, 16-byte aligned area; FXSAVE is allowed in ring 3.
        unsafe { asm!("fxsave64 [{p}]", p = in(reg) area.0.as_mut_ptr(), options(nostack)) };
        let a = &area.0;
        let fcw = u16::from_le_bytes([a[0], a[1]]);
        let fsw = u16::from_le_bytes([a[2], a[3]]);
        let ftw = a[4];
        let mxcsr = u32::from_le_bytes([a[24], a[25], a[26], a[27]]);
        if fcw != 0x037F || fsw != 0 || ftw != 0 || mxcsr != 0x1F80 {
            return Err(alloc_free::Text::controls(fcw, fsw, ftw, mxcsr));
        }
        if let Some(i) = (32..160).find(|&i| a[i] != 0) {
            return Err(alloc_free::Text::register("ST", (i - 32) / 16));
        }
        if let Some(i) = (160..416).find(|&i| a[i] != 0) {
            return Err(alloc_free::Text::register("XMM", (i - 160) / 16));
        }
        if regs.avx {
            let mut ymm = [0u8; 512];
            regs.store(&mut ymm);
            if let Some(i) = (0..512).find(|&i| ymm[i] != 0) {
                return Err(alloc_free::Text::register("YMM", i / 32));
            }
            return Ok("x87, MXCSR and YMM0-15 (AVX)");
        }
        Ok("x87, MXCSR and XMM0-15")
    }

    /// A failure to report, without needing an allocator for the message.
    pub mod alloc_free {
        pub enum Text {
            Controls { fcw: u16, fsw: u16, ftw: u8, mxcsr: u32 },
            Register { name: &'static str, index: usize },
        }

        impl Text {
            pub fn controls(fcw: u16, fsw: u16, ftw: u8, mxcsr: u32) -> Text {
                Text::Controls { fcw, fsw, ftw, mxcsr }
            }

            pub fn register(name: &'static str, index: usize) -> Text {
                Text::Register { name, index }
            }
        }

        impl core::fmt::Display for Text {
            fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
                match self {
                    Text::Controls { fcw, fsw, ftw, mxcsr } => write!(
                        f,
                        "control/status not initial: FCW {fcw:#06x} FSW {fsw:#06x} FTW {ftw:#04x} MXCSR {mxcsr:#010x}"
                    ),
                    Text::Register { name, index } => write!(f, "{name}{index} is not zero"),
                }
            }
        }
    }
}

#[cfg(target_arch = "aarch64")]
mod arch {
    use core::arch::asm;

    /// V0-V31: the AArch64 FP/SIMD registers.
    pub struct Regs;

    impl Regs {
        pub fn detect() -> Regs {
            Regs
        }

        pub fn register_bytes(&self) -> usize {
            16
        }

        pub fn bytes(&self) -> usize {
            512
        }

        pub fn register_name(&self) -> &'static str {
            "V"
        }

        pub fn describe(&self) -> &'static str {
            "V0-V31, FPCR and FPSR"
        }

        pub fn load(&self, v: &[u8; 512]) {
            // SAFETY: reads 512 bytes of `v`; only FP/SIMD registers change.
            unsafe {
                asm!(
                    ".arch_extension fp", ".arch_extension simd",
                    "ldp q0, q1, [{p}]", "ldp q2, q3, [{p}, #32]", "ldp q4, q5, [{p}, #64]",
                    "ldp q6, q7, [{p}, #96]", "ldp q8, q9, [{p}, #128]", "ldp q10, q11, [{p}, #160]",
                    "ldp q12, q13, [{p}, #192]", "ldp q14, q15, [{p}, #224]", "ldp q16, q17, [{p}, #256]",
                    "ldp q18, q19, [{p}, #288]", "ldp q20, q21, [{p}, #320]", "ldp q22, q23, [{p}, #352]",
                    "ldp q24, q25, [{p}, #384]", "ldp q26, q27, [{p}, #416]", "ldp q28, q29, [{p}, #448]",
                    "ldp q30, q31, [{p}, #480]",
                    p = in(reg) v.as_ptr(), options(nostack, readonly)
                )
            };
        }

        pub fn store(&self, v: &mut [u8; 512]) {
            // SAFETY: writes 512 bytes of `v`.
            unsafe {
                asm!(
                    ".arch_extension fp", ".arch_extension simd",
                    "stp q0, q1, [{p}]", "stp q2, q3, [{p}, #32]", "stp q4, q5, [{p}, #64]",
                    "stp q6, q7, [{p}, #96]", "stp q8, q9, [{p}, #128]", "stp q10, q11, [{p}, #160]",
                    "stp q12, q13, [{p}, #192]", "stp q14, q15, [{p}, #224]", "stp q16, q17, [{p}, #256]",
                    "stp q18, q19, [{p}, #288]", "stp q20, q21, [{p}, #320]", "stp q22, q23, [{p}, #352]",
                    "stp q24, q25, [{p}, #384]", "stp q26, q27, [{p}, #416]", "stp q28, q29, [{p}, #448]",
                    "stp q30, q31, [{p}, #480]",
                    p = in(reg) v.as_mut_ptr(), options(nostack)
                )
            };
        }
    }

    /// FPCR's rounding mode, flush-to-zero and default-NaN from the seed (no trap
    /// enables), and FPSR's cumulative flags from the seed.
    pub fn set_controls(seed: u8) {
        let s = u64::from(seed);
        let fpcr = ((s & 3) << 22) | (((s >> 2) & 1) << 24) | (((s >> 3) & 1) << 25);
        let fpsr = (s & 0x1F) | (((s >> 1) & 1) << 27);
        // SAFETY: only control and flag bits, no trap enables.
        unsafe {
            asm!(
                ".arch_extension fp",
                "msr fpcr, {c}",
                "msr fpsr, {s}",
                c = in(reg) fpcr,
                s = in(reg) fpsr,
                options(nomem, nostack)
            )
        };
    }

    /// `(FPCR, FPSR)`.
    pub fn controls() -> (u64, u64) {
        let (c, s): (u64, u64);
        // SAFETY: reads two registers.
        unsafe {
            asm!(".arch_extension fp", "mrs {c}, fpcr", "mrs {s}, fpsr", c = out(reg) c, s = out(reg) s, options(nomem, nostack))
        };
        (c, s)
    }

    /// AArch64 has no x87 stack.
    pub fn x87_push(_seed: u8) {}

    pub fn x87_check(_seed: u8) -> Option<&'static str> {
        None
    }

    /// Every register in the initial state: V0-V31 zero, FPCR and FPSR zero.
    pub fn initial_state() -> Result<&'static str, Text> {
        let (fpcr, fpsr) = controls();
        if fpcr != 0 || fpsr != 0 {
            return Err(Text::Controls { fpcr, fpsr });
        }
        let mut v = [0u8; 512];
        Regs.store(&mut v);
        if let Some(i) = (0..512).find(|&i| v[i] != 0) {
            return Err(Text::Register(i / 16));
        }
        Ok("V0-V31, FPCR and FPSR")
    }

    pub enum Text {
        Controls { fpcr: u64, fpsr: u64 },
        Register(usize),
    }

    impl core::fmt::Display for Text {
        fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
            match self {
                Text::Controls { fpcr, fpsr } => {
                    write!(f, "control/status not initial: FPCR {fpcr:#x} FPSR {fpsr:#x}")
                }
                Text::Register(i) => write!(f, "V{i} is not zero"),
            }
        }
    }
}
