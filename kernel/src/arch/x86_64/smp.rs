//! Starting the application processors (ADR-0024).
//!
//! The firmware leaves every CPU but the boot CPU parked. Each is started with the
//! INIT / start-up / start-up sequence and begins in 16-bit real mode at a page below
//! 1 MiB, where a trampoline takes it through protected mode into long mode on
//! page tables of its own (the trampoline page mapped where it is, plus the kernel's
//! upper half), and jumps to [`ap_entry`] on a guarded kernel stack. From there the
//! CPU loads the shared GDT with its own TSS, the shared IDT, the kernel's page
//! tables, switches the FPU off as the boot CPU did, programs `syscall`, its local
//! APIC and a timer, and becomes one more CPU the scheduler hands threads to.
//!
//! One CPU is started at a time: the trampoline page and its data are reused.
//! A CPU that does not answer within the time allowed is reported and left alone.

use core::arch::global_asm;
use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

use alloc::string::String;
use x86_64::structures::paging::PageTableFlags;

use super::percpu::{self, MAX_CPUS};
use super::{acpi, apic, gdt};
use crate::mm::kstack::KernelStack;
use crate::mm::{frame, paging, phys_to_virt};

// The trampoline is data: it is copied below 1 MiB and runs there, never where the
// kernel image holds it -- so it lives in `.rodata`, outside the kernel's
// executable segment (whose every instruction the build decodes as 64-bit code).
global_asm!(
    ".section .rodata.ap_trampoline, \"a\"",
    ".global ap_tr_start",
    "ap_tr_start:",
    ".code16",
    "cli",
    "cld",
    // The start-up IPI set CS to the trampoline page; data is addressed from there.
    "mov %cs, %ax",
    "mov %ax, %ds",
    "movzwl %ax, %ebx",
    "shl $4, %ebx",
    "lgdtl (ap_tr_gdt_ptr - ap_tr_start)",
    "mov %cr0, %eax",
    "or $1, %eax",
    "mov %eax, %cr0",
    "ljmpl *(ap_tr_far32 - ap_tr_start)",
    ".code32",
    ".global ap_tr_prot32",
    "ap_tr_prot32:",
    "mov $0x10, %ax",
    "mov %ax, %ds",
    "mov %ax, %es",
    "mov %ax, %ss",
    // PAE and global pages, the trampoline's page tables, then long mode with NX.
    "mov %cr4, %eax",
    "or $0xA0, %eax",
    "mov %eax, %cr4",
    "mov (ap_tr_cr3 - ap_tr_start)(%ebx), %eax",
    "mov %eax, %cr3",
    "mov $0xC0000080, %ecx",
    "rdmsr",
    "or $0x900, %eax",
    "wrmsr",
    // Paging and write protection on: long mode is active.
    "mov %cr0, %eax",
    "or $0x80010000, %eax",
    "mov %eax, %cr0",
    "ljmp *(ap_tr_far64 - ap_tr_start)(%ebx)",
    ".code64",
    ".global ap_tr_long64",
    "ap_tr_long64:",
    // Nothing is known about the upper halves after the mode switch.
    "mov %ebx, %ebx",
    "mov $0x10, %ax",
    "mov %ax, %ds",
    "mov %ax, %es",
    "mov %ax, %ss",
    "mov (ap_tr_stack - ap_tr_start)(%rbx), %rsp",
    "mov (ap_tr_cpu - ap_tr_start)(%rbx), %rdi",
    "mov (ap_tr_entry - ap_tr_start)(%rbx), %rax",
    "xor %ebp, %ebp",
    "jmp *%rax",
    ".balign 16",
    ".global ap_tr_gdt",
    "ap_tr_gdt:",
    ".quad 0",
    ".quad 0x00cf9a000000ffff",
    ".quad 0x00cf92000000ffff",
    ".quad 0x00af9a000000ffff",
    ".global ap_tr_gdt_ptr",
    "ap_tr_gdt_ptr:",
    ".word 31",
    ".long 0",
    ".global ap_tr_far32",
    "ap_tr_far32:",
    ".long 0",
    ".word 0x08",
    ".global ap_tr_far64",
    "ap_tr_far64:",
    ".long 0",
    ".word 0x18",
    ".balign 8",
    ".global ap_tr_cr3",
    "ap_tr_cr3: .quad 0",
    ".global ap_tr_stack",
    "ap_tr_stack: .quad 0",
    ".global ap_tr_cpu",
    "ap_tr_cpu: .quad 0",
    ".global ap_tr_entry",
    "ap_tr_entry: .quad 0",
    ".global ap_tr_end",
    "ap_tr_end:",
    options(att_syntax)
);

unsafe extern "C" {
    static ap_tr_start: u8;
    static ap_tr_prot32: u8;
    static ap_tr_long64: u8;
    static ap_tr_gdt: u8;
    static ap_tr_gdt_ptr: u8;
    static ap_tr_far32: u8;
    static ap_tr_far64: u8;
    static ap_tr_cr3: u8;
    static ap_tr_stack: u8;
    static ap_tr_cpu: u8;
    static ap_tr_entry: u8;
    static ap_tr_end: u8;
}

