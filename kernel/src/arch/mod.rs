//! Architecture layer: x86-64 and AArch64 (ADR-0028). Everything outside this
//! module reaches the hardware through the names both architectures export.

#[cfg(target_arch = "x86_64")]
pub mod x86_64;

#[cfg(target_arch = "x86_64")]
pub use self::x86_64::*;

#[cfg(target_arch = "aarch64")]
pub mod aarch64;

#[cfg(target_arch = "aarch64")]
pub use self::aarch64::*;
