# Bukti uji (sesi verifikasi 2026-09-16; diperbarui 2026-09-30 dengan desktop, Agent Center dan Command Center)

Log serial guest yang dihasilkan `cargo xtask test` dan `cargo xtask soak --boots 100`, dibersihkan dari escape ANSI OVMF. Semua berasal dari profil ADR-0003 (QEMU TCG); bukan performa perangkat fisik.

| File | Skenario | Hasil |
|---|---|---|
| `acceptance.log` | boot normal, `init` menjalankan 116 uji K01–K03, D01, C01, A01, U01, G01, L01–L03, P01, I01, jaringan (`NET`) dan `TLS` | `ALL TESTS PASSED (116/116, 0 skipped)`, exit QEMU 33; termasuk 128 token inferensi yang cocok dengan baseline, sesi yang bertahan melewati empat worker, model yang menulis di Agent Center lalu berhenti di antara dua langkah saat Stop ditekan (`Stop took 2 ms`), Command Center yang asal hasilnya dicocokkan dengan byte di disk, berkas yang berubah diindeks ulang sendirian dalam 3 ms tanpa full rescan, agent yang tidak bisa keluar workspace, context bundle yang diverifikasi provenance-nya, dan paket yang ditolak dengan alasannya masing-masing. Volume data ditulis dan dibaca ulang byte demi byte, dan store paket, daftar revokasi serta tambalan agent semuanya mendarat di disk. Controller PS/2 juga disuruh menekan satu tombol sendiri, jadi IRQ 1 terbukti hidup tanpa ada orang di depan mesin. Ring masukan konsol diluapkan dengan sengaja di bawah sesi yang hidup: `console input: 8 byte(s) dropped` lalu `[shell] input was lost; the line was discarded` |
| `panic-diagnosis.log` | `selftest=panic` | pesan panic + lokasi + backtrace, exit 127 |
| `kernel-fault-diagnosis.log` | `selftest=kfault` | dump register page fault ring 0 (`cr2=0xfffff000dead0000`) lalu panic, exit 127 |
| `kernel-stack-overflow-diagnosis.log` | `selftest=stack` | double fault dari guard page kernel stack (stack IST), exit 127 |
| `init-exit-diagnosis.log` | proses pertama selesai tanpa meminta shutdown (`init=bin/hello`) | kernel mengatakannya dan berhenti, exit 35 |
| `init-missing-diagnosis.log` | `init=` menyebut program yang tidak ada | panic yang menyebut nama program itu, exit 127 |
| `terminal.log` | sesi interaktif dikendalikan dari **keyboard** (monitor QEMU `sendkey`; mesin ini tidak punya COM2) | tiap perintah yang diketik dijawab, `stop` menghentikan worker yang macet, `quit` menutup sesi; exit 33 |
| `terminal-serial.log` | sesi yang sama lewat **konsol serial** COM2 (pty) | sama, dengan `console input: keyboard (IRQ1) and COM2 serial (IRQ3)` |
| `desktop.log`, `desktop-1-terminal.png` … `desktop-6-command-center.png` | desktop (ADR-0020, `init=bin/spacedesk`) dikendalikan dari **keyboard** lewat monitor QEMU | layar di-lease ke `bin/spacedesk`; terminal, file manager dan Agent Center dibuka dengan pintasan; worker dibuat crash dan terminal menjawab `status` yang diketik sesudahnya; worker macet dihentikan Stop; model dijalankan dari Agent Center lalu dihentikan Stop di antara dua langkah (ADR-0021); ubah ukuran, pindah, minimize, workspace, kontras tinggi, Alt+F4; Command Center: Super+Space, ketik `channel`, Enter, Ctrl+B, Ctrl+O membuka file manager dengan `IPC.TXT` terpilih (ADR-0022); lalu Ctrl+Alt+Delete; exit 33. Screenshot: terminal, setelah worker crash, inferensi yang dihentikan, kontras tinggi, setelah jendela ditutup, Command Center |
| `storage-reboot-boot1.log`, `storage-reboot-boot2.log` | image yang sama di-boot dua kali (D01 setelah reboot) | virtio-blk siap, FAT32 ter-mount, checksum model cocok pada kedua boot, dan boot kedua **menemukan penghitung yang ditinggalkan boot pertama** (`persistence: generation N survived the reboot`) |
| `compat-summary.txt` | `cargo xtask compat`: sepuluh konfigurasi mesin (ADR-0010) | semuanya boot dan lulus; image yang sama untuk semua, dengan kartu jaringan dan ringkasan TCP-nya per mesin |
| `network-summary.txt` | jaringan (ADR-0016): driver, 16 uji `NET`, layanan `spacenet`, layanan lab, dan pemeriksaan pcap dari skenario acceptance | lease, ARP/ICMP, bangun saat tidur, DHCP, DNS, 64 KiB TCP utuh, 4 penolakan dengan 0 frame terkirim, timeout, `Refused`, `Reset`, 20 koneksi tanpa kebocoran, layanan dibunuh lalu dijalankan ulang; di kabel: setiap FIN peer di-ACK, 0 segmen dikirim ulang |
| `tls-summary.txt` | TLS dan entropi (ADR-0017): gerbang build tanpa instruksi FPU/vektor, sumber entropi kernel, uji K02 `SYS_RANDOM` dan FPU mati, 8 uji `TLS` dengan jawaban `bin/tlsprobe`, log server TLS lab, mesin lain, dan gigi uji | tiga suite TLS 1.3 membawa 16 KiB utuh (handshake 40–70 ms); sertifikat kedaluwarsa, salah nama dan tak dikenal ditolak dengan alert yang diterima server; record yang diubah tertangkap (`BadRecordMac`); pemotongan dilaporkan; `cpu-max` memakai RDRAND, `i440fx` melewati TLS tanpa entropi |
| `cloud-summary.txt` | adapter cloud (ADR-0018) terhadap penyedia **tiruan**: 11 uji `I01` dan uji `NET` pemutusan aliran, baris adapter, log penyedia tiruan, dan gigi uji (8 pelemahan) | kredensial, streaming, alat lewat broker, penolakan alat untuk kredensial, tenggat, retry terbatas, JSON bermusuhan, pemutusan saat melewati reservasi, budget dan local-only ditolak dengan 0 frame |
| `compat-no-disk.log` | mesin tanpa perangkat blok | `virtio-blk: no device present`, uji berbasis disk dilewati, `ALL TESTS PASSED (77/77, 39 skipped)`; uji jaringan tetap jalan karena kartunya ada, uji TLS dan I01 dilewati karena sertifikat otoritas lab ada di disk |
| `compat-virtio-small-queue.log` | virtio-blk dengan `queue-size=4` | antrean hasil negosiasi 4 deskriptor, 2 halaman data per permintaan, FAT32 tetap ter-mount |
| `compat-no-vga.log` | mesin tanpa VGA (tanpa GOP) | `framebuffer: none usable; serial console only`, keenam uji yang membutuhkan layar (termasuk Agent Center dan Command Center) dilewati dengan alasan, `ALL TESTS PASSED (110/110, 6 skipped)` |
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
