# ADR-0005: Format executable (ELF64 statis) dan initrd (ustar)

Status: Diterima — 2026-09-15

## Keputusan

- **Executable**: ELF64 x86-64 `ET_EXEC` statis, non-PIE, tanpa relokasi dinamis, tanpa interpreter. Hanya `PT_LOAD` yang dimuat; hak halaman mengikuti `p_flags` (W → writable, tanpa X → NX). Program user di-link di `0x40_0000` (`user/libspace/user.ld`), stack 64 KiB di bawah `0x7FFF_F000_0000`, region `mem_map` mulai `0x10_0000_0000`. Parser bersama: `spaceabi::elf` (dipakai bootloader untuk kernel dan kernel untuk program user), semua akses dibatasi ukuran file.
- **Initrd**: arsip ustar (`tar` POSIX) berisi `bin/<program>`; dibaca lewat `spaceabi::tar` tanpa alokasi. Dibuat oleh `xtask` dari binari yang sudah di-strip (`llvm-objcopy --strip-all`); binari dengan debug info tetap di `target/` untuk `addr2line`.
- **Konfigurasi boot**: `\EFI\SPACEOS\spaceos.cfg` berisi `cmdline=<key=value ...>`; kernel membaca `selftest=` untuk fault injection.

## Konsekuensi

- Tidak ada dynamic linking / libc pada tahap ini; port CLI pihak ketiga (PRD §4) memerlukan keputusan terpisah.
- Package manager, tanda tangan, dan VFS (tahap 3, P01) tidak diblokir oleh format ini: initrd hanya jalur bootstrap.
