//! Deliberately misbehaves in the way the parent asks for. The kernel must kill this
//! process (and only this process) with the matching reason.
#![no_std]
#![no_main]

use libspace::{handle, println, sys};

#[unsafe(no_mangle)]
pub extern "C" fn space_main() -> i32 {
    let mut buf = [0u8; 64];
    let (len, _) = sys::recv(handle::BOOTSTRAP, &mut buf, false).expect("mode from parent");
    let mode = core::str::from_utf8(&buf[..len]).unwrap_or("?");
    println!("[fault] pid {} performing '{}'", sys::self_info().map(|i| i.pid).unwrap_or(0), mode);
    match mode {
        "kernel_write" => {
            let p = 0xFFFF_8000_0000_0000u64 as *mut u64;
            // SAFETY: intentionally invalid: writing into the kernel half must fault.
            unsafe { core::ptr::write_volatile(p, 1) };
        }
        "null_read" => {
            let p = core::ptr::null::<u64>();
            // SAFETY: intentionally invalid.
            let v = unsafe { core::ptr::read_volatile(p) };
            println!("[fault] read {v}");
        }
        "nx_exec" => {
            // The stack is mapped NX: executing from it must fault. Words, so the
            // code is aligned on AArch64 too (a misaligned pc is a different fault).
            let code = [if cfg!(target_arch = "aarch64") { 0xD65F_03C0u32 } else { 0xC3C3_C3C3 }; 4]; // ret
            let f: extern "C" fn() = unsafe { core::mem::transmute(code.as_ptr()) };
            f();
        }
        "div_zero" => arch::div_zero(),
        "ud2" => arch::undefined_instruction(),
        "cli" => arch::privileged_instruction(),
        "int3" => arch::breakpoint(),
        "simd_fp" => arch::simd_fp_exception(),
        "x87_fp" => arch::x87_fp_exception(),
        "tf_syscall" => arch::single_step_into_syscall(),
        "noncanon_rsp_syscall" | "kernel_rsp_syscall" => {
            // The kernel must never touch the user stack pointer: with it pointing at
            // a non-canonical or kernel address, a system call must still return
            // normally. Exit 0 on success.
            let bad_sp: u64 =
                if mode == "noncanon_rsp_syscall" { 0x8000_0000_0000_0000 } else { 0xFFFF_8000_0000_0000 };
            let t = arch::syscall_with_stack(bad_sp);
            println!("[fault] syscall with a hostile rsp returned ticks={t}");
            return 0;
        }
        "kernel_rip_jump" => {
            let f: extern "C" fn() = unsafe { core::mem::transmute(0xFFFF_FFFF_8010_0000u64 as *const ()) };
            f();
        }
        "noncanon_rip_jump" => {
            let f: extern "C" fn() = unsafe { core::mem::transmute(0x8000_0000_0000u64 as *const ()) };
            f();
        }
        "ro_vmo_write" => {
            // A memory object mapped read-only must fault on a write even though the
            // handle carries the WRITE right.
            let h = sys::vmo_create(4096).expect("vmo_create");
            let p = sys::vmo_map(h, true).expect("vmo_map read-only");
            // SAFETY: intentionally writing through a read-only mapping.
            unsafe { core::ptr::write_volatile(p, 1) };
        }
        "kernel_syscall_ptr" => {
            // Not a fault: the kernel must reject the pointer with Error::Fault instead
            // of touching kernel memory. Exit 0 if it does.
            let r = unsafe { sys::raw(spaceabi::syscall::nr::LOG, 0xFFFF_FFFF_8000_0000, 16, 0, 0, 0, 0) };
            return if spaceabi::error::decode(r) == Err(spaceabi::error::Error::Fault) { 0 } else { 2 };
        }
        _ => {}
    }
    println!("[fault] survived '{}' - this is a test failure", mode);
    3
}

#[cfg(target_arch = "x86_64")]
mod arch {
    pub fn div_zero() {
        // Rust's `/` inserts a software check that panics; use the instruction
        // directly so the CPU raises #DE.
        // SAFETY: intentionally raises a divide error.
        unsafe {
            core::arch::asm!("xor edx, edx", "mov eax, 1", "xor ecx, ecx", "div ecx",
                out("eax") _, out("edx") _, out("ecx") _)
        };
    }

    pub fn undefined_instruction() {
        // SAFETY: undefined instruction; the kernel must report INVALID_OPCODE.
        unsafe { core::arch::asm!("ud2") };
    }

    pub fn privileged_instruction() {
        // SAFETY: privileged instruction in ring 3 -> #GP.
        unsafe { core::arch::asm!("cli") };
    }

    pub fn breakpoint() {
        // SAFETY: breakpoint trap in ring 3; the kernel must report BREAKPOINT.
        unsafe { core::arch::asm!("int3") };
    }

