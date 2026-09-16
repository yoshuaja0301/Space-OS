# Keterbatasan dan batas yang diketahui (milestone tahap 1–2)

Daftar ini adalah bagian wajib setiap milestone (PRD §8). "Belum ada" berarti tidak ada kode, bukan "hampir".

## Kernel

- **Satu CPU** (ADR-0006): AP tidak dibangunkan; `SYSCALL_KERNEL_RSP` global; spinlock = interrupt off.
- **Satu thread per proses**; tidak ada `thread_create`.
- **Tidak ada shared memory / memory object antarproses**; IPC hanya pesan ≤ 256 byte + 1 handle. Transfer data besar (buffer model) memerlukan memory object pada ABI v1 (tahap 4).
- **Tidak ada timeout** pada `recv`/`wait`; `send` tidak pernah memblokir (antrean 64 → `WouldBlock`).
- **Stack user tetap 64 KiB**, dipetakan penuh saat spawn; tidak ada demand paging atau pertumbuhan stack.
- **Heap kernel tetap 16 MiB**; kehabisan heap = panic (alloc error), bukan penolakan bertahap.
- **Kuota menghitung halaman user saja**; frame page-table dan objek kernel (thread, channel) belum dibebankan ke proses. Headroom heap kernel dan `try_reserve` mengubah kehabisan heap menjadi error syscall (`NoMemory`), tetapi satu proses masih dapat menghabiskan headroom bersama (ancaman PRD §5 "resource exhaustion" baru ditutup sebagian).
- Siklus referensi antar-channel (endpoint A dikirim lewat channel B dan endpoint B dikirim lewat channel A) tidak dideteksi dan bocor; siklus satu channel ditolak (`Invalid`).
- NMI bersarang (NMI kedua saat handler NMI belum selesai) merusak frame di stack IST NMI.
- Headroom heap kernel 1 MiB menolak alokasi yang dipicu user, tetapi fragmentasi ekstrem masih dapat membuat alokasi internal kernel gagal (panic).
- Hanya **PIC + PIT**; belum ada ACPI/LAPIC/IOAPIC/HPET; RSDP hanya diteruskan.
- Framebuffer dipetakan write-back lewat linear map (cukup untuk QEMU; perangkat fisik memerlukan write-combining/PAT).
- Reklamasi memori `BOOTLOADER_RECLAIMABLE` dilakukan segera; UEFI runtime services tidak dipakai (region-nya dibiarkan RESERVED).
- Keyboard hanya dikuras (IRQ1), tidak diteruskan ke user-space.

## Penyimpanan

- Driver blok dan FAT32 berada **di dalam kernel** (ADR-0007), bukan user-space; tanpa IOMMU, driver DMA tetap komponen tepercaya.
- FAT32 **read-only**, satu volume, tanpa cache blok, tanpa mount table; entri long-name dilewati sehingga berkas guest harus bernama 8.3.
- VirtIO memakai polling, satu permintaan pada satu waktu, tanpa interrupt; perangkat yang macet menghasilkan error setelah batas polling, bukan hang, tetapi batas itu membekukan CPU selama beberapa saat.
- Hanya perangkat virtio-blk 1.0 modern; perangkat legacy/transitional tanpa kapabilitas modern diabaikan.

## Komputasi

- Satu koneksi per layanan compute (bootstrap channel-nya); belum ada broker koneksi atau multiplexing klien.
- Frame buffer dibebankan ke kuota **layanan**, bukan klien, jadi klien yang nakal dapat menghabiskan kuota layanannya.
- Submit bersifat sinkron dari sudut pandang klien (satu round trip IPC per operasi); belum ada batching, antrean asinkron, atau event completion.
- Matematika f32 diimplementasikan sendiri (tanpa libm); akurasinya memadai untuk model uji, bukan pustaka numerik umum.
- Backend hanya CPU skalar, tanpa SIMD, tanpa GPU/NPU.

## Belum ada (tahap berikutnya)

- VirtIO net/input/display, jaringan (sisa tahap 3).
- Tokenizer dan inferensi model (tahap 5, A01).
- SpaceLink, Tool Broker, Agent Runtime, Space Guard sebagai layanan, Space Shell, package service, tanda tangan paket (5A).
- GPU, ARM64 (6–7).

## Verifikasi

- Semua bukti berasal dari **QEMU TCG** dengan profil ADR-0003; belum pernah dicoba pada PC fisik.
- Soak 100 boot (K01) dijalankan pada satu host; stress 8 jam (PRD §9) belum dijalankan.
- Tidak ada fuzzing syscall; uji negatif bersifat contoh, bukan eksplorasi acak.
