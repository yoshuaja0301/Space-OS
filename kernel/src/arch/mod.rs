//! Architecture layer. Only x86-64 exists today; the module boundary is what a future
//! ARM64 port (PRD §6) replaces.

#[cfg(target_arch = "x86_64")]
pub mod x86_64;

#[cfg(target_arch = "x86_64")]
pub use self::x86_64::*;
