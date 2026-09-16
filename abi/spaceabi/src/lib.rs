//! Space OS ABI, version 0.
//!
//! This crate is the single source of truth for every contract that crosses a
//! privilege or component boundary:
//!
//! * [`boot`]    – the structure the UEFI bootloader (`spaceboot`) hands to the kernel.
//! * [`compute`] – the Space Compute service contract (client <-> compute service).
//! * [`math`]    – deterministic f32 math shared by the guest and the baseline tool.
//! * [`model`]   – the SpaceLM v0 model container.
//! * [`syscall`] – syscall numbers and argument structures (kernel <-> user space).
//! * [`error`]   – error codes returned by syscalls.
//! * [`handle`]  – capability handle type and rights bits.
//! * [`elf`]     – a minimal ELF64 program-header parser shared by the bootloader
//!   (kernel image) and the kernel (user programs).
//!
//! It is `no_std`, allocation free, and must stay `#[repr(C)]`-stable: changing a
//! layout here means bumping [`ABI_VERSION`] (see docs/adr/0004-syscall-abi-v0.md).

#![no_std]
#![deny(unsafe_op_in_unsafe_fn)]

pub mod agent;
pub mod boot;
pub mod compute;
pub mod elf;
pub mod error;
pub mod handle;
pub mod hmac;
pub mod link;
pub mod math;
pub mod model;
pub mod pkg;
pub mod sha256;
pub mod shell;
pub mod syscall;
pub mod tar;

/// Version of the kernel <-> user ABI described by this crate.
pub const ABI_VERSION: u32 = 0;

/// Page size used by every Space OS interface (x86-64 4 KiB pages).
pub const PAGE_SIZE: usize = 4096;
