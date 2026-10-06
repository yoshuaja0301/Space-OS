//! Other processors: every CPU the MADT lists as enabled is started with PSCI
//! `CPU_ON` and schedules threads from the one run queue, as on x86-64 (ADR-0024,
//! ADR-0028).
//!
//! A CPU that PSCI starts begins at the physical address it is given, at EL1, with
//! its MMU and caches off. [`secondary_entry`] -- a few instructions that only read
//! through `x0`, so they run anywhere -- loads the registers the boot CPU uses
//! (MAIR, TCR, SCTLR, the kernel root in TTBR1) plus, in TTBR0, a small table that
//! maps its own page at its physical address: the instruction after "MMU on" is
//! fetched from the same address as before. It then installs the exception vectors
//! and jumps to the kernel's virtual address, where [`secondary_main`] drops the
//! identity map and joins the scheduler.
//! Everything the starting CPU reads with its caches off -- the boot block, the
//! identity tables, the entry code -- is cleaned to the point of coherency first.

use core::arch::{asm, global_asm};
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use alloc::vec::Vec;

use super::{cpu, exceptions, gic, mmu, percpu, power, timer};
use crate::mm::kstack::KernelStack;
use crate::mm::{MapFlags, frame, paging, phys_to_virt, pt};
use crate::{acpi, sched};

/// What a starting CPU needs before its MMU is on, at the offsets
/// [`secondary_entry`] reads.
#[repr(C)]
struct BootBlock {
    identity_root: u64, // 0
    kernel_root: u64,   // 8
    tcr: u64,           // 16
    mair: u64,          // 24
    sctlr: u64,         // 32
    stack_top: u64,     // 40
    entry: u64,         // 48
    cpu: u64,           // 56
    block_virt: u64,    // 64
    vbar: u64,          // 72
    /// Set by the CPU once it schedules (80).
    started: AtomicU64,
}

global_asm!(
    ".section .text.secondary, \"ax\"",
    ".balign 64",
    ".global secondary_entry",
    // x0 = physical address of the BootBlock; MMU and caches off. Everything is
    // read from the block before the MMU comes on: the identity map covers this
    // code only, not the block.
    "secondary_entry:",
    "    msr daifset, #0xf",
    "    ldr x2, [x0, #40]",
    "    ldr x3, [x0, #48]",
    "    ldr x4, [x0, #56]",
    "    ldr x5, [x0, #64]",
    "    ldr x6, [x0, #72]",
    "    ldr x1, [x0, #24]",
    "    msr mair_el1, x1",
    "    ldr x1, [x0, #16]",
    "    msr tcr_el1, x1",
    "    ldr x1, [x0, #0]",
    "    msr ttbr0_el1, x1",
    "    ldr x1, [x0, #8]",
    "    msr ttbr1_el1, x1",
    "    ldr x1, [x0, #32]",
    "    isb",
    "    tlbi vmalle1",
    "    dsb nsh",
    "    isb",
    "    msr sctlr_el1, x1",
    "    isb",
    "    msr vbar_el1, x6",
    "    msr spsel, #1",
    "    mov sp, x2",
    "    msr tpidr_el1, x4",
    "    mov x0, x5",
    "    mov x29, xzr",
    "    mov x30, xzr",
    "    isb",
    "    br x3",
    "secondary_entry_end:",
);

unsafe extern "C" {
    static secondary_entry: u8;
    static secondary_entry_end: u8;
}

/// MPIDR affinity of each started CPU, by index (for SGIs).
static AFFINITY: [AtomicU64; percpu::MAX_CPUS] = [const { AtomicU64::new(u64::MAX) }; percpu::MAX_CPUS];
static SMP_ON: AtomicBool = AtomicBool::new(false);

/// This CPU's affinity, as MPIDR_EL1 holds it (Aff3 in 39:32, Aff2-0 in 23:0).
fn mpidr() -> u64 {
    let v: u64;
    // SAFETY: reading an ID register.
    unsafe { asm!("mrs {}, mpidr_el1", out(reg) v, options(nomem, nostack)) };
    v & 0xFF_00FF_FFFF
}

