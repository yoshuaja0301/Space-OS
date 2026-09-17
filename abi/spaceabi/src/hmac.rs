//! HMAC-SHA256 (RFC 2104), shared by the host packaging tool and the guest.
//!
//! Space OS packages are authenticated with a symmetric MAC rather than a public-key
//! signature. That is a deliberate, documented limitation (ADR-0014): a from-scratch
//! Ed25519 would be a worse thing to trust than an honest HMAC, and HMAC over a
//! SHA-256 the guest already verifies against the FIPS vectors is small enough to
//! read in one sitting.
//!
//! The implementation is checked in the guest against the RFC 4231 test vectors, so
//! "the host and the guest agree" is not the only evidence that it is correct.

use crate::sha256::Sha256;

const BLOCK: usize = 64;

/// HMAC-SHA256 of `data` under `key`.
pub fn hmac_sha256(key: &[u8], data: &[u8]) -> [u8; 32] {
    let mut block = [0u8; BLOCK];
    if key.len() > BLOCK {
        // A key longer than the block size is replaced by its digest (RFC 2104).
        let mut h = Sha256::new();
        h.update(key);
        block[..32].copy_from_slice(&h.finish());
    } else {
        block[..key.len()].copy_from_slice(key);
    }

    let mut ipad = [0x36u8; BLOCK];
    let mut opad = [0x5cu8; BLOCK];
    for i in 0..BLOCK {
        ipad[i] ^= block[i];
        opad[i] ^= block[i];
    }

    let mut inner = Sha256::new();
    inner.update(&ipad);
    inner.update(data);
    let inner = inner.finish();

    let mut outer = Sha256::new();
    outer.update(&opad);
    outer.update(&inner);
    outer.finish()
}

/// Compare two MACs without an early exit.
///
/// A byte-by-byte `==` leaks where the first difference is through timing. Nothing
/// here is remotely timing-hardened otherwise, but a MAC comparison is the one place
/// where the cost of getting it right is a single line.
pub fn verify(expected: &[u8; 32], got: &[u8; 32]) -> bool {
    let mut diff = 0u8;
    for i in 0..32 {
        diff |= expected[i] ^ got[i];
    }
    diff == 0
}
