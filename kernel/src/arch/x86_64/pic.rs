//! Legacy 8259 PIC pair remapped to vectors 32..47 (ADR-0003: the MVP uses PIC+PIT;
//! LAPIC/IOAPIC and SMP come with the Developer Preview).

use x86_64::instructions::port::Port;

const PIC1_CMD: u16 = 0x20;
const PIC1_DATA: u16 = 0x21;
const PIC2_CMD: u16 = 0xA0;
const PIC2_DATA: u16 = 0xA1;

fn io_wait() {
    // SAFETY: port 0x80 is the traditional POST/delay port.
    unsafe { Port::<u8>::new(0x80).write(0) };
}

pub fn init() {
    // SAFETY: standard 8259 initialisation sequence.
    unsafe {
        let mut c1 = Port::<u8>::new(PIC1_CMD);
        let mut d1 = Port::<u8>::new(PIC1_DATA);
        let mut c2 = Port::<u8>::new(PIC2_CMD);
        let mut d2 = Port::<u8>::new(PIC2_DATA);
        c1.write(0x11);
        io_wait();
        c2.write(0x11);
        io_wait();
        d1.write(32); // master vector base
        io_wait();
        d2.write(40); // slave vector base
        io_wait();
        d1.write(4); // slave on IRQ2
        io_wait();
        d2.write(2);
        io_wait();
        d1.write(0x01); // 8086 mode
        io_wait();
        d2.write(0x01);
        io_wait();
        // Unmask timer (IRQ0), keyboard (IRQ1) and the cascade (IRQ2); mask everything else.
        d1.write(0xF8);
        d2.write(0xFF);
    }
    println!("[kernel] pic remapped to vectors 32..47");
}

pub fn eoi(irq: u8) {
    // SAFETY: end-of-interrupt command.
    unsafe {
        if irq >= 8 {
            Port::<u8>::new(PIC2_CMD).write(0x20);
        }
        Port::<u8>::new(PIC1_CMD).write(0x20);
    }
}

/// IRQ 7 / IRQ 15 may be spurious; check the in-service register.
pub fn is_spurious(irq: u8) -> bool {
    if irq != 7 && irq != 15 {
        return false;
    }
    // SAFETY: OCW3 read ISR.
    unsafe {
        let (mut cmd, bit) =
            if irq == 7 { (Port::<u8>::new(PIC1_CMD), 7) } else { (Port::<u8>::new(PIC2_CMD), 7) };
        cmd.write(0x0B);
        let isr = cmd.read();
        let spurious = isr & (1 << bit) == 0;
        if spurious && irq == 15 {
            // The master still saw IRQ2.
            Port::<u8>::new(PIC1_CMD).write(0x20);
        }
        spurious
    }
}
