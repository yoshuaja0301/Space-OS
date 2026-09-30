//! VirtIO entropy device (virtio-rng): the host fills the buffers it is given with
//! random bytes. One request at a time, polled, like the other virtio drivers.

use spaceabi::error::Error;

use crate::dev::pci;
use crate::dev::virtio::{DESC_F_WRITE, DmaPage, SplitQueue, Transport, VIRTIO_VENDOR};
use crate::sync::SpinLock;

/// Transitional and modern PCI device IDs of the entropy device.
const RNG_DEVICE_IDS: [u16; 2] = [0x1005, 0x1044];
/// Spins before a request is given up: the host answers from its own entropy pool
/// in microseconds, so this only bounds a device that never answers.
const POLL_LIMIT: u32 = 10_000_000;
/// Bytes asked for per request: one page.
const REQUEST_MAX: usize = 4096;

struct VirtioRng {
    transport: Transport,
    queue: SplitQueue,
    page: DmaPage,
}

static RNG: SpinLock<Option<VirtioRng>> = SpinLock::new(None);

pub fn init() {
    match probe() {
        Ok(Some(msg)) => println!("[kernel] virtio-rng: {msg}"),
        Ok(None) => println!("[kernel] virtio-rng: no device present"),
        Err(e) => println!("[kernel] virtio-rng: initialisation failed: {e}"),
    }
}

fn probe() -> Result<Option<alloc::string::String>, Error> {
    let Some(addr) = pci::find(VIRTIO_VENDOR, &RNG_DEVICE_IDS) else {
        return Ok(None);
    };
    addr.enable_memory_and_bus_master();
    addr.disable_intx();
    let Some(t) = Transport::probe(addr, 0)? else {
        println!("[kernel] virtio-rng: device has no modern virtio capabilities; ignored");
        return Ok(None);
    };
    if !t.reset() {
        println!("[kernel] virtio-rng: device never finished resetting; ignored");
        return Ok(None);
    }
    if t.negotiate(0).is_none() {
        println!("[kernel] virtio-rng: device lacks VIRTIO_F_VERSION_1; ignored");
        t.set_status(0);
        return Ok(None);
    }
    let Some(queue) = t.setup_queue(0, 4, 1)? else {
        println!("[kernel] virtio-rng: request queue unusable; ignored");
        t.set_status(0);
        return Ok(None);
    };
    let page = DmaPage::new()?;
    t.driver_ok();
    let msg = alloc::format!(
        "pci {:02x}:{:02x}.{} queue size {}, status {:#x}",
        addr.bus,
        addr.device,
        addr.function,
        queue.size,
        t.status()
    );
    *RNG.lock() = Some(VirtioRng { transport: t, queue, page });
    Ok(Some(msg))
}

pub fn present() -> bool {
    RNG.lock().is_some()
}

/// Fill the start of `out` with bytes from the device; returns how many it gave.
pub fn read(out: &mut [u8]) -> Result<usize, Error> {
    let mut guard = RNG.lock();
    let Some(d) = guard.as_mut() else { return Err(Error::NotFound) };
    let want = out.len().min(REQUEST_MAX);
    if want == 0 {
        return Ok(0);
    }
    d.queue.set_desc(0, d.page.phys, want as u32, DESC_F_WRITE, 0);
    d.queue.push_avail(0);
    d.queue.notify();
    let mut spins = 0u32;
    while !d.queue.has_used() {
        spins += 1;
        if spins > POLL_LIMIT {
            // The buffer is still the device's: it may yet be written. The device
            // is reset and never used again rather than risk that.
            d.transport.reset();
            *guard = None;
            println!("[kernel] virtio-rng: no answer within the polling limit; device disabled");
            return Err(Error::NotFound);
        }
        core::hint::spin_loop();
    }
    let used = d.queue.pop_used();
    match used {
        Some((0, len)) if (len as usize) <= want => {
            let len = len as usize;
            // SAFETY: the page is ours again (the device returned the descriptor)
            // and `len` is within the bytes we offered.
            let src = unsafe { core::slice::from_raw_parts(d.page.virt as *const u8, len) };
            out[..len].copy_from_slice(src);
            Ok(len)
        }
        _ => Err(Error::Invalid),
    }
}
