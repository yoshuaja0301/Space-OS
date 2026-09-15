# Pengujian

Semua uji berjalan **di dalam guest** (kernel + user-space Space OS); host hanya menjalankan QEMU dan membaca log serial serta kode keluar. Tidak ada uji yang bergantung pada Linux di dalam guest.

## Skenario `cargo xtask test`

| Skenario | cmdline | Harapan |
|---|---|---|
| `acceptance` | — | marker `[kernel] selftest: heap ok`, `[init] Space OS init running`, `[init] ALL TESTS PASSED`; exit 33 |
| `panic-diagnosis` | `selftest=panic` | `!!! KERNEL PANIC !!!`, pesan, `backtrace (frame pointers):`, `spacekernel: halted after panic`; exit 127 |
| `kernel-fault-diagnosis` | `selftest=kfault` | `!!! CPU EXCEPTION IN KERNEL MODE: page fault !!!`, `cr2=0xfffff000dead0000`, lalu panic; exit 127 |
| `kernel-stack-overflow-diagnosis` | `selftest=stack` | `!!! CPU EXCEPTION IN KERNEL MODE: double fault !!!` (guard page kernel stack), lalu panic; exit 127 |

Kode keluar QEMU berasal dari `isa-debug-exit`: `(nilai << 1) | 1`; kernel menulis `0x10` (sukses, 33), `0x11` (uji gagal, 35), `0x3f` (panic, 127). Time-out 240 detik per boot dihitung sebagai gagal.

## Uji yang dijalankan `init` (skenario acceptance)

| ID | Uji | Program |
|---|---|---|
| K01 | init tercapai, 1 proses hidup | — |
| K02 | proses hello keluar 0 | `bin/hello` |
| K02 | spawn program tak ada → `NotFound`; spawn dengan kuota 4 halaman → `Quota` | — |
| K02 | tulis memori kernel / baca NULL / eksekusi stack NX / lompat ke alamat kernel → dibunuh `PAGE_FAULT`; `div` → `DIVIDE_ERROR`; `ud2` → `INVALID_OPCODE`; `cli` → `GENERAL_PROTECTION`; `int3` → `BREAKPOINT`; lompat ke alamat non-kanonik → `GENERAL_PROTECTION` (CPU asli) atau `PAGE_FAULT` (TCG) | `bin/fault` |
| K02 | TF disetel lalu `syscall`: `#DB` mendarat di ring 0 → kernel selamat, proses dimatikan `DEBUG` | `bin/fault` |
| K02 | `syscall` dengan `rsp` non-kanonik dan dengan `rsp` = alamat kernel → kembali normal, proses keluar 0 | `bin/fault` |
| K02 | ELF rusak dari initrd (`fixtures/bad_entry`, `bad_magic`, `truncated`, `huge_segment`) → `NoExec`/`Quota`, tanpa crash | fixture dibuat `xtask` dari `hello` |
| K02 | pointer kernel ke syscall → `Fault`, tidak didereferensi | `bin/fault` |
| K02 | 33 uji negatif ABI: nomor syscall salah, handle salah, jenis objek salah, pointer buruk, hak dipersempit tidak bisa diperluas, transfer endpoint sesama channel ditolak tanpa kehilangan handle, pesan terlalu besar tetap di antrean, peer tertutup, double close | `bin/abi_negative` |
| K02 | IPC echo 3 pesan + transfer handle + `PeerClosed` mengakhiri layanan | `bin/ipc_echo` |
| K02 | `sleep(50 ms)` memajukan `ticks` | — |
| K02 | proses loop tanpa syscall tidak membuat init kelaparan; `kill` → `SIGNAL` | `bin/spin` |
| K03 | kuota 100 halaman: `mem_map` ditolak tepat pada batas, halaman nol, dilepas dan dapat dipakai lagi | `bin/quota` |
| K03 | 50 siklus spawn/IPC/exit → frame bebas dan heap kernel identik | `bin/worker` |
| K03 | 20 siklus "kill saat blocking di `recv`" → peer melihat `PeerClosed`, frame bebas dan heap kernel identik | `bin/ipc_echo` |

## Selftest kernel (sebelum user-space)

`kernel/src/selftest.rs`: heap alokasi/bebas tanpa selisih; 64 frame berbeda dan kembali penuh; map/write/translate/unmap halaman kernel; address space user map/cek-akses/tolak-overlap/unmap/drop tanpa selisih frame.

## Soak K01

`cargo xtask soak --boots 100` menjalankan 100 cold boot skenario acceptance, berhenti pada kegagalan pertama, dan menulis `build/logs/soak/summary.txt` + log per boot. Hasil sesi ini ada di `docs/evidence/soak-summary.txt`.

## Bukti

`docs/evidence/` berisi log serial dari sesi verifikasi (dibersihkan dari escape ANSI OVMF). Bukti harus diperbarui bila perilaku berubah; CI menjalankan `cargo xtask ci` pada setiap push/PR dan soak 100 boot secara manual/terjadwal (`.github/workflows/ci.yml`).
