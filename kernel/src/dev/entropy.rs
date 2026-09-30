//! Random bytes for user space (`SYS_RANDOM`): from the virtio entropy device when
//! there is one, from RDRAND when the CPU has it, otherwise refused. There is no
//! fallback beyond these two: a caller that needs randomness for keys (TLS) must
//! learn that there is none, not receive something predictable.

use spaceabi::error::Error;

use crate::arch::cpu;
use crate::dev::virtio_rng;

pub fn describe() -> &'static str {
    if virtio_rng::present() {
        "virtio-rng"
    } else if cpu::rdrand64().is_some() {
        "RDRAND"
    } else {
        "none"
    }
}

/// Fill all of `out`, or fail with `NotFound` when no source is available.
pub fn fill(out: &mut [u8]) -> Result<(), Error> {
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
    Ok(())
}
