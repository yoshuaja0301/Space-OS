//! Local APIC: interrupts between CPUs, and the application processors' timer.
//!
//! The boot CPU keeps the PIC and the PIT it has always had (ADR-0003); the PIC
//! reaches it through LINT0 in the virtual-wire mode the firmware leaves, and nothing
//! here changes that. What the local APIC adds is what one CPU cannot do by itself:
//! start the others, tell an idle one there is work, stop them all when the kernel
//! panics, and give every application processor a timer of its own.
//!
//! Both register interfaces are handled: memory-mapped xAPIC, and x2APIC through MSRs
//! when the firmware already switched to it (machines with many CPUs do).

use core::sync::atomic::{AtomicU8, AtomicU64, Ordering};

use x86_64::registers::model_specific::Msr;

const IA32_APIC_BASE: u32 = 0x1B;
const BASE_X2APIC: u64 = 1 << 10;
const BASE_ENABLE: u64 = 1 << 11;

// Register offsets of the xAPIC page; the x2APIC MSR is 0x800 + offset / 16.
const REG_ID: u32 = 0x20;
const REG_TPR: u32 = 0x80;
const REG_EOI: u32 = 0xB0;
const REG_SVR: u32 = 0xF0;
const REG_ESR: u32 = 0x280;
const REG_ICR_LOW: u32 = 0x300;
const REG_ICR_HIGH: u32 = 0x310;
const REG_LVT_TIMER: u32 = 0x320;
const REG_LVT_LINT0: u32 = 0x350;
const REG_LVT_LINT1: u32 = 0x360;
const REG_LVT_ERROR: u32 = 0x370;
const REG_TIMER_INIT: u32 = 0x380;
const REG_TIMER_CURRENT: u32 = 0x390;
const REG_TIMER_DIVIDE: u32 = 0x3E0;
const X2APIC_ICR_MSR: u32 = 0x830;

const LVT_MASKED: u32 = 1 << 16;
const LVT_PERIODIC: u32 = 1 << 17;
const SVR_ENABLE: u32 = 1 << 8;
const ICR_PENDING: u32 = 1 << 12;
const ICR_ASSERT: u32 = 1 << 14;
const ICR_INIT: u32 = 0b101 << 8;
const ICR_STARTUP: u32 = 0b110 << 8;
const ICR_NMI: u32 = 0b100 << 8;
const ICR_ALL_BUT_SELF: u32 = 0b11 << 18;
/// Divide the timer's input clock by 16.
const TIMER_DIVIDE_16: u32 = 0b0011;

/// "There is work": an idle CPU wakes and looks at the run queue.
pub const VEC_RESCHEDULE: u8 = 0xF1;
/// An application processor's tick.
pub const VEC_TIMER: u8 = 0xF2;
pub const VEC_SPURIOUS: u8 = 0xFF;

/// How often an application processor's timer fires.
pub const AP_TICK_HZ: u64 = 100;

const MODE_NONE: u8 = 0;
const MODE_XAPIC: u8 = 1;
const MODE_X2APIC: u8 = 2;
static MODE: AtomicU8 = AtomicU8::new(MODE_NONE);
static MMIO: AtomicU64 = AtomicU64::new(0);
/// Initial count for one application-processor tick (divide-by-16 clock).
static TICK_COUNT: AtomicU64 = AtomicU64::new(0);

fn read(reg: u32) -> u32 {
    match MODE.load(Ordering::Relaxed) {
        // SAFETY: an x2APIC register MSR, present in x2APIC mode.
        MODE_X2APIC => unsafe { Msr::new(0x800 + (reg >> 4)).read() as u32 },
        // SAFETY: a register inside the mapped xAPIC page.
        _ => unsafe { core::ptr::read_volatile((MMIO.load(Ordering::Relaxed) + reg as u64) as *const u32) },
    }
}

fn write(reg: u32, value: u32) {
    match MODE.load(Ordering::Relaxed) {
        // SAFETY: an x2APIC register MSR, present in x2APIC mode.
        MODE_X2APIC => unsafe { Msr::new(0x800 + (reg >> 4)).write(value as u64) },
        // SAFETY: a register inside the mapped xAPIC page.
        _ => unsafe {
            core::ptr::write_volatile((MMIO.load(Ordering::Relaxed) + reg as u64) as *mut u32, value)
        },
    }
}

pub fn present() -> bool {
    MODE.load(Ordering::Relaxed) != MODE_NONE
}

/// This CPU's local APIC ID.
pub fn id() -> u32 {
    match MODE.load(Ordering::Relaxed) {
        MODE_X2APIC => read(REG_ID),
        _ => read(REG_ID) >> 24,
    }
}

