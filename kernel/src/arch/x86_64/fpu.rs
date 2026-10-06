//! x87, SSE and AVX state per thread (ADR-0031).
//!
//! Every CPU switches the units on for user space -- CR0.EM and TS clear, MP and NE
//! set, CR4.OSFXSR and OSXMMEXCPT set -- and the scheduler saves and loads each
//! thread's state on every switch (`crate::fpu`). Where the CPU has XSAVE, the state
//! is x87, SSE and, when the CPU has it, AVX (`XCR0`), and its size is what CPUID
//! leaf 0xD says for exactly those parts; without XSAVE it is the 512-byte FXSAVE
//! image (x87 and SSE). AVX-512 and other XSAVE components are not switched on: a
//! program that tries them gets #UD. The boot CPU decides, every other CPU follows.
//!
//! The kernel is built soft-float and never uses these registers; the save and
//! restore routines below are the only instructions in it that touch them, which
//! the build gate checks.

use core::arch::{asm, global_asm};
use core::sync::atomic::{AtomicU8, AtomicU64, AtomicUsize, Ordering};

use x86_64::registers::control::{Cr0, Cr0Flags, Cr4, Cr4Flags};

/// XCR0 components.
const XCR0_X87: u64 = 1 << 0;
const XCR0_SSE: u64 = 1 << 1;
const XCR0_AVX: u64 = 1 << 2;

const FXSAVE: u8 = 1;
const XSAVE: u8 = 2;

static KIND: AtomicU8 = AtomicU8::new(0);
static XCR0: AtomicU64 = AtomicU64::new(0);
static SIZE: AtomicUsize = AtomicUsize::new(512);

global_asm!(
    ".section .text",
    ".global spaceos_fxsave",
    "spaceos_fxsave:",
    "    fxsave64 [rdi]",
    "    ret",
    ".global spaceos_fxrstor",
    "spaceos_fxrstor:",
    "    fxrstor64 [rdi]",
    "    ret",
    // Every component XCR0 enables (EDX:EAX all ones).
    ".global spaceos_xsave",
    "spaceos_xsave:",
    "    mov eax, -1",
    "    mov edx, -1",
    "    xsave64 [rdi]",
    "    ret",
    ".global spaceos_xrstor",
    "spaceos_xrstor:",
    "    mov eax, -1",
    "    mov edx, -1",
    "    xrstor64 [rdi]",
    "    ret",
);

unsafe extern "C" {
    fn spaceos_fxsave(area: *mut u8);
    fn spaceos_fxrstor(area: *const u8);
    fn spaceos_xsave(area: *mut u8);
    fn spaceos_xrstor(area: *const u8);
}

fn cpuid(leaf: u32, sub: u32) -> core::arch::x86_64::CpuidResult {
    core::arch::x86_64::__cpuid_count(leaf, sub)
}

fn set_xcr0(value: u64) {
    // SAFETY: CR4.OSXSAVE is set; `value` enables x87 and SSE and only components
    // CPUID leaf 0xD reports.
    unsafe {
        asm!("xsetbv", in("ecx") 0, in("eax") value as u32, in("edx") (value >> 32) as u32, options(nomem, nostack))
    };
}

/// Switch the units on for user space on the calling CPU. The boot CPU (`boot`)
/// also decides what is saved and how big a thread's area is.
pub fn init_cpu(boot: bool) {
    let xsave = cpuid(1, 0).ecx & (1 << 26) != 0;
    // SAFETY: the kernel itself never uses x87/SSE/AVX; these bits only decide that
    // user space may, and how its exceptions are reported (#MF and #XM).
    unsafe {
        Cr0::update(|f| {
            f.remove(Cr0Flags::EMULATE_COPROCESSOR | Cr0Flags::TASK_SWITCHED);
            f.insert(Cr0Flags::MONITOR_COPROCESSOR | Cr0Flags::NUMERIC_ERROR);
        });
        Cr4::update(|f| {
            f.insert(Cr4Flags::OSFXSR | Cr4Flags::OSXMMEXCPT_ENABLE);
            if xsave {
                f.insert(Cr4Flags::OSXSAVE);
            }
        });
    }
    if boot {
        if xsave {
            let leaf = cpuid(0xD, 0);
            let supported = u64::from(leaf.eax) | (u64::from(leaf.edx) << 32);
            let avx = cpuid(1, 0).ecx & (1 << 28) != 0 && supported & XCR0_AVX != 0;
            let xcr0 = XCR0_X87 | XCR0_SSE | if avx { XCR0_AVX } else { 0 };
            set_xcr0(xcr0);
            // EBX: the area size for exactly the components now enabled.
            let size = (cpuid(0xD, 0).ebx as usize).max(576);
            XCR0.store(xcr0, Ordering::Relaxed);
            SIZE.store(size, Ordering::Relaxed);
            KIND.store(XSAVE, Ordering::Release);
        } else {
            SIZE.store(512, Ordering::Relaxed);
            KIND.store(FXSAVE, Ordering::Release);
        }
        log();
    } else if KIND.load(Ordering::Acquire) == XSAVE {
        set_xcr0(XCR0.load(Ordering::Relaxed));
    }
}

/// Say what a thread's FP/SIMD state is (before the heap exists: no allocation).
fn log() {
    let size = SIZE.load(Ordering::Relaxed);
    if KIND.load(Ordering::Acquire) == XSAVE {
        let avx = XCR0.load(Ordering::Relaxed) & XCR0_AVX != 0;
        println!(
            "[kernel] fpu: x87 and SSE{} per thread through XSAVE, {size} bytes",
            if avx { " and AVX" } else { "" }
        );
    } else {
        println!("[kernel] fpu: x87 and SSE per thread through FXSAVE, {size} bytes");
    }
}

/// `(size, alignment)` of a thread's area: XSAVE wants 64-byte alignment.
pub fn area_layout() -> (usize, usize) {
    (SIZE.load(Ordering::Relaxed), 64)
}

/// The initial state: every register zero, x87 control word 0x37F (all exceptions
/// masked, 64-bit precision, round to nearest), MXCSR 0x1F80 (the same for SSE).
/// In the XSAVE layout the header's XSTATE_BV is zero, so XRSTOR puts x87 and the
/// vector registers in their initial state and takes only MXCSR from the image.
///
/// # Safety
/// `area` points to `size` writable bytes.
pub unsafe fn initial_state(area: *mut u8, size: usize) {
    // SAFETY: as the caller promises.
    unsafe {
        core::ptr::write_bytes(area, 0, size);
        core::ptr::write_unaligned(area.cast::<u16>(), 0x037F);
        core::ptr::write_unaligned(area.add(24).cast::<u32>(), 0x1F80);
    }
}

/// # Safety
/// `area` is a thread's area (see `crate::fpu::FpuArea::save`).
pub unsafe fn save(area: *mut u8) {
    // SAFETY: an area of the size and alignment this CPU's save needs.
    unsafe { if KIND.load(Ordering::Relaxed) == XSAVE { spaceos_xsave(area) } else { spaceos_fxsave(area) } }
}

/// # Safety
/// `area` holds a state saved by [`save`] or made by [`initial_state`].
pub unsafe fn restore(area: *const u8) {
    // SAFETY: as above.
    unsafe {
        if KIND.load(Ordering::Relaxed) == XSAVE { spaceos_xrstor(area) } else { spaceos_fxrstor(area) }
    }
}
