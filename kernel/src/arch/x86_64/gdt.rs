//! GDT + TSS. Layout is fixed by the `syscall`/`sysret` requirements (ADR-0004):
//!
//! | index | selector | segment       |
//! |-------|----------|---------------|
//! | 0     | 0x00     | null          |
//! | 1     | 0x08     | kernel code   |
//! | 2     | 0x10     | kernel data   |
//! | 3     | 0x1b     | user data     |
//! | 4     | 0x23     | user code     |
//! | 5     | 0x28     | TSS           |

use x86_64::VirtAddr;
use x86_64::instructions::segmentation::{CS, DS, ES, FS, GS, SS, Segment};
use x86_64::instructions::tables::load_tss;
use x86_64::structures::gdt::{Descriptor, GlobalDescriptorTable, SegmentSelector};
use x86_64::structures::tss::TaskStateSegment;

use crate::sync::StaticCell;

pub const DOUBLE_FAULT_IST: u16 = 1;
/// NMI can arrive while `rsp` still points at a user stack (syscall entry/exit
/// window), so it must run on its own stack.
pub const NMI_IST: u16 = 2;
/// `#DB` fires on the first kernel instruction after `syscall` when user code set
/// TF; same stack hazard as NMI.
pub const DEBUG_IST: u16 = 3;
pub const MACHINE_CHECK_IST: u16 = 4;
const IST_STACK_SIZE: usize = 16 * 1024;

#[repr(align(16))]
struct IstStack(#[allow(dead_code)] [u8; IST_STACK_SIZE]);

static DOUBLE_FAULT_STACK: StaticCell<IstStack> = StaticCell::new(IstStack([0; IST_STACK_SIZE]));
static NMI_STACK: StaticCell<IstStack> = StaticCell::new(IstStack([0; IST_STACK_SIZE]));
static DEBUG_STACK: StaticCell<IstStack> = StaticCell::new(IstStack([0; IST_STACK_SIZE]));
static MACHINE_CHECK_STACK: StaticCell<IstStack> = StaticCell::new(IstStack([0; IST_STACK_SIZE]));
static TSS: StaticCell<TaskStateSegment> = StaticCell::new(TaskStateSegment::new());
static GDT: StaticCell<GlobalDescriptorTable> = StaticCell::new(GlobalDescriptorTable::new());

#[derive(Clone, Copy)]
#[allow(dead_code)]
pub struct Selectors {
    pub kernel_code: SegmentSelector,
    pub kernel_data: SegmentSelector,
    pub user_data: SegmentSelector,
    pub user_code: SegmentSelector,
    pub tss: SegmentSelector,
}

static SELECTORS: StaticCell<Option<Selectors>> = StaticCell::new(None);

pub fn selectors() -> Selectors {
    // SAFETY: written once in `init` before any reader exists.
    unsafe { (*SELECTORS.get()).expect("gdt not initialised") }
}

pub fn init() {
    // SAFETY: single CPU, early boot, interrupts disabled.
    unsafe {
        let tss = TSS.get_mut();
        let top = |s: &StaticCell<IstStack>| VirtAddr::new(s.get() as u64 + IST_STACK_SIZE as u64);
        tss.interrupt_stack_table[(DOUBLE_FAULT_IST - 1) as usize] = top(&DOUBLE_FAULT_STACK);
        tss.interrupt_stack_table[(NMI_IST - 1) as usize] = top(&NMI_STACK);
        tss.interrupt_stack_table[(DEBUG_IST - 1) as usize] = top(&DEBUG_STACK);
        tss.interrupt_stack_table[(MACHINE_CHECK_IST - 1) as usize] = top(&MACHINE_CHECK_STACK);

        let gdt = GDT.get_mut();
        let kernel_code = gdt.append(Descriptor::kernel_code_segment());
        let kernel_data = gdt.append(Descriptor::kernel_data_segment());
        let user_data = gdt.append(Descriptor::user_data_segment());
        let user_code = gdt.append(Descriptor::user_code_segment());
        let tss_sel = gdt.append(Descriptor::tss_segment(&*TSS.get()));
        let sel = Selectors { kernel_code, kernel_data, user_data, user_code, tss: tss_sel };
        assert_eq!(kernel_code.0, 0x08);
        assert_eq!(kernel_data.0, 0x10);
        assert_eq!(user_data.0, 0x1b);
        assert_eq!(user_code.0, 0x23);
        *SELECTORS.get_mut() = Some(sel);

        (*GDT.get()).load();
        CS::set_reg(kernel_code);
        SS::set_reg(kernel_data);
        DS::set_reg(SegmentSelector(0));
        ES::set_reg(SegmentSelector(0));
        FS::set_reg(SegmentSelector(0));
        GS::set_reg(SegmentSelector(0));
        load_tss(tss_sel);
    }
    println!("[kernel] gdt/tss loaded (IST stacks: double fault, NMI, debug, machine check)");
}

/// Stack the CPU switches to on a ring 3 -> ring 0 transition (interrupt/exception).
pub fn set_kernel_stack(top: u64) {
    // SAFETY: single CPU; the TSS is only read by hardware on privilege transitions.
    unsafe { (*TSS.get()).privilege_stack_table[0] = VirtAddr::new(top) };
}
