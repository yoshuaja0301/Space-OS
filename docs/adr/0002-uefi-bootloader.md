# ADR-0002: Bootloader UEFI sendiri (`spaceboot`) dan protokol boot

Status: Diterima — 2026-09-15

## Konteks

PRD §2 "Rantai boot": UEFI memuat bootloader, menyerahkan memory map dan framebuffer, lalu kernel menginisialisasi memori, interrupt, scheduler, dan init. Referensi: spesifikasi UEFI (https://uefi.org/specifications); versi firmware yang diuji: OVMF 2024.02 (paket Ubuntu 24.04 `ovmf`).

## Keputusan

- Bootloader ditulis sendiri di `boot/spaceboot/` (Rust, crate `uefi` 0.35) sebagai aplikasi UEFI `\EFI\BOOT\BOOTX64.EFI`, bukan memakai GRUB/Limine/`bootloader` crate, agar kontrak boot sepenuhnya dimiliki proyek.
- Kontrak `BootInfo` didefinisikan di `spaceabi::boot` (repr(C), bermagic dan berversi). Bootloader menyerahkan: memory map yang sudah dinormalisasi (`MemRegion` dengan jenis USABLE/RESERVED/BOOTLOADER_RECLAIMABLE/KERNEL/ACPI/MMIO/FRAMEBUFFER), framebuffer GOP, RSDP ACPI, initrd, command line (`spaceos.cfg`), PML4 boot, dan stack boot.
- Bootloader membangun page table: kernel di `0xFFFF_FFFF_8000_0000` (top 2 GiB), linear map seluruh RAM (+ framebuffer, minimal 4 GiB) di `PHYS_OFFSET = 0xFFFF_8000_0000_0000` dengan halaman 2 MiB, dan identity map sementara yang dibuang kernel. NXE dan CR0.WP diaktifkan sebelum lompat.
- Semua yang harus bertahan (image kernel, initrd, cmdline, page table, stack, boot info, memory map) dialokasikan dengan tipe memori UEFI kustom `0x8000_0000` sehingga kernel dapat mereklamasi memori bootloader/UEFI boot services tanpa risiko.

Pemeriksaan lingkungan: bootloader menolak firmware yang berjalan dengan 5-level paging (CR4.LA57) dengan pesan yang jelas (tabelnya 4-level), dan tidak menyentuh framebuffer pada mode GOP BltOnly (konsol serial saja). Lompatan akhir ke kernel memakai register eksplisit (`rcx` stack, `rdi` BootInfo, `rax` entry) sehingga urutan instruksinya tidak dapat saling menimpa operand.

## Konsekuensi

- Kernel tidak bergantung pada Multiboot atau format lain; port ARM64 hanya perlu bootloader `aarch64-unknown-uefi` yang mengisi `BootInfo` yang sama.
- Perubahan layout `BootInfo` wajib menaikkan `BOOT_INFO_VERSION`.
