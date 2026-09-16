# Roadmap PRD §8 vs kondisi repo

| Tahap | Hasil kerja PRD | Status repo |
|---|---|---|
| 1 Kernel boot | Bootloader UEFI, kernel, memori awal, serial dan crash log | **selesai**: `spaceboot`, `spacekernel`, frame/paging/heap, serial + framebuffer, panic dengan backtrace, 3 skenario diagnosis crash, soak 100 boot |
| 2 User space CPU | Address space, syscall, ELF loader, IPC, allocator, scalar compute | **selesai kecuali "scalar compute"**: address space per proses, 20 syscall ABI v0, loader ELF, channel IPC + transfer handle, allocator kernel/user, kuota. "Scalar compute" (operasi CPU untuk inferensi) ditunda ke tahap 4 karena bergantung pada kontrak Compute ABI |
| 3 Perangkat virtual | VirtIO block, VFS, network, input, display | **sebagian**: PCI, virtio-blk 1.0 (polling), FAT32 read-only dan ABI file selesai (D01 verified); masukan konsol (keyboard PS/2 dan COM2) selesai; network dan display di luar konsol teks belum |
| 4 Compute ABI | Kontrak v0, CPU backend, handle dan quota | **selesai**: objek memori bersama di kernel, layanan `spacecompute` user-space, backend CPU, uji kontrak C01 lulus |
| 5 Inferensi native | Model kecil, tokenizer, generation, benchmark offline | **selesai untuk model referensi**: format SpaceLM v0, validasi + checksum, runtime `spaceai`, 128 token offline cocok dengan baseline; model terlatih berlisensi belum |
| 5A Developer Preview | SpaceLink, Tool Broker, agent, desktop, adapter | **sebagian**: `spaceshell` + `spaceterm` (sesi yang bertahan melewati worker crash dan bisa diketik orang, U01), `spacebroker` + `spaceagent` (scope workspace dan audit, G01), `spacelink` (indeks, revokasi, context bundle, L01–L03). `spacepkg` (paket terautentikasi dan rollback, P01). Adapter cloud (I01) terhalang jaringan dan TLS |
| 6 GPU terpilih | Studi kelayakan, driver | **studi selesai, driver belum**: `docs/gpu-feasibility.md` memetakan apa yang sudah siap (Compute ABI v0 tidak menyebut backend), tiga kapabilitas perangkat yang belum ada untuk driver user-space, dan mengapa tanpa IOMMU driver DMA tetap tepercaya. Driver tidak ditulis: tidak ada perangkat keras untuk memverifikasinya (H01) |
| 7 Hardware Preview | Installer, recovery, matriks hardware, ARM64 | **sebagian**: matriks konfigurasi mesin virtual (`cargo xtask compat`, ADR-0010) berjalan di CI; perangkat keras fisik, installer, recovery dan ARM64 belum |

## Backlog berikutnya (urutan PRD "Urutan backlog pertama" sudah selesai sampai "syscall/IPC serta negative tests")

1. **Storage (D01)**: driver VirtIO block (PCI modern, virtqueue split) sebagai *user-space server* dengan akses MMIO/IRQ lewat capability baru; VFS minimal read-only (FAT dari ESP) → baca file model + verifikasi hash SHA-256 → uji reboot.
2. **Compute ABI v0 (C01)**: objek `MemoryObject` yang dapat di-map dua proses (buffer), layanan `spacecompute` user-space dengan `device_query/buffer_create/buffer_map/queue_create/submit/wait/cancel/release`, contract test versi/invalid handle/batas/unsupported/timeout/cancel.
3. **Inferensi (A01)**: port engine CPU kecil untuk satu arsitektur model, tokenizer, generate 128 token, baseline dipatok.
4. SMP + LAPIC (ADR-0006), thread ganda, timeout IPC, `select`/multi-wait (menghapus polling 2 ms di `spaceshell`), dan kepemilikan konsol yang ditegakkan kernel (sekarang antrean masukan global).
5. **Penyimpanan yang bisa ditulis**: FAT32 tulis atau filesystem sendiri. Ini satu-satunya penghalang untuk tambalan agent yang persisten (G01), revokasi yang bertahan reboot (L02), dan instalasi/rollback paket (P01).
6. **Jaringan**: virtio-net, stack TCP/IP, dan TLS. I01 (adapter cloud dengan auth, streaming, tool use, timeout, cost cap) tidak bisa dimulai sebelum ketiganya ada; TLS tidak akan ditulis sendiri.
7. **ARM64 (H02)**: port `kernel/src/arch/aarch64` (boot EL1, MMU tabel translasi, vektor eksepsi, GIC, timer generik), profil QEMU `virt` + AAVMF, lalu ulangi K01–K03 di sana.