fn sysreg_state() -> (u64, u64, u64) {
    let (tcr, mair, sctlr): (u64, u64, u64);
    // SAFETY: reading system registers.
    unsafe {
        asm!(
            "mrs {t}, tcr_el1",
            "mrs {m}, mair_el1",
            "mrs {s}, sctlr_el1",
            t = out(reg) tcr,
            m = out(reg) mair,
            s = out(reg) sctlr,
            options(nomem, nostack)
        )
    };
    (tcr, mair, sctlr)
}

/// Clean `len` bytes at virtual `va` to the point of coherency.
fn clean_to_poc(va: u64, len: u64) {
    let mut a = va & !63;
    while a < va + len {
        // SAFETY: cache maintenance on mapped kernel memory changes no data.
        unsafe { asm!("dc cvac, {}", in(reg) a, options(nostack, preserves_flags)) };
        a += 64;
    }
    // SAFETY: a barrier.
    unsafe { asm!("dsb sy", options(nostack, preserves_flags)) };
}

/// The enabled CPUs of the MADT's GIC CPU interface structures, as MPIDR values.
fn listed(rsdp: u64) -> Vec<u64> {
    let Ok(madt) = acpi::find(rsdp, b"APIC") else { return Vec::new() };
    acpi::madt_entries(madt)
        .filter(|(kind, e)| *kind == 0x0B && e.len() >= 76 && acpi::u32_at(e, 12) & 1 != 0)
        .map(|(_, e)| acpi::u64_at(e, 68) & 0xFF_00FF_FFFF)
        .collect()
}

pub fn init(rsdp: u64) {
    let me = mpidr();
    AFFINITY[0].store(me, Ordering::Relaxed);
    let cpus = listed(rsdp);
    if cpus.len() <= 1 {
        println!("[kernel] smp: one processor (MPIDR {me:#x})");
        return;
    }
    if !power::psci_present() {
        println!(
            "[kernel] smp: no PSCI to start the other {} CPU(s); running on the boot CPU only",
            cpus.len() - 1
        );
        return;
    }
    let (entry_va, end_va) =
        (core::ptr::addr_of!(secondary_entry) as u64, core::ptr::addr_of!(secondary_entry_end) as u64);
    let (Some(entry_pa), Some(end_pa)) =
        (paging::translate_current(entry_va), paging::translate_current(end_va - 4))
    else {
        println!("[kernel] smp: the entry code has no physical address; running on the boot CPU only");
        return;
    };
    // The identity map of the entry code: its page (two if it straddles one).
    let Some(identity) = frame::alloc_zeroed() else {
        println!("[kernel] smp: no memory for the start-up tables; running on the boot CPU only");
        return;
    };
    let identity = identity.start_address().as_u64();
    let mut page = entry_pa & !0xFFF;
    while page <= end_pa {
        if pt::map(identity, page, page, MapFlags::EXECUTABLE).is_err() {
            println!("[kernel] smp: cannot map the entry code; running on the boot CPU only");
            return;
        }
        page += 0x1000;
    }
    clean_to_poc(entry_va, end_va - entry_va);
    clean_page_tables(identity);
    let (tcr, mair, sctlr) = sysreg_state();
    let kernel_root = mmu::root_for(u64::MAX);
    let mut started = Vec::new();
    let mut parked = Vec::new();
    for (i, &target) in cpus.iter().filter(|&&m| m != me).enumerate().take(percpu::MAX_CPUS - 1) {
        let cpu = i + 1;
        match start(cpu, target, entry_pa, identity, kernel_root, tcr, mair, sctlr) {
            Ok(()) => started.push(target),
            Err(why) => {
                println!("[kernel] smp: the processor with MPIDR {target:#x} stays parked: {why}");
                parked.push(target);
            }
        }
    }
    let more = cpus.len().saturating_sub(1 + started.len() + parked.len());
    if started.is_empty() {
        println!(
            "[kernel] smp: none of the other {} CPU(s) started; running on the boot CPU only",
            cpus.len() - 1
        );
        return;
    }
    SMP_ON.store(true, Ordering::Release);
    let list: Vec<alloc::string::String> = started.iter().map(|m| alloc::format!("{m:#x}")).collect();
    println!(
        "[kernel] smp: {} CPUs online (boot CPU MPIDR {me:#x}; started {}), GICv3, AP timer {} Hz{}",
        1 + started.len(),
        list.join(" "),
        timer::AP_TICK_HZ,
        if more > 0 {
            alloc::format!("; {more} more listed stay parked")
        } else {
            alloc::string::String::new()
        }
    );
}

