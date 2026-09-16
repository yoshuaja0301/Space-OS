# ADR-0007: Driver tempat, VirtIO block, FAT32 read-only, dan ABI file

Status: Diterima — 2026-09-16

## Konteks

Tahap 3 PRD meminta VirtIO block dan VFS agar model dapat dibaca dari disk guest dan checksum-nya diverifikasi setelah reboot (D01). PRD §2 juga menyatakan driver user-space menerima akses MMIO/interrupt/DMA secara terbatas, dan **melarang** menyebut driver aman hanya karena berada di user-space.

## Keputusan

1. **Driver di kernel untuk MVP.** `pci`, `virtio_blk`, dan `fat32` berjalan di kernel. Alasannya: driver user-space memerlukan capability MMIO/IRQ/DMA yang belum ada, dan tanpa IOMMU driver DMA tetap komponen tepercaya (PRD §2) — memindahkannya ke user-space sekarang menambah mekanisme tanpa menambah isolasi nyata. Ditulis eksplisit di `docs/limitations.md`, bukan diklaim sebagai desain akhir. Pemindahan ke user-space adalah pekerjaan Developer Preview bersama capability MMIO/IRQ dan dukungan IOMMU.
2. **VirtIO 1.0 modern saja, polling.** Kemampuan PCI vendor-specific (common/notify/device cfg) dipetakan uncached ke jendela MMIO khusus (`0xFFFF_B000_0000_0000`, PCD+PWT+NX). Antrean split ukuran 16 dalam satu halaman DMA; permintaan baca memakai scatter-gather hingga 8 halaman (32 KiB) per permintaan. Penyelesaian dideteksi dengan polling berbatas (`POLL_LIMIT`), sehingga perangkat yang macet menghasilkan error, bukan hang. Interrupt dan antrean asinkron menyusul bersama driver user-space. Perangkat legacy/transitional tanpa kapabilitas modern diabaikan dengan pesan.
3. **FAT32 read-only, nama 8.3.** Volume data adalah FAT32 tanpa tabel partisi. Entri long-name dilewati; berkas yang harus dibaca guest diberi nama 8.3. Setiap nilai dari volume (bytes/sector, cluster, ukuran FAT, rantai cluster) divalidasi sebelum dipakai, jadi volume rusak atau bermusuhan menghasilkan error, bukan panic. FAT dibaca lewat cache satu sektor.
4. **ABI file.** `SYS_FS_OPEN(root, path)` membutuhkan hak root `FS` dan mengembalikan handle objek `File` dengan hak `READ|TRANSFER|DUP`; `SYS_FS_READ(file, offset, buf, len)` membutuhkan `READ`; `SYS_FS_STAT` melaporkan ukuran dan ukuran blok. Path maksimum 255 byte, case-insensitive. Pointer tujuan divalidasi sebelum perangkat disentuh agar pointer buruk tidak menghabiskan satu pembacaan.
5. **Bukti D01.** Disk data dibuat `xtask` (model + `manifest.txt` berisi ukuran dan SHA-256 yang dihitung pustaka host independen). `init` menghitung SHA-256 di guest dengan implementasi sendiri (diuji lebih dulu terhadap vektor FIPS: string kosong, "abc", sejuta 'a') lalu membandingkannya dengan manifest. Skenario `storage-reboot` menjalankan image yang sama dua kali berturut-turut dan menuntut verifikasi lulus pada kedua boot.

## Konsekuensi

- Kernel tumbuh ~900 baris; batas "microkernel" untuk MVP kini eksplisit: penjadwalan, memori, IPC, capability, plus driver blok dan FAT32 read-only.
- Belum ada: tulis ke disk, cache blok, beberapa volume, mount table, jaringan, input, display. Lihat `docs/limitations.md`.