/// Offset of a trampoline symbol from the trampoline's start.
macro_rules! offset {
    ($sym:ident) => {
        (&raw const $sym) as usize - (&raw const ap_tr_start) as usize
    };
}

/// The CPU index that last reached `ap_entry` (0 = none yet).
static STARTED: AtomicUsize = AtomicUsize::new(0);
/// IST stack tops and the idle stack top of each application processor, written by
/// the boot CPU before it sends the start-up IPI.
static AP_IST: [[AtomicU64; gdt::IST_COUNT]; MAX_CPUS] =
    [const { [const { AtomicU64::new(0) }; gdt::IST_COUNT] }; MAX_CPUS];
static AP_STACK: [AtomicU64; MAX_CPUS] = [const { AtomicU64::new(0) }; MAX_CPUS];
static SMP_ON: AtomicBool = AtomicBool::new(false);

/// Tell CPU `cpu` (an index) to look at the run queue.
pub fn kick(cpu: usize) {
    if SMP_ON.load(Ordering::Relaxed) {
        apic::send_ipi(percpu::apic_id(cpu), apic::VEC_RESCHEDULE);
    }
}

/// Stop every other CPU (panic path).
pub fn halt_others() {
    if SMP_ON.load(Ordering::Relaxed) {
        apic::nmi_all_others();
    }
}

/// Wait about `ms` milliseconds on the PIT with interrupts on, then turn them off.
fn wait_ms(ms: u64) {
    let until = crate::sched::now_ticks() + ms;
    while crate::sched::now_ticks() < until {
        x86_64::instructions::interrupts::enable_and_hlt();
        x86_64::instructions::interrupts::disable();
    }
}

/// Wait until CPU `cpu` says it started, for at most `ms` milliseconds.
fn wait_started(cpu: usize, ms: u64) -> bool {
    let until = crate::sched::now_ticks() + ms;
    loop {
        if STARTED.load(Ordering::Acquire) == cpu {
            return true;
        }
        if crate::sched::now_ticks() >= until {
            return false;
        }
        x86_64::instructions::interrupts::enable_and_hlt();
        x86_64::instructions::interrupts::disable();
    }
}

/// The trampoline in its page below 1 MiB, with page tables that map that page where
/// it is and the kernel's upper half where it always is.
struct Trampoline {
    page: u64,
}

fn table(pa: u64) -> &'static mut [u64; 512] {
    // SAFETY: a frame kept for the trampoline, reachable through the linear map and
    // used by nothing else.
    unsafe { &mut *(phys_to_virt(pa).as_mut_ptr::<[u64; 512]>()) }
}

fn prepare_trampoline() -> Result<Trampoline, &'static str> {
    let take = || frame::take_low().map(|f| f.start_address().as_u64()).ok_or("no free page below 1 MiB");
    let (page, pml4, pdpt, pd, pt) = (take()?, take()?, take()?, take()?, take()?);
    let len = offset!(ap_tr_end);
    if len > 4096 {
        return Err("the trampoline does not fit a page");
    }
    let dst = phys_to_virt(page).as_mut_ptr::<u8>();
    let patches32 = [
        (offset!(ap_tr_gdt_ptr) + 2, page as usize + offset!(ap_tr_gdt)),
        (offset!(ap_tr_far32), page as usize + offset!(ap_tr_prot32)),
        (offset!(ap_tr_far64), page as usize + offset!(ap_tr_long64)),
    ];
    let patches64 =
        [(offset!(ap_tr_cr3), pml4), (offset!(ap_tr_entry), ap_entry as *const () as usize as u64)];
    // SAFETY: `page` is a whole free frame below 1 MiB, mapped by the linear map; the
    // source is the trampoline's bytes in the kernel image; every patch lies inside
    // the copied bytes.
    unsafe {
        core::ptr::write_bytes(dst, 0, 4096);
        core::ptr::copy_nonoverlapping(&raw const ap_tr_start, dst, len);
        for (off, v) in patches32 {
            core::ptr::write_unaligned(dst.add(off) as *mut u32, v as u32);
        }
        for (off, v) in patches64 {
            core::ptr::write_unaligned(dst.add(off) as *mut u64, v);
        }
    }
    for t in [pml4, pdpt, pd, pt] {
        table(t).fill(0);
    }
    let rw = (PageTableFlags::PRESENT | PageTableFlags::WRITABLE).bits();
    let kernel = table(paging::kernel_pml4().start_address().as_u64());
    table(pml4)[256..].copy_from_slice(&kernel[256..]);
    table(pml4)[0] = pdpt | rw;
    table(pdpt)[0] = pd | rw;
    table(pd)[0] = pt | rw;
    table(pt)[((page >> 12) & 511) as usize] = page | rw;
    Ok(Trampoline { page })
}