/// Take the boot CPU's local APIC into use. `Err` says why there is none to use, in
/// which case the machine runs on the boot CPU alone, as before.
pub fn init_bsp() -> Result<(), &'static str> {
    if core::arch::x86_64::__cpuid(1).edx & (1 << 9) == 0 {
        return Err("the CPU has no local APIC");
    }
    // SAFETY: IA32_APIC_BASE exists wherever CPUID reports an APIC.
    let base = unsafe { Msr::new(IA32_APIC_BASE).read() };
    if base & BASE_ENABLE == 0 {
        // Turning it on here would move the PIC's interrupts; leave the machine as
        // the firmware set it up.
        return Err("the firmware left the local APIC disabled");
    }
    if base & BASE_X2APIC != 0 {
        MODE.store(MODE_X2APIC, Ordering::Relaxed);
    } else {
        let phys = base & 0x000F_FFFF_FFFF_F000;
        let virt = crate::mm::mmio::map(phys, 4096).map_err(|_| "no room to map the local APIC")?;
        MMIO.store(virt, Ordering::Relaxed);
        MODE.store(MODE_XAPIC, Ordering::Relaxed);
    }
    // Software-enable with a spurious vector of our own. LINT0 keeps whatever the
    // firmware programmed: that is how the PIC reaches this CPU.
    write(REG_SVR, read(REG_SVR) | SVR_ENABLE | VEC_SPURIOUS as u32);
    write(REG_TPR, 0);
    write(REG_ESR, 0);
    write(REG_ESR, 0);
    Ok(())
}

pub fn x2apic() -> bool {
    MODE.load(Ordering::Relaxed) == MODE_X2APIC
}

/// Local APIC of an application processor that just started: same mode as the boot
/// CPU, the PIC kept away (only the boot CPU takes its interrupts), and a periodic
/// timer of [`AP_TICK_HZ`].
pub fn init_ap() {
    if MODE.load(Ordering::Relaxed) == MODE_X2APIC {
        // A CPU comes out of INIT in xAPIC mode.
        // SAFETY: switching this CPU's APIC to the mode the boot CPU uses.
        unsafe {
            let mut m = Msr::new(IA32_APIC_BASE);
            let v = m.read();
            m.write(v | BASE_ENABLE | BASE_X2APIC);
        }
    }
    write(REG_SVR, SVR_ENABLE | VEC_SPURIOUS as u32);
    write(REG_TPR, 0);
    write(REG_LVT_LINT0, LVT_MASKED);
    write(REG_LVT_LINT1, LVT_MASKED);
    write(REG_LVT_ERROR, LVT_MASKED);
    write(REG_ESR, 0);
    write(REG_ESR, 0);
    let count = TICK_COUNT.load(Ordering::Relaxed);
    if count != 0 {
        write(REG_TIMER_DIVIDE, TIMER_DIVIDE_16);
        write(REG_LVT_TIMER, VEC_TIMER as u32 | LVT_PERIODIC);
        write(REG_TIMER_INIT, count.min(u32::MAX as u64) as u32);
    }
}

/// Measure the timer against the PIT: `wait_ms` must really wait (interrupts on).
/// Returns the counts per application-processor tick, or 0 if the timer did not
/// move.
pub fn calibrate(wait_ms: &dyn Fn(u64)) -> u64 {
    const MEASURE_MS: u64 = 50;
    write(REG_TIMER_DIVIDE, TIMER_DIVIDE_16);
    write(REG_LVT_TIMER, LVT_MASKED | VEC_TIMER as u32);
    wait_ms(1); // start on a tick boundary
    write(REG_TIMER_INIT, u32::MAX);
    wait_ms(MEASURE_MS);
    let left = read(REG_TIMER_CURRENT);
    write(REG_TIMER_INIT, 0);
    let per_ms = (u32::MAX - left) as u64 / MEASURE_MS;
    let count = per_ms * 1000 / AP_TICK_HZ;
    TICK_COUNT.store(count, Ordering::Relaxed);
    count
}

pub fn eoi() {
    write(REG_EOI, 0);
}

fn send(dest: u32, low: u32) {
    crate::sync::without_interrupts(|| match MODE.load(Ordering::Relaxed) {
        // SAFETY: the x2APIC interrupt command register.
        MODE_X2APIC => unsafe { Msr::new(X2APIC_ICR_MSR).write(((dest as u64) << 32) | low as u64) },
        MODE_XAPIC => {
            while read(REG_ICR_LOW) & ICR_PENDING != 0 {
                core::hint::spin_loop();
            }
            write(REG_ICR_HIGH, dest << 24);
            write(REG_ICR_LOW, low);
        }
        _ => {}
    })
}

/// A fixed interrupt with `vector` to the CPU with local APIC ID `dest`.
pub fn send_ipi(dest: u32, vector: u8) {
    send(dest, ICR_ASSERT | vector as u32);
}

pub fn send_init(dest: u32) {
    send(dest, ICR_ASSERT | ICR_INIT);
}

/// Start-up: the CPU begins in real mode at physical `page << 12`.
pub fn send_startup(dest: u32, page: u8) {
    send(dest, ICR_ASSERT | ICR_STARTUP | page as u32);
}

/// An NMI to every other CPU (the panic path: nothing is masked against it).
pub fn nmi_all_others() {
    if present() {
        send(0, ICR_ASSERT | ICR_NMI | ICR_ALL_BUT_SELF);
    }
}
