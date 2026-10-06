//! The counter AArch64 keeps time with: the generic timer's virtual count
//! (`CNTVCT_EL0`), at the rate `CNTFRQ_EL0` states. It has at least 56 bits, so it
//! does not wrap in any machine's lifetime.

use super::cpu;
use crate::clock::Counter;

pub fn counter(_rsdp: u64) -> Result<Counter, &'static str> {
    let hz = cpu::frequency();
    if hz == 0 {
        return Err("CNTFRQ_EL0 states no rate");
    }
    Ok(Counter { name: "generic timer count", hz, mask: u64::MAX })
}

pub fn read() -> u64 {
    cpu::timestamp()
}