/// Guarded stacks for a new CPU: its idle thread's, and one per IST entry. They live
/// as long as the kernel.
fn stacks_for(cpu: usize) -> Result<(), &'static str> {
    let stack = || -> Result<u64, &'static str> {
        let s = KernelStack::new().map_err(|_| "no memory for its stacks")?;
        let top = s.top;
        core::mem::forget(s);
        Ok(top)
    };
    AP_STACK[cpu].store(stack()?, Ordering::Relaxed);
    for slot in &AP_IST[cpu] {
        slot.store(stack()?, Ordering::Relaxed);
    }
    Ok(())
}

fn start_one(t: &Trampoline, cpu: usize, apic_id: u32) -> Result<(), &'static str> {
    stacks_for(cpu)?;
    let dst = phys_to_virt(t.page).as_mut_ptr::<u8>();
    // SAFETY: the trampoline page is ours; no other CPU is running it (they start
    // one at a time, and the previous one left it before saying it started).
    let (stack_at, cpu_at) = (offset!(ap_tr_stack), offset!(ap_tr_cpu));
    unsafe {
        core::ptr::write_unaligned(dst.add(stack_at) as *mut u64, AP_STACK[cpu].load(Ordering::Relaxed));
        core::ptr::write_unaligned(dst.add(cpu_at) as *mut u64, cpu as u64);
    }
    percpu::set_apic_id(cpu, apic_id);
    core::sync::atomic::fence(Ordering::SeqCst);
    apic::send_init(apic_id);
    wait_ms(10);
    let vector = (t.page >> 12) as u8;
    apic::send_startup(apic_id, vector);
    if wait_started(cpu, 2) {
        return Ok(());
    }
    apic::send_startup(apic_id, vector);
    if wait_started(cpu, 500) {
        return Ok(());
    }
    // Back to waiting for a start-up IPI: a CPU that woke late must not run the
    // trampoline once it holds another CPU's number.
    apic::send_init(apic_id);
    Err("it did not answer the start-up IPI")
}

/// Bring up every processor the firmware lists. Runs on the boot CPU after the
/// scheduler, with interrupts off (it turns them on only to wait).
pub fn init(rsdp: u64) {
    if let Err(why) = apic::init_bsp() {
        println!("[kernel] smp: {why}; running on the boot CPU only");
        return;
    }
    let bsp = apic::id();
    percpu::set_apic_id(0, bsp);
    let cpus = match acpi::processors(rsdp) {
        Ok(p) => p,
        Err(why) => {
            println!("[kernel] smp: {why}; running on the boot CPU only");
            return;
        }
    };
    let others: alloc::vec::Vec<u32> = cpus.apic_ids.iter().copied().filter(|&id| id != bsp).collect();
    if others.is_empty() {
        println!("[kernel] smp: one processor (local APIC {bsp})");
        return;
    }
    let count = apic::calibrate(&wait_ms);
    if count == 0 {
        println!("[kernel] smp: the local APIC timer does not count; running on the boot CPU only");
        return;
    }
    let tramp = match prepare_trampoline() {
        Ok(t) => t,
        Err(why) => {
            println!("[kernel] smp: {why}; running on the boot CPU only");
            return;
        }
    };
    SMP_ON.store(true, Ordering::Relaxed);
    let mut ids = String::new();
    let mut next = 1usize;
    let mut failed = 0usize;
    let mut left_out = 0usize;
    for &id in &others {
        if next >= MAX_CPUS {
            left_out += 1;
            continue;
        }
        match start_one(&tramp, next, id) {
            Ok(()) => {
                if !ids.is_empty() {
                    ids.push(' ');
                }
                ids.push_str(&alloc::format!("{id}"));
                next += 1;
            }
            Err(why) => {
                println!("[kernel] smp: the processor with local APIC {id} stays parked: {why}");
                failed += 1;
            }
        }
    }
    println!(
        "[kernel] smp: {} CPUs online (boot CPU local APIC {bsp}; started {}), {} APIC mode, AP timer {} Hz ({count} counts){}{}",
        next,
        if ids.is_empty() { String::from("none") } else { ids },
        if apic::x2apic() { "x2APIC" } else { "xAPIC" },
        apic::AP_TICK_HZ,
        if failed > 0 { alloc::format!("; {failed} did not start") } else { String::new() },
        if left_out > 0 {
            alloc::format!("; {left_out} left parked (more than {MAX_CPUS} CPUs)")
        } else {
            String::new()
        },
    );
}

/// Where an application processor arrives from the trampoline: on its idle stack,
/// on the trampoline's page tables, with interrupts off.
extern "C" fn ap_entry(cpu: u64) -> ! {
    let cpu = cpu as usize;
    let ist = core::array::from_fn(|i| AP_IST[cpu][i].load(Ordering::Relaxed));
    // SAFETY: this is CPU `cpu`, and this is its first act (it takes no lock before).
    unsafe { gdt::init_ap(cpu, ist) };
    super::idt::load();
    paging::activate_kernel();
    super::fpu::init_cpu(false);
    super::syscall::init_cpu(cpu);
    apic::init_ap();
    crate::sched::init_ap(AP_STACK[cpu].load(Ordering::Relaxed));
    STARTED.store(cpu, Ordering::Release);
    crate::sched::idle_loop()
}