/// Clean every table of the hierarchy at `root` (a fresh, small one) to the point of
/// coherency: the starting CPU's first walk happens as its caches come on.
fn clean_page_tables(root: u64) {
    fn walk(t: u64, level: usize) {
        clean_to_poc(phys_to_virt(t).as_u64(), 4096);
        if level == 0 {
            return;
        }
        // SAFETY: a table of the start-up hierarchy, which nothing else uses.
        let tab = unsafe { pt::table(t) };
        for &e in tab.iter() {
            if mmu::present(e) && !mmu::is_block(e, level) {
                walk(mmu::addr(e), level - 1);
            }
        }
    }
    walk(root, 3);
}

#[allow(clippy::too_many_arguments)]
fn start(
    cpu: usize,
    target: u64,
    entry_pa: u64,
    identity: u64,
    kernel_root: u64,
    tcr: u64,
    mair: u64,
    sctlr: u64,
) -> Result<(), &'static str> {
    let stack = KernelStack::new().map_err(|_| "no kernel stack")?;
    let stack_top = stack.top;
    let block_pa = frame::alloc_zeroed().ok_or("no memory for its boot block")?.start_address().as_u64();
    let block_va = phys_to_virt(block_pa).as_u64();
    // SAFETY: a fresh frame, reached through the linear map; only this function and
    // the starting CPU touch it.
    let block = unsafe { &mut *(block_va as *mut BootBlock) };
    *block = BootBlock {
        identity_root: identity,
        kernel_root,
        tcr,
        mair,
        sctlr,
        stack_top,
        entry: secondary_main as *const () as usize as u64,
        cpu: cpu as u64,
        block_virt: block_va,
        vbar: exceptions::vectors(),
        started: AtomicU64::new(0),
    };
    clean_to_poc(block_va, core::mem::size_of::<BootBlock>() as u64);
    AFFINITY[cpu].store(target, Ordering::Relaxed);
    match power::cpu_on(target, entry_pa, block_pa) {
        0 => {}
        -4 => return Err("PSCI says it is already on"),
        -2 | -9 => return Err("PSCI refused the request"),
        _ => return Err("PSCI could not start it"),
    }
    if !crate::dev::wait::until(1_000, || block.started.load(Ordering::Acquire) != 0) {
        return Err("it did not answer within a second");
    }
    // The CPU runs on this stack from now on; the slot stays allocated for good.
    core::mem::forget(stack);
    Ok(())
}

/// A started CPU, on its kernel stack at its virtual address, its identity map
/// still in TTBR0.
extern "C" fn secondary_main(block: &'static BootBlock) -> ! {
    mmu::load_user_root(block.kernel_root);
    cpu::init();
    if let Err(why) = gic::init_cpu() {
        println!("[kernel] smp: cpu {}: {why}; it stops", block.cpu);
        super::halt_forever();
    }
    timer::start_cpu();
    sched::init_ap(block.stack_top);
    block.started.store(1, Ordering::Release);
    sched::idle_loop()
}

/// Tell CPU `cpu` (an index) to look at the run queue.
pub fn kick(cpu: usize) {
    if SMP_ON.load(Ordering::Relaxed) {
        let target = AFFINITY.get(cpu).map_or(u64::MAX, |a| a.load(Ordering::Relaxed));
        if target != u64::MAX {
            gic::send_sgi(gic::SGI_RESCHEDULE, target);
        }
    }
}

/// Stop every other CPU (panic path). An interrupt, so a CPU spinning with its
/// interrupts masked does not see it (GICv3 has no NMI without pseudo-NMI).
pub fn halt_others() {
    if SMP_ON.load(Ordering::Relaxed) {
        gic::send_sgi_others(gic::SGI_HALT);
    }
}
