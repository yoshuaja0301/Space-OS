//! Kernel-mode self tests run at boot (before user space exists) plus the
//! command-line driven fault injection used by the test harness.

use alloc::boxed::Box;
use alloc::vec::Vec;

use x86_64::structures::paging::PageTableFlags;

use crate::mm::{self, AddressSpace, frame, heap, paging};
use crate::{arch, cmdline};

const SCRATCH_VA: u64 = mm::KSTACK_BASE + 512 * 1024 * 1024;

pub fn run_early() {
    // Heap: allocations are returned in full.
    let (_, used0) = heap::stats();
    {
        let v: Vec<u64> = (0..1000).collect();
        assert_eq!(v.iter().sum::<u64>(), 499_500);
        let b = Box::new([0xA5u8; 4096]);
        assert_eq!(b[4095], 0xA5);
    }
    let (_, used1) = heap::stats();
    assert_eq!(used0, used1, "kernel heap leaked {} bytes", used1 as isize - used0 as isize);

    // Frames: distinct, zeroed, and returned in full.
    let (_, free0) = frame::stats();
    let frames: Vec<_> = (0..64).map(|_| frame::alloc_zeroed().expect("frame")).collect();
    for (i, f) in frames.iter().enumerate() {
        assert!(!frames[..i].contains(f), "duplicate frame handed out");
    }
    for f in frames {
        frame::free(f);
    }
    let (_, free1) = frame::stats();
    assert_eq!(free0, free1, "frame allocator leaked");

    // Kernel paging: map, write, read back through the linear map, unmap.
    let f = frame::alloc_zeroed().expect("frame");
    paging::map_kernel_page(
        SCRATCH_VA,
        f,
        PageTableFlags::PRESENT | PageTableFlags::WRITABLE | PageTableFlags::NO_EXECUTE,
    )
    .expect("map scratch");
    // SAFETY: just mapped.
    unsafe { core::ptr::write_volatile(SCRATCH_VA as *mut u64, 0xDEAD_BEEF_CAFE_F00D) };
    let via_linear = mm::phys_to_virt(f.start_address().as_u64()).as_ptr::<u64>();
    // SAFETY: linear map of the same frame.
    assert_eq!(unsafe { core::ptr::read_volatile(via_linear) }, 0xDEAD_BEEF_CAFE_F00D);
    assert_eq!(paging::translate_current(SCRATCH_VA), Some(f.start_address().as_u64()));
    let f2 = paging::unmap_kernel_page(SCRATCH_VA).expect("unmap scratch");
    assert_eq!(f, f2);
    assert_eq!(paging::translate_current(SCRATCH_VA), None);
    frame::free(f2);

    // User address space: map/check/unmap/drop returns every frame.
    let (_, free2) = frame::stats();
    {
        let mut space = AddressSpace::new().expect("address space");
        space.map_region(0x1000_0000, 4, true, false).expect("map user region");
        space.write_initial(0x1000_0010, b"space").expect("write_initial");
        assert!(space.check_user_range(0x1000_0000, 4 * 4096, true));
        assert!(!space.check_user_range(0x1000_0000, 4 * 4096 + 1, false));
        assert!(!space.check_user_range(0xFFFF_8000_0000_0000, 8, false));
        assert!(space.map_region(0x1000_1000, 1, true, false).is_err(), "overlap must be rejected");
        assert_eq!(space.used_pages, 4);
        space.unmap_region(0x1000_0000, 4).expect("unmap");
        assert_eq!(space.used_pages, 0);
        space.map_region(0x2000_0000, 2, false, true).expect("map again");
    }
    let (_, free3) = frame::stats();
    assert_eq!(free2, free3, "address space teardown leaked frames");

    println!("[kernel] selftest: heap ok, frames ok, paging ok, address-space ok");
}

#[inline(never)]
extern "C" fn overflow_stack() -> ! {
    #[inline(never)]
    #[allow(unconditional_recursion)] // the overflow is the point of this test
    fn recurse(n: u64) -> u64 {
        let pad = core::hint::black_box([n; 64]);
        let r = recurse(n + 1);
        core::hint::black_box(r + pad[(n % 64) as usize])
    }
    let v = recurse(0);
    panic!("selftest=stack: recursion returned {v} without faulting");
}

/// `selftest=panic` / `selftest=kfault` / `selftest=stack` exercise the crash paths.
pub fn run_cmdline_fault_injection() {
    match cmdline::get("selftest") {
        Some("panic") => panic!("deliberate kernel panic (cmdline selftest=panic)"),
        Some("kfault") => {
            println!("[kernel] selftest=kfault: touching an unmapped kernel address on purpose");
            let bad = 0xFFFF_F000_DEAD_0000u64 as *const u64;
            // SAFETY: intentionally unsound; the resulting kernel-mode page fault is the test.
            let v = unsafe { core::ptr::read_volatile(bad) };
            println!("[kernel] unexpected: read {v:#x}");
        }
        Some("stack") => {
            println!(
                "[kernel] selftest=stack: recursing on a guarded kernel stack until the guard page triggers a double fault"
            );
            // The boot stack lives in the linear map and has no guard page, so run the
            // overflow on a real kernel-stack slot (32 KiB mapped below a guard).
            let stack = crate::mm::kstack::KernelStack::new().expect("kernel stack for selftest");
            let top = stack.top;
            core::mem::forget(stack);
            // SAFETY: switches to a freshly mapped stack and never returns.
            unsafe {
                core::arch::asm!(
                    "mov rsp, {top}",
                    "xor rbp, rbp",
                    "call {f}",
                    "ud2",
                    top = in(reg) top,
                    f = sym overflow_stack,
                    options(noreturn)
                )
            }
        }
        Some(other) => println!("[kernel] unknown selftest '{other}' ignored"),
        None => {}
    }
    let _ = arch::interrupts_enabled();
}
