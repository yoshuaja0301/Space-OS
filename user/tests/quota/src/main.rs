//! Memory quota: map pages until the kernel says `Quota`, verify the pages are
//! usable and zeroed, release them, and verify the quota is available again.
#![no_std]
#![no_main]

use libspace::spaceabi::error::Error;
use libspace::{println, sys};

const CHUNK_PAGES: usize = 16;
const CHUNK: usize = CHUNK_PAGES * 4096;

#[unsafe(no_mangle)]
pub extern "C" fn space_main() -> i32 {
    let before = sys::self_info().expect("self_info");
    println!("[quota] quota {} pages, {} used before test", before.quota_pages, before.used_pages);
    let expect_chunks = (before.quota_pages - before.used_pages) as usize / CHUNK_PAGES;

    let mut chunks: [*mut u8; 512] = [core::ptr::null_mut(); 512];
    let mut n = 0usize;
    let err = loop {
        match sys::mem_map(CHUNK) {
            Ok(p) => {
                // SAFETY: freshly mapped CHUNK bytes.
                unsafe {
                    for i in (0..CHUNK).step_by(4096) {
                        if *p.add(i) != 0 {
                            println!("[quota] page not zeroed");
                            return 3;
                        }
                        *p.add(i) = 0xAB;
                    }
                }
                chunks[n] = p;
                n += 1;
                if n == chunks.len() {
                    println!("[quota] never hit the quota");
                    return 2;
                }
            }
            Err(e) => break e,
        }
    };
    println!(
        "[quota] mapped {} chunks of {} pages, then got {:?} (expected {} chunks)",
        n, CHUNK_PAGES, err, expect_chunks
    );
    if err != Error::Quota || n != expect_chunks {
        return 1;
    }
    let mid = sys::self_info().expect("self_info");
    if mid.used_pages + CHUNK_PAGES as u64 <= mid.quota_pages {
        println!("[quota] accounting mismatch: used {} of {}", mid.used_pages, mid.quota_pages);
        return 4;
    }
    for &c in &chunks[..n] {
        sys::mem_unmap(c, CHUNK).expect("unmap");
    }
    let after = sys::self_info().expect("self_info");
    if after.used_pages != before.used_pages {
        println!("[quota] pages not released: {} vs {}", after.used_pages, before.used_pages);
        return 5;
    }
    let again = sys::mem_map(CHUNK).expect("map after release");
    sys::mem_unmap(again, CHUNK).expect("unmap again");
    println!("[quota] ok: quota enforced at {} pages and fully reusable", mid.quota_pages);
    0
}
