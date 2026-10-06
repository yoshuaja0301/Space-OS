//! Random bytes for user space (`SYS_RANDOM`): from the virtio entropy device when
//! there is one, from RDRAND when the CPU has it, otherwise refused. There is no
//! fallback beyond these two: a caller that needs randomness for keys (TLS) must
//! learn that there is none, not receive something predictable.
//!
//! Entropy the bootloader got from the firmware (`BootInfo::entropy`, ADR-0032) is
//! mixed into every byte handed out, and never counted: its quality is not assumed
//! (PRD v0.2 §7.2), so without a device or RDRAND the answer is still a refusal.
//! It is kept only as a hash, and stretched with SHA-256 over a counter, so the
//! stream it adds is independent of what the hardware gives: good hardware output
//! stays good, and predictable hardware output is no longer predictable to anyone
//! who does not know the firmware's bytes.

use spaceabi::error::Error;
use spaceabi::sha256::Sha256;

use crate::arch::cpu;
use crate::dev::virtio_rng;
use crate::sync::SpinLock;

/// The boot entropy's hash and how many blocks have been drawn from it.
static BOOT_SEED: SpinLock<Option<([u8; 32], u64)>> = SpinLock::new(None);

/// Keep the bootloader's entropy (as a hash) to mix into everything [`fill`] hands
/// out. Nothing is kept for an empty slice. Allocation free: called before the heap.
pub fn add_boot_entropy(bytes: &[u8]) {
    if bytes.is_empty() {
        return;
    }
    let mut h = Sha256::new();
    h.update(b"spaceos boot entropy v1");
    h.update(bytes);
    *BOOT_SEED.lock() = Some((h.finish(), 0));
}

/// True when firmware entropy is being mixed in.
pub fn boot_entropy_mixed() -> bool {
    BOOT_SEED.lock().is_some()
}

/// `entropy=boot-only` on the command line, a test switch: the device and RDRAND
/// are ignored, so the firmware's bytes are all there is -- and, their quality not
/// being assumed, that is no source at all.
fn boot_only() -> bool {
    crate::cmdline::get("entropy") == Some("boot-only")
}

pub fn describe() -> &'static str {
    if boot_only() {
        "none"
    } else if virtio_rng::present() {
        "virtio-rng"
    } else if cpu::rdrand64().is_some() {
        "RDRAND"
    } else {
        "none"
    }
}

/// XOR the boot entropy's stream into `out`.
fn mix_boot_entropy(out: &mut [u8]) {
    let mut seed = BOOT_SEED.lock();
    let Some((key, counter)) = seed.as_mut() else { return };
    for chunk in out.chunks_mut(32) {
        let mut h = Sha256::new();
        h.update(key);
        h.update(&counter.to_le_bytes());
        *counter += 1;
        for (o, k) in chunk.iter_mut().zip(h.finish()) {
            *o ^= k;
        }
    }
}

/// Fill all of `out`, or fail with `NotFound` when no source is available.
pub fn fill(out: &mut [u8]) -> Result<(), Error> {
    if boot_only() {
        return Err(Error::NotFound);
    }
    let mut at = 0;
    let mut empty_reads = 0;
    while at < out.len() && virtio_rng::present() {
        match virtio_rng::read(&mut out[at..]) {
            Ok(0) => {
                empty_reads += 1;
                if empty_reads > 16 {
                    break;
                }
            }
            Ok(n) => at += n,
            Err(_) => break,
        }
    }
    while at < out.len() {
        let v = cpu::rdrand64().ok_or(Error::NotFound)?.to_le_bytes();
        let n = (out.len() - at).min(8);
        out[at..at + n].copy_from_slice(&v[..n]);
        at += n;
    }
    mix_boot_entropy(out);
    Ok(())
}
