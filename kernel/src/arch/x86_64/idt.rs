//! Interrupt descriptor table. All 256 vectors go through assembly stubs in `isr.s`
//! that build a [`super::trap::TrapFrame`] and call `x86_64_trap_handler`.

use core::arch::global_asm;

use crate::sync::StaticCell;

global_asm!(include_str!("isr.s"));

unsafe extern "C" {
    /// 256 stub addresses, defined in isr.s.
    static ISR_TABLE: [u64; 256];
}

#[repr(C, packed)]
#[derive(Clone, Copy)]
struct Entry {
    offset_low: u16,
    selector: u16,
    ist: u8,
    type_attr: u8,
    offset_mid: u16,
    offset_high: u32,
    zero: u32,
}

impl Entry {
    const fn missing() -> Self {
        Entry { offset_low: 0, selector: 0, ist: 0, type_attr: 0, offset_mid: 0, offset_high: 0, zero: 0 }
    }

    fn new(handler: u64, selector: u16, ist: u8, dpl: u8) -> Self {
        Entry {
            offset_low: handler as u16,
            selector,
            ist,
            // present | DPL | 64-bit interrupt gate (0xE): interrupts stay disabled on entry
            type_attr: 0x80 | ((dpl & 3) << 5) | 0x0E,
            offset_mid: (handler >> 16) as u16,
            offset_high: (handler >> 32) as u32,
            zero: 0,
        }
    }
}

#[repr(C, align(16))]
struct Idt([Entry; 256]);

#[repr(C, packed)]
struct Pointer {
    limit: u16,
    base: u64,
}

static IDT: StaticCell<Idt> = StaticCell::new(Idt([Entry::missing(); 256]));

pub fn init() {
    let cs = super::gdt::selectors().kernel_code.0;
    // SAFETY: early boot, single CPU; ISR_TABLE is a static array from isr.s.
    unsafe {
        let idt = IDT.get_mut();
        for (v, entry) in idt.0.iter_mut().enumerate() {
            let ist = match v {
                1 => super::gdt::DEBUG_IST as u8,
                2 => super::gdt::NMI_IST as u8,
                8 => super::gdt::DOUBLE_FAULT_IST as u8,
                18 => super::gdt::MACHINE_CHECK_IST as u8,
                _ => 0,
            };
            // `int3` from ring 3 is allowed (DPL 3) so a user breakpoint reports as
            // BREAKPOINT; every other software `int n` from ring 3 raises #GP.
            let dpl = if v == 3 { 3 } else { 0 };
            *entry = Entry::new(ISR_TABLE[v], cs, ist, dpl);
        }
        let ptr = Pointer { limit: (core::mem::size_of::<Idt>() - 1) as u16, base: IDT.get() as u64 };
        core::arch::asm!("lidt [{}]", in(reg) &ptr, options(nostack, readonly, preserves_flags));
    }
    println!("[kernel] idt loaded (256 vectors)");
}
