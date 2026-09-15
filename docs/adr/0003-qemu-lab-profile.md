# ADR-0003: Profil laboratorium QEMU yang dipatok

Status: Diterima — 2026-09-15

## Konteks

PRD §6 mengusulkan QEMU x86-64 + UEFI, 4 vCPU, 8 GiB RAM sebagai profil laboratorium, dan meminta host, versi QEMU/firmware, mode akselerasi, serta flag CPU dipatok agar hasil dapat dibandingkan (§9).

## Keputusan

Profil dipatok di `xtask/src/main.rs::qemu_args` dan dipakai oleh `build`, `run`, `test`, `soak`:

| Parameter | Nilai |
|---|---|
| Mesin | `q35`, `accel=tcg` (tanpa KVM agar identik di CI dan laptop) |
| CPU | `qemu64`, `-smp 4` (kernel MVP hanya memakai BSP) |
| RAM | `8G` |
| Firmware | OVMF `OVMF_CODE_4M.fd` + salinan `OVMF_VARS_4M.fd` (Ubuntu 24.04, paket `ovmf` 2024.02) |
| Disk | image ESP 64 MiB (MBR + partisi FAT32) lewat AHCI q35 |
| Konsol | serial COM1 → file log; tampilan `none` (framebuffer GOP tetap ada) |
| Keluar | `isa-debug-exit` iobase `0xf4`: kernel menulis `0x10` sukses, `0x11` gagal, `0x3f` panic → status proses QEMU 33/35/127 |
| Lain | `-no-reboot` (triple fault = keluar, bukan boot ulang) |

QEMU yang diuji: 8.2.2 (Ubuntu 24.04). VirtIO block/network/display belum diaktifkan (tahap 3).

## Konsekuensi

- Waktu boot ~5 detik per cold boot di TCG (OVMF ≈ 3–4 detik); soak 100 boot ≈ 9 menit.
- Angka apa pun dari profil ini adalah angka emulasi, bukan performa perangkat fisik (PRD §9).
