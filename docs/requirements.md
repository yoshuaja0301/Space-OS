# Traceability persyaratan PRD §7

Status memakai label PRD §4: **planned** (belum dikerjakan), **experimental** (ada implementasi, bukti belum lengkap), **verified** (bukti uji otomatis tersedia di repo). Bukti dihasilkan oleh `cargo xtask test` / `cargo xtask soak` (lihat `docs/testing.md`) dan disalin ke `docs/evidence/`.

| ID | P | Persyaratan (ringkas) | Status | Bukti / catatan |
|---|---|---|---|---|
| K01 | P0 | Boot UEFI ke init di QEMU; 100 cold boot berturut-turut tanpa panic | **verified** | `xtask soak --boots 100` → `docs/evidence/soak-summary.txt` (100/100 pada build hardening, rata-rata 6,0 s per boot di TCG); skenario `acceptance` (marker `[init] Space OS init running`); diagnosis panic diuji oleh skenario `panic-diagnosis`, `kernel-fault-diagnosis`, `kernel-stack-overflow-diagnosis` |
| K02 | P0 | User-space, syscall, IPC, timer, isolasi; akses terlarang mematikan proses uji, bukan kernel | **verified** | `init` menjalankan: tulis ke memori kernel, baca NULL, eksekusi stack NX, lompat ke alamat kernel/non-kanonik, `div`, `ud2`, `cli`, `int3`, TF+`syscall` → proses dibunuh dengan alasan yang benar, kernel lanjut; `syscall` dengan `rsp` bermusuhan kembali normal; ELF rusak ditolak; 33 uji negatif ABI (`bin/abi_negative`); IPC echo + transfer handle; `sleep(50 ms)`; proses runaway di-preempt dan di-kill |
| K03 | P0 | Kuota dan reclamation; siklus berulang tanpa kebocoran yang terus tumbuh | **verified** | `bin/quota`: `mem_map` ditolak `Quota` tepat pada batas dan dapat dipakai ulang; 50 siklus spawn/exit `bin/worker`, 20 siklus kill-saat-blocking `bin/ipc_echo`, dan 3 × 20 siklus kill saat `sleep`/`recv`/`wait` (`bin/blocker`) → jumlah frame bebas dan heap kernel identik sebelum/sesudah |
| D01 | P0 | VirtIO block dan VFS; baca model dari disk guest, verifikasi checksum setelah reboot | planned | Tahap 3. Bootloader sudah memuat file dari ESP; kernel belum punya driver blok |
| C01 | P0 | Compute ABI CPU: versi, invalid handle, batas buffer, unsupported op, timeout, cancel | planned | Tahap 4. Model handle/hak/error code ABI v0 (ADR-0004) dirancang agar Compute ABI memakai mekanisme yang sama |
| A01 | P0 | Model kecil menghasilkan 128 token offline sesuai baseline | planned | Tahap 5; memerlukan D01 + C01 |
| L01–L03 | P1 | SpaceLink indeks, revokasi, context bundle | planned | Developer Preview; menunggu akses repo SpaceLink (PRD §10) |
| G01 | P1 | Agent membaca/patch/uji dalam workspace; keluar scope ditolak | planned | Model capability kernel sudah default-deny; Tool Broker belum ada |
| I01 | P1 | Satu adapter cloud lulus auth/streaming/tool use/timeout/cost cap | planned | Memerlukan jaringan (tahap 3) |
| U01 | P1 | Desktop/terminal/file manager/Stop tetap hidup saat worker inferensi crash | experimental | Primitive terbukti: worker user-space yang crash/di-kill tidak mengganggu `init` (uji K02); belum ada desktop |
| P01 | P1 | Paket bertanda tangan dan rollback | planned | — |
| H01 | P2 | PC/GPU referensi | planned | — |
| H02 | P2 | ARM64 boot/isolasi/inferensi CPU virtual | planned | Batas modul `kernel/src/arch/` disiapkan (ADR-0001) |

## Pemetaan uji → persyaratan

`user/init/src/main.rs` memberi label setiap uji dengan ID persyaratan; ringkasan lulus/gagal dicetak sebagai `[init] PASS <ID>: …` / `[init] FAIL <ID>: …` dan kemudian `[init] ALL TESTS PASSED (n/n)` atau `[init] TESTS FAILED`. Harness membaca marker ini dan kode keluar QEMU.
