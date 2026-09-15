# ADR-0004: ABI syscall dan model capability versi 0

Status: Diterima — 2026-09-15

## Konteks

PRD §2: kernel berorientasi capability; §5: default-deny, hak dapat dicabut, hasil dari luar adalah data tidak tepercaya. Diperlukan ABI kernel↔user yang eksplisit, berversi, tanpa pointer mentah lintas proses (§4 untuk Compute ABI menuntut hal yang sama).

## Keputusan

Sumber kebenaran: crate `abi/spaceabi` (`ABI_VERSION = 0`).

**Konvensi pemanggilan**: instruksi `syscall`; nomor di `rax`, argumen di `rdi, rsi, rdx, r10, r8, r9`; hasil di `rax` sebagai `isize` (negatif = kode error `spaceabi::error::Error`). Blok argumen besar (`RecvArgs`, `SpawnArgs`, `ExitStatus`, `KernelStats`) adalah struct `repr(C)` di memori user.

**Validasi**: setiap pointer user diperiksa terhadap page table pemanggil (harus user-accessible, dan writable bila ditulis) sebelum disentuh; pointer kernel/half atas atau halaman tak terpeta → `Fault`, tanpa kernel pernah mendereferensi. Ukuran salin maksimum 1 MiB.

**Capability**: handle = indeks tabel per proses ke objek kernel (`Channel`, `Process`, `Root`) + bit hak (`SEND, RECV, TRANSFER, DUP, WAIT, KILL, SPAWN, STATS, SHUTDOWN, DEBUG`). Hak hanya bisa dipersempit (`handle_dup` dengan mask), tidak pernah diperluas. Objek dengan jenis salah → `Denied`; indeks tak ada → `BadHandle`. Tidak ada identitas global: proses hanya dapat memengaruhi proses lain lewat handle `Process` yang diterimanya. `init` menerima handle `Root` (spawn, statistik, shutdown, debug); anak menerima apa yang diberikan orang tuanya pada handle 0.

**Transfer handle**: handle dipindah (bukan disalin) lewat pesan channel atau saat spawn; membutuhkan hak `TRANSFER`. Handle yang dipindahkan dianggap terkonsumsi walaupun pengiriman gagal (aturan sederhana, tanpa "setengah-transfer"). Endpoint dari channel yang sama (sisi mana pun, termasuk duplikatnya) tidak boleh dikirim lewat channel itu sendiri (`Invalid`): pesan seperti itu membentuk siklus referensi yang tidak pernah bisa diterima siapa pun.

**IPC**: channel dua arah, pesan ≤ 256 byte inline + satu handle opsional, antrean terbatas 64 pesan (`WouldBlock` bila penuh; `send` tidak pernah memblokir), `recv` memblokir kecuali `NONBLOCK`; pesan yang tidak muat di buffer penerima tetap di antrean (`MsgSize`); sisi peer tertutup → `PeerClosed`.

**Kuota**: setiap proses memiliki `quota_pages` yang dicek pada setiap pemetaan (kode, stack, `mem_map`); melampaui → `Quota`, bukan pembunuhan proses.

**Terminasi**: exception dari ring 3 mematikan proses dengan `ExitStatus::killed(reason, addr)` (`PAGE_FAULT`, `GENERAL_PROTECTION`, `INVALID_OPCODE`, `DIVIDE_ERROR`, `DEBUG` untuk `#DB`/single-step, `BREAKPOINT` untuk `int3`, `OTHER_EXCEPTION`); `kill` dari pemegang handle `KILL` menghasilkan alasan `SIGNAL`. Kernel tidak pernah panic karena kesalahan program user: `#DB` yang mendarat di instruksi kernel pertama setelah `syscall` (user menyetel TF) ditangani di stack IST sendiri dan proses dimatikan saat trap terulang di ring 3; `syscall` dengan `rsp` user non-kanonik/alamat kernel tidak pernah disentuh kernel; `rip` kembali yang bukan alamat user mematikan proses alih-alih `sysret` (#GP ring 0).

**Batas alokasi kernel yang dipicu user**: syscall yang membuat objek kernel (pesan, channel, handle, proses, region) menolak dengan `NoMemory` bila heap kernel akan turun di bawah headroom 1 MiB, dan memakai `try_reserve` agar kehabisan memori menjadi error syscall, bukan panic kernel.

Daftar syscall: lihat `abi/spaceabi/src/syscall.rs` (`nr::*`).

## Konsekuensi

- Program user tidak dapat memanggil apa pun tanpa handle yang tepat → default-deny.
- Belum ada: thread ganda per proses, memory object yang dapat dibagi antarproses (shared memory), timeout pada `recv`/`wait`, dan kirim pesan yang memblokir. Direncanakan untuk ABI v1 bersama Compute ABI (tahap 4).
