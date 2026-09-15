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
- Thread yang di-`kill` saat menunggu `wait` pada proses yang belum keluar tetap tercatat di wait queue proses itu sampai proses tersebut keluar (Arc lepas terlambat, bukan bocor permanen).
- Siklus referensi antar-channel (endpoint A dikirim lewat channel B dan endpoint B dikirim lewat channel A) tidak dideteksi dan bocor; siklus satu channel ditolak (`Invalid`).
- NMI bersarang (NMI kedua saat handler NMI belum selesai) merusak frame di stack IST NMI.
- Headroom heap kernel 1 MiB menolak alokasi yang dipicu user, tetapi fragmentasi ekstrem masih dapat membuat alokasi internal kernel gagal (panic).
- Hanya **PIC + PIT**; belum ada ACPI/LAPIC/IOAPIC/HPET; RSDP hanya diteruskan.
- Framebuffer dipetakan write-back lewat linear map (cukup untuk QEMU; perangkat fisik memerlukan write-combining/PAT).
- Reklamasi memori `BOOTLOADER_RECLAIMABLE` dilakukan segera; UEFI runtime services tidak dipakai (region-nya dibiarkan RESERVED).
- Keyboard hanya dikuras (IRQ1), tidak diteruskan ke user-space.

## Belum ada (tahap berikutnya)

- Driver VirtIO block/net/input/display, VFS, jaringan (tahap 3, D01).
- Compute ABI v0, backend CPU, tokenizer, inferensi (tahap 4–5, C01, A01).
- SpaceLink, Tool Broker, Agent Runtime, Space Guard sebagai layanan, Space Shell, package service, tanda tangan paket (5A).
- GPU, ARM64 (6–7).

## Verifikasi

- Semua bukti berasal dari **QEMU TCG** dengan profil ADR-0003; belum pernah dicoba pada PC fisik.
- Soak 100 boot (K01) dijalankan pada satu host; stress 8 jam (PRD §9) belum dijalankan.
- Tidak ada fuzzing syscall; uji negatif bersifat contoh, bukan eksplorasi acak.
