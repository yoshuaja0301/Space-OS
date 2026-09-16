# Keterbatasan dan batas yang diketahui (milestone tahap 1–5)

Daftar ini adalah bagian wajib setiap milestone (PRD §8). "Belum ada" berarti tidak ada kode, bukan "hampir".

## Kernel

- **Satu CPU** (ADR-0006): AP tidak dibangunkan; `SYSCALL_KERNEL_RSP` global; spinlock = interrupt off.
- **Satu thread per proses**; tidak ada `thread_create`.
- **Pesan IPC ≤ 256 byte + 1 handle**; data besar memakai memory object (`VMO_CREATE`/`VMO_MAP`, tahap 4). Belum ada `VMO_UNMAP` khusus: pemetaan dilepas lewat `mem_unmap` dengan alamat dan panjang yang sama.
- **Memory object tidak dapat diperbesar, dipotong, atau dipetakan sebagian**; satu objek dipetakan utuh pada alamat yang dipilih kernel.
- **Tidak ada timeout** pada `recv`/`wait`; `send` tidak pernah memblokir (antrean 64 → `WouldBlock`).
- **Stack user tetap 64 KiB**, dipetakan penuh saat spawn; tidak ada demand paging atau pertumbuhan stack.
- **Heap kernel tetap 16 MiB**; kehabisan heap = panic (alloc error), bukan penolakan bertahap.
- **Kuota menghitung halaman user saja**; frame page-table dan objek kernel (thread, channel) belum dibebankan ke proses. Headroom heap kernel dan `try_reserve` mengubah kehabisan heap menjadi error syscall (`NoMemory`), tetapi satu proses masih dapat menghabiskan headroom bersama (ancaman PRD §5 "resource exhaustion" baru ditutup sebagian).
- Siklus referensi antar-channel (endpoint A dikirim lewat channel B dan endpoint B dikirim lewat channel A) tidak dideteksi dan bocor; siklus satu channel ditolak (`Invalid`).
- NMI bersarang (NMI kedua saat handler NMI belum selesai) merusak frame di stack IST NMI.
- Headroom heap kernel 1 MiB menolak alokasi yang dipicu user, tetapi fragmentasi ekstrem masih dapat membuat alokasi internal kernel gagal (panic).
- Hanya **PIC + PIT**; belum ada ACPI/LAPIC/IOAPIC/HPET; RSDP hanya diteruskan.
- Linear map hanya memuat RAM dan framebuffer; MMIO perangkat dipetakan uncached on demand (ADR-0010). Framebuffer sendiri masih write-back lewat linear map (cukup untuk QEMU; perangkat fisik memerlukan write-combining/PAT).
- Granularitas linear map 2 MiB: satu halaman besar yang sebagian RAM dan sebagian MMIO tetap dipetakan write-back seluruhnya. Pada q35/i440fx batas PCI hole sejajar 2 MiB sehingga tidak terjadi.
- Reklamasi memori `BOOTLOADER_RECLAIMABLE` dilakukan segera; UEFI runtime services tidak dipakai (region-nya dibiarkan RESERVED).
- Keyboard hanya dikuras (IRQ1), tidak diteruskan ke user-space.

## Penyimpanan

- Driver blok dan FAT32 berada **di dalam kernel** (ADR-0007), bukan user-space; tanpa IOMMU, driver DMA tetap komponen tepercaya.
- FAT32 **read-only**, satu volume, tanpa cache blok, tanpa mount table; entri long-name dilewati sehingga berkas guest harus bernama 8.3.
- VirtIO memakai polling, satu permintaan pada satu waktu, tanpa interrupt; perangkat yang macet menghasilkan error setelah batas polling, bukan hang, tetapi batas itu membekukan CPU selama beberapa saat.
- Hanya perangkat virtio-blk yang menawarkan kapabilitas modern (VIRTIO_F_VERSION_1). Perangkat transisional diterima karena juga menawarkannya; perangkat legacy murni diabaikan dengan pesan, bukan crash.
- Ukuran antrean yang dipakai adalah hasil negosiasi, maksimum 16, dan satu permintaan dipotong agar muat (`size - 2` halaman data, maksimum 8 = 32 KiB). Antrean < 3 deskriptor membuat perangkat ditolak.
- Permintaan yang melewati batas polling membuat perangkat di-reset dan dinonaktifkan permanen; tidak ada percobaan ulang atau pemulihan.

