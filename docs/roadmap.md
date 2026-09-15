# Roadmap PRD §8 vs kondisi repo

| Tahap | Hasil kerja PRD | Status repo |
|---|---|---|
| 1 Kernel boot | Bootloader UEFI, kernel, memori awal, serial dan crash log | **selesai**: `spaceboot`, `spacekernel`, frame/paging/heap, serial + framebuffer, panic dengan backtrace, 3 skenario diagnosis crash, soak 100 boot |
| 2 User space CPU | Address space, syscall, ELF loader, IPC, allocator, scalar compute | **selesai kecuali "scalar compute"**: address space per proses, 20 syscall ABI v0, loader ELF, channel IPC + transfer handle, allocator kernel/user, kuota. "Scalar compute" (operasi CPU untuk inferensi) ditunda ke tahap 4 karena bergantung pada kontrak Compute ABI |
| 3 Perangkat virtual | VirtIO block, VFS, network, input, display | belum |
| 4 Compute ABI | Kontrak v0, CPU backend, handle dan quota | belum (model handle/hak/error sudah disiapkan di `spaceabi`) |
| 5 Inferensi native | Model kecil, tokenizer, generation, benchmark offline | belum |
| 5A Developer Preview | SpaceLink, Tool Broker, agent, desktop, adapter | belum |
| 6 GPU terpilih | Studi kelayakan, driver | belum |
| 7 Hardware Preview | Installer, recovery, matriks hardware, ARM64 | belum |

## Backlog berikutnya (urutan PRD "Urutan backlog pertama" sudah selesai sampai "syscall/IPC serta negative tests")

1. **Storage (D01)**: driver VirtIO block (PCI modern, virtqueue split) sebagai *user-space server* dengan akses MMIO/IRQ lewat capability baru; VFS minimal read-only (FAT dari ESP) → baca file model + verifikasi hash SHA-256 → uji reboot.
2. **Compute ABI v0 (C01)**: objek `MemoryObject` yang dapat di-map dua proses (buffer), layanan `spacecompute` user-space dengan `device_query/buffer_create/buffer_map/queue_create/submit/wait/cancel/release`, contract test versi/invalid handle/batas/unsupported/timeout/cancel.
3. **Inferensi (A01)**: port engine CPU kecil untuk satu arsitektur model, tokenizer, generate 128 token, baseline dipatok.
4. SMP + LAPIC (ADR-0006), thread ganda, timeout IPC.
