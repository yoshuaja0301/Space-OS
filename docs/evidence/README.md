# Bukti uji (sesi verifikasi 2026-09-16)

Log serial guest yang dihasilkan `cargo xtask test` dan `cargo xtask soak --boots 100`, dibersihkan dari escape ANSI OVMF. Semua berasal dari profil ADR-0003 (QEMU TCG); bukan performa perangkat fisik.

| File | Skenario | Hasil |
|---|---|---|
| `acceptance.log` | boot normal, `init` menjalankan 54 uji K01–K03, D01, C01, A01, U01, G01, L01–L03 dan P01 | `ALL TESTS PASSED (54/54, 0 skipped)`, exit QEMU 33; termasuk 128 token inferensi yang cocok dengan baseline, sesi yang bertahan melewati empat worker, agent yang tidak bisa keluar workspace, context bundle yang diverifikasi provenance-nya, dan paket yang ditolak dengan alasannya masing-masing. Controller PS/2 juga disuruh menekan satu tombol sendiri, jadi IRQ 1 terbukti hidup tanpa ada orang di depan mesin. Ring masukan konsol diluapkan dengan sengaja di bawah sesi yang hidup: `console input: 8 byte(s) dropped` lalu `[shell] input was lost; the line was discarded` |
| `panic-diagnosis.log` | `selftest=panic` | pesan panic + lokasi + backtrace, exit 127 |
| `kernel-fault-diagnosis.log` | `selftest=kfault` | dump register page fault ring 0 (`cr2=0xfffff000dead0000`) lalu panic, exit 127 |
| `kernel-stack-overflow-diagnosis.log` | `selftest=stack` | double fault dari guard page kernel stack (stack IST), exit 127 |
| `init-exit-diagnosis.log` | proses pertama selesai tanpa meminta shutdown (`init=bin/hello`) | kernel mengatakannya dan berhenti, exit 35 |
| `init-missing-diagnosis.log` | `init=` menyebut program yang tidak ada | panic yang menyebut nama program itu, exit 127 |
| `terminal.log` | sesi interaktif dikendalikan dari **keyboard** (monitor QEMU `sendkey`; mesin ini tidak punya COM2) | tiap perintah yang diketik dijawab, `stop` menghentikan worker yang macet, `quit` menutup sesi; exit 33 |
| `terminal-serial.log` | sesi yang sama lewat **konsol serial** COM2 (pty) | sama, dengan `console input: keyboard (IRQ1) and COM2 serial (IRQ3)` |
| `storage-reboot-boot1.log`, `storage-reboot-boot2.log` | image yang sama di-boot dua kali (D01 setelah reboot) | virtio-blk siap, FAT32 ter-mount, checksum model cocok pada kedua boot |
| `compat-summary.txt` | `cargo xtask compat`: sembilan konfigurasi mesin (ADR-0010) | semuanya boot dan lulus; image yang sama untuk semua |
| `compat-no-disk.log` | mesin tanpa perangkat blok | `virtio-blk: no device present`, uji berbasis disk dilewati, `ALL TESTS PASSED (44/44, 10 skipped)` |
| `compat-virtio-small-queue.log` | virtio-blk dengan `queue-size=4` | antrean hasil negosiasi 4 deskriptor, 2 halaman data per permintaan, FAT32 tetap ter-mount |
| `soak-summary.txt` | 100 cold boot skenario acceptance pada build lengkap tahap 5 (K01) | lihat isi file; setiap boot memuat model dari disk, memverifikasi checksum, dan menghasilkan 128 token |

## Lingkungan host

| Komponen | Versi |
|---|---|
| Host OS | Ubuntu 24.04.4 LTS (container, tanpa KVM → TCG) |
| QEMU | 8.2.2 (Debian 1:8.2.2+ds-0ubuntu1.18) |
| OVMF | 2024.02-2ubuntu0.9 (`OVMF_CODE_4M.fd`) |
| Rust | 1.94.1 (e408947bf 2026-03-25), target `x86_64-unknown-none`, `x86_64-unknown-uefi` |
| Profil QEMU | q35, TCG, 4 vCPU `qemu64`, 8 GiB, AHCI, `isa-debug-exit` (acuan; variasi lain ada di `compat-summary.txt`) |

Untuk mereproduksi: `cargo xtask test`, `cargo xtask compat`, lalu `cargo xtask soak --boots 100`; log baru ada di `build/logs/`.