## Komputasi

- Satu koneksi per layanan compute (bootstrap channel-nya); belum ada broker koneksi atau multiplexing klien.
- Frame buffer dibebankan ke kuota **layanan**, bukan klien, jadi klien yang nakal dapat menghabiskan kuota layanannya.
- Submit bersifat sinkron dari sudut pandang klien (satu round trip IPC per operasi); belum ada batching, antrean asinkron, atau event completion.
- Matematika f32 diimplementasikan sendiri (tanpa libm); akurasinya memadai untuk model uji, bukan pustaka numerik umum.
- Backend hanya CPU skalar, tanpa SIMD, tanpa GPU/NPU.

## Inferensi

- Model referensi **tidak dilatih** (bobot dari PRNG ber-seed) dan hanya 115 ribu parameter; keluarannya tidak bermakna sebagai teks. A01 membuktikan pipeline-nya benar dan deterministik, bukan kualitas model.
- Belum ada model terlatih berlisensi, tokenizer sub-word, quantization, batching, atau sampling; dekode greedy dengan KV cache f32 penuh.
- Satu langkah dekode memakai 54 round trip IPC; cukup untuk model uji, bukan untuk throughput.
- Angka kecepatan berasal dari QEMU TCG, bukan perangkat fisik.

## Sesi dan antarmuka

- `spaceshell` mengawasi **satu** worker; belum ada tabel job atau penjadwalan beberapa job paralel.
- Loop sesi memakai polling 2 ms karena belum ada `select`, `recv` bertimeout, atau notifikasi exit lewat channel. Itu kompromi yang disengaja (ADR-0011), bukan desain akhir.
- **Belum ada masukan keyboard ke user space**: IRQ1 dikuras kernel dan tidak diteruskan. Perintah sesi datang lewat channel kontrol, jadi "terminal" belum bisa diketik manusia.
- Belum ada window manager, font selain 8x16 bawaan, atau grafik selain teks di framebuffer.
- `SYS_FS_LIST` mengembalikan maksimum 64 entri per panggilan dan tidak punya kursor; direktori yang lebih besar terpotong tanpa cara melanjutkan. Entri `.` dan `..` ikut dikembalikan apa adanya.

## Kompatibilitas

- Matriks `cargo xtask compat` (ADR-0010) mencakup sembilan konfigurasi QEMU: q35 dan i440fx, 1–4 vCPU, 2–8 GiB, `qemu64` dan `max`, virtio-blk modern/transisional/antrean kecil, tanpa disk, tanpa VGA, dan VGA vmware. Semua mem-boot image yang sama.
- **Di luar cakupan**: perangkat keras fisik, SMP (AP tidak dibangunkan apa pun `-smp`), boot legacy BIOS (hanya UEFI), firmware dengan 5-level paging (ditolak dengan pesan), disk selain virtio-blk (AHCI/NVMe), dan filesystem selain FAT32 read-only.
- Mesin tanpa disk melewati uji D01/A01 dan melaporkannya sebagai *skipped*; hitungannya terpisah dari yang lulus agar tidak terbaca seolah-olah dijalankan.

## Belum ada (tahap berikutnya)

- VirtIO net/input/display, jaringan (sisa tahap 3).
- SpaceLink, Tool Broker, Agent Runtime, Space Guard sebagai layanan, package service, tanda tangan paket (5A).
- GPU, ARM64 (6–7).

## Verifikasi

- Semua bukti berasal dari **QEMU TCG** dengan profil ADR-0003; belum pernah dicoba pada PC fisik.
- Soak 100 boot (K01) dijalankan pada satu host; stress 8 jam (PRD §9) belum dijalankan.
- Tidak ada fuzzing syscall; uji negatif bersifat contoh, bukan eksplorasi acak.
