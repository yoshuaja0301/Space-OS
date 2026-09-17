# ADR-0001: Microkernel berorientasi capability, Rust stable, target bare-metal

Status: Diterima — 2026-09-15

## Konteks

PRD §2 mengusulkan microkernel berorientasi capability dengan Rust sebagai bahasa utama dan Assembly terbatas untuk boot, interrupt, dan perpindahan konteks. PRD §1 mendefinisikan "native": kernel, proses, syscall, IPC, dan layanan dasar berjalan di Space OS sendiri; pustaka berlisensi sesuai boleh dipakai.

## Keputusan

1. **Kernel**: microkernel. Yang ada di kernel hanya address space, thread, IPC channel, handle/capability, timer, interrupt, dan kuota memori (`kernel/`). Tokenizer, model, indeks, shell, driver kompleks berada di user-space.
2. **Bahasa**: Rust **stable** (dipatok `1.94.1` di `rust-toolchain.toml`), bukan nightly. Konsekuensinya: tidak memakai `abi_x86_interrupt`; stub interrupt ditulis dalam assembly (`kernel/src/arch/x86_64/isr.s`), context switch dan syscall entry memakai `global_asm!`, entry point memakai naked function (stabil sejak Rust 1.88).
3. **Target**: kernel dan program user memakai target bawaan `x86_64-unknown-none` (soft-float, tanpa SSE, red zone dimatikan, `code-model=kernel`) dengan `relocation-model=static` dan frame pointer aktif untuk backtrace panic. Bootloader memakai `x86_64-unknown-uefi`.
4. **Pustaka yang diizinkan** (semua MIT/Apache-2.0 atau MPL-2.0, tanpa dependensi pada Linux): `x86_64` (struktur GDT/paging/MSR), `uefi` (bootloader), `linked_list_allocator` (heap), `noto-sans-mono-bitmap` (font konsol). Tidak ada yang berjalan di host untuk menggantikan fungsi guest.
5. **Modularitas untuk ARM64**: semua kode spesifik arsitektur berada di `kernel/src/arch/<arch>/`; modul lain hanya memakai antarmuka `crate::arch::*`.

## Konsekuensi

- Tidak ada ketergantungan pada nightly → build reproduktif dan CI sederhana.
- Semua kode `unsafe` harus memberi alasan (`#![deny(unsafe_op_in_unsafe_fn)]`); clippy `-D warnings` pada semua target.
- Kernel pra-SMP: lihat ADR-0006.