    /// Unmask SSE's divide-by-zero in MXCSR, then divide 1 by 0: #XM, which must
    /// end this process (SIMD_FP_ERROR) and nothing else (ADR-0031).
    pub fn simd_fp_exception() {
        let mxcsr: u32 = 0x1F80 & !(1 << 9);
        // SAFETY: deliberate; only this process's own MXCSR and XMM0/XMM1 change.
        unsafe {
            core::arch::asm!(
                "ldmxcsr [{m}]",
                "pxor xmm1, xmm1",
                "movd xmm0, {one:e}",
                "divss xmm0, xmm1",
                m = in(reg) &mxcsr,
                one = in(reg) 0x3F80_0000u32,
                options(nostack, readonly)
            )
        };
    }

    /// Unmask the x87 zero-divide exception, divide 1 by 0 and wait for the unit:
    /// #MF, which must end this process (X87_FP_ERROR) and nothing else.
    pub fn x87_fp_exception() {
        let fcw: u16 = 0x037F & !(1 << 2);
        // SAFETY: deliberate; only this process's own x87 state changes.
        unsafe {
            core::arch::asm!("fldcw [{c}]", "fld1", "fldz", "fdivp", "fwait", c = in(reg) &fcw, options(nostack, readonly))
        };
    }

    pub fn single_step_into_syscall() {
        // Set TF and immediately execute `syscall`: the single-step trap is then
        // delivered on the first *kernel* instruction. The kernel must survive
        // that and terminate this process when the trap re-fires in ring 3.
        // SAFETY: deliberate.
        unsafe {
            core::arch::asm!(
                "pushfq",
                "or qword ptr [rsp], 0x100",
                "popfq",
                "syscall",
                inout("rax") spaceabi::syscall::nr::TICKS as u64 => _,
                out("rcx") _, out("r11") _,
            )
        };
    }

    /// `TICKS` with the stack pointer at `bad`, restored afterwards.
    pub fn syscall_with_stack(bad: u64) -> u64 {
        let t: u64;
        // SAFETY: rsp is restored before anything touches the stack again.
        unsafe {
            core::arch::asm!(
                "mov r12, rsp",
                "mov rsp, {bad}",
                "syscall",
                "mov rsp, r12",
                bad = in(reg) bad,
                inout("rax") spaceabi::syscall::nr::TICKS as u64 => t,
                out("rcx") _, out("r11") _, out("r12") _,
                options(nostack)
            )
        };
        t
    }
}

#[cfg(target_arch = "aarch64")]
mod arch {
    use libspace::println;

    /// AArch64 integer division by zero does not trap: the quotient is 0. Say so;
    /// the parent does not ask for this on AArch64 (ADR-0028).
    pub fn div_zero() {
        let q: u64;
        // SAFETY: `udiv` by zero is defined (it gives 0).
        unsafe { core::arch::asm!("udiv {q}, {n}, xzr", q = out(reg) q, n = in(reg) 1u64) };
        println!("[fault] udiv by zero gave {q}: AArch64 does not trap it");
    }

    pub fn undefined_instruction() {
        // SAFETY: a permanently undefined instruction; INVALID_OPCODE.
        unsafe { core::arch::asm!("udf #0") };
    }

    pub fn privileged_instruction() {
        // Masking interrupts from EL0 traps while SCTLR_EL1.UMA is clear, which the
        // kernel makes sure of: GENERAL_PROTECTION.
        // SAFETY: deliberate.
        unsafe { core::arch::asm!("msr daifset, #2") };
    }

    pub fn breakpoint() {
        // SAFETY: a breakpoint instruction; BREAKPOINT.
        unsafe { core::arch::asm!("brk #0") };
    }

    /// AArch64 FP exceptions do not trap unless the core implements the FPCR trap
    /// enables, which the Cortex-A72 does not: the parent does not ask for these.
    pub fn simd_fp_exception() {
        println!("[fault] FP exceptions do not trap on this core");
    }

    pub fn x87_fp_exception() {
        println!("[fault] AArch64 has no x87");
    }

    /// Single-stepping is a debug feature only EL1 can turn on: nothing to try
    /// from EL0. The parent does not ask for this on AArch64.
    pub fn single_step_into_syscall() {
        println!("[fault] no single-step from EL0 on AArch64");
    }

    /// `TICKS` with the stack pointer at `bad`, restored afterwards.
    pub fn syscall_with_stack(bad: u64) -> u64 {
        let t: u64;
        // SAFETY: sp is restored before anything touches the stack again; `svc`
        // itself does not use it.
        unsafe {
            core::arch::asm!(
                "mov x20, sp",
                "mov sp, {bad}",
                "svc #0",
                "mov sp, x20",
                bad = in(reg) bad,
                in("x8") spaceabi::syscall::nr::TICKS as u64,
                lateout("x0") t,
                out("x20") _,
                options(nostack)
            )
        };
        t
    }
}
