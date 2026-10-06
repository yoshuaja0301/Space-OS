//! GDT + one TSS per CPU. Layout is fixed by the `syscall`/`sysret` requirements
//! (ADR-0004), with the TSS descriptors after the four code/data segments:
//!
//! | index | selector            | segment       |
//! |-------|---------------------|---------------|
//! | 0     | 0x00                | null          |
//! | 1     | 0x08                | kernel code   |
//! | 2     | 0x10                | kernel data   |
//! | 3     | 0x1b                | user data     |
//! | 4     | 0x23                | user code     |
//! | 5+2i  | 0x28 + 16i          | TSS of CPU i  |
//!
//! Every CPU loads the same table and its own TSS, which is also how a CPU tells
//! which one it is ([`super::percpu::index`]).

use x86_64::VirtAddr;
use x86_64::instructions::segmentation::{CS, DS, ES, FS, GS, SS, Segment};
use x86_64::instructions::tables::load_tss;
use x86_64::structures::gdt::{Descriptor, GlobalDescriptorTable, SegmentSelector};
use x86_64::structures::tss::TaskStateSegment;

use super::percpu::{self, MAX_CPUS, TSS_SELECTOR_BASE};
use crate::sync::StaticCell;

pub const DOUBLE_FAULT_IST: u16 = 1;
/// NMI can arrive while `rsp` still points at a user stack (syscall entry/exit
/// window), so it must run on its own stack.
pub const NMI_IST: u16 = 2;
/// `#DB` fires on the first kernel instruction after `syscall` when user code set
/// TF; same stack hazard as NMI.
pub const DEBUG_IST: u16 = 3;
pub const MACHINE_CHECK_IST: u16 = 4;
/// The IST stacks in the order of their indices.
pub const IST_COUNT: usize = 4;
const IST_STACK_SIZE: usize = 16 * 1024;

#[repr(align(16))]
struct IstStack(#[allow(dead_code)] [u8; IST_STACK_SIZE]);

// The boot CPU's IST stacks. Application processors get guarded kernel stacks.
static DOUBLE_FAULT_STACK: StaticCell<IstStack> = StaticCell::new(IstStack([0; IST_STACK_SIZE]));
static NMI_STACK: StaticCell<IstStack> = StaticCell::new(IstStack([0; IST_STACK_SIZE]));
static DEBUG_STACK: StaticCell<IstStack> = StaticCell::new(IstStack([0; IST_STACK_SIZE]));
static MACHINE_CHECK_STACK: StaticCell<IstStack> = StaticCell::new(IstStack([0; IST_STACK_SIZE]));

static TSS: [StaticCell<TaskStateSegment>; MAX_CPUS] =
    [const { StaticCell::new(TaskStateSegment::new()) }; MAX_CPUS];

const GDT_ENTRIES: usize = 5 + 2 * MAX_CPUS;
static GDT: StaticCell<GlobalDescriptorTable<GDT_ENTRIES>> = StaticCell::new(GlobalDescriptorTable::empty());

#[derive(Clone, Copy)]
#[allow(dead_code)]
pub struct Selectors {
    pub kernel_code: SegmentSelector,
    pub kernel_data: SegmentSelector,
    pub user_data: SegmentSelector,
    pub user_code: SegmentSelector,
}

static SELECTORS: StaticCell<Option<Selectors>> = StaticCell::new(None);

pub fn selectors() -> Selectors {
    // SAFETY: written once in `init` before any reader exists.
    unsafe { (*SELECTORS.get()).expect("gdt not initialised") }
}

fn tss_selector(cpu: usize) -> SegmentSelector {
    SegmentSelector(TSS_SELECTOR_BASE + 16 * cpu as u16)
}

/// Build the table (boot CPU, once) and load it with CPU 0's TSS.
pub fn init() {
    let top = |s: &StaticCell<IstStack>| s.get() as u64 + IST_STACK_SIZE as u64;
    let ist = [top(&DOUBLE_FAULT_STACK), top(&NMI_STACK), top(&DEBUG_STACK), top(&MACHINE_CHECK_STACK)];
    // SAFETY: early boot, only the boot CPU runs, interrupts disabled.
    unsafe {
        set_ist(0, ist);
        let gdt = GDT.get_mut();
        let kernel_code = gdt.append(Descriptor::kernel_code_segment());
        let kernel_data = gdt.append(Descriptor::kernel_data_segment());
        let user_data = gdt.append(Descriptor::user_data_segment());
        let user_code = gdt.append(Descriptor::user_code_segment());
        for (cpu, tss) in TSS.iter().enumerate() {
            let sel = gdt.append(Descriptor::tss_segment(&*tss.get()));
            assert_eq!(sel.0, tss_selector(cpu).0);
        }
        assert_eq!(kernel_code.0, 0x08);
        assert_eq!(kernel_data.0, 0x10);
        assert_eq!(user_data.0, 0x1b);
        assert_eq!(user_code.0, 0x23);
        *SELECTORS.get_mut() = Some(Selectors { kernel_code, kernel_data, user_data, user_code });
        load(0);
    }
    println!("[kernel] gdt/tss loaded (IST stacks: double fault, NMI, debug, machine check)");
}

/// # Safety
/// Before `cpu` loads its TSS, by that CPU or the one starting it.
unsafe fn set_ist(cpu: usize, tops: [u64; IST_COUNT]) {
    // SAFETY: see above; no CPU uses this TSS yet.
    let tss = unsafe { &mut *TSS[cpu].get() };
    // Indexed, not iterated: the TSS is packed, and a reference into it could be
    // misaligned.
    for (i, top) in tops.into_iter().enumerate() {
        tss.interrupt_stack_table[i] = VirtAddr::new(top);
    }
}

/// Load the table, the segments and `cpu`'s TSS on the calling CPU.
///
/// # Safety
/// `cpu` must be the calling CPU's index, and its TSS must not be loaded anywhere.
unsafe fn load(cpu: usize) {
    let sel = selectors();
    // SAFETY: the table is complete and 'static; see above for the TSS.
    unsafe {
        (*GDT.get()).load();
        CS::set_reg(sel.kernel_code);
        SS::set_reg(sel.kernel_data);
        DS::set_reg(SegmentSelector(0));
        ES::set_reg(SegmentSelector(0));
        FS::set_reg(SegmentSelector(0));
        GS::set_reg(SegmentSelector(0));
        load_tss(tss_selector(cpu));
    }
}

/// An application processor's first act: the shared table, its own TSS with its own
/// IST stacks. Until this has run, [`percpu::index`] would call it CPU 0.
///
/// # Safety
/// Called once, by CPU `cpu` itself.
pub unsafe fn init_ap(cpu: usize, ist_tops: [u64; IST_COUNT]) {
    // SAFETY: see above.
    unsafe {
        set_ist(cpu, ist_tops);
        load(cpu);
    }
}

/// Stack the CPU switches to on a ring 3 -> ring 0 transition (interrupt/exception).
pub fn set_kernel_stack(top: u64) {
    // SAFETY: each CPU writes only its own TSS, with interrupts disabled; the CPU
    // reads it only on a privilege transition.
    unsafe { (*TSS[percpu::index()].get()).privilege_stack_table[0] = VirtAddr::new(top) };
}
