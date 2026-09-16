# Pengujian

Semua uji berjalan **di dalam guest** (kernel + user-space Space OS); host hanya menjalankan QEMU dan membaca log serial serta kode keluar. Tidak ada uji yang bergantung pada Linux di dalam guest.

## Skenario `cargo xtask test`

| Skenario | cmdline | Harapan |
|---|---|---|
| `acceptance` | — | marker `[kernel] selftest: heap ok`, `[init] Space OS init running`, `[init] ALL TESTS PASSED`; exit 33 |
| `panic-diagnosis` | `selftest=panic` | `!!! KERNEL PANIC !!!`, pesan, `backtrace (frame pointers):`, `spacekernel: halted after panic`; exit 127 |
| `kernel-fault-diagnosis` | `selftest=kfault` | `!!! CPU EXCEPTION IN KERNEL MODE: page fault !!!`, `cr2=0xfffff000dead0000`, lalu panic; exit 127 |
| `kernel-stack-overflow-diagnosis` | `selftest=stack` | `!!! CPU EXCEPTION IN KERNEL MODE: double fault !!!` (guard page kernel stack), lalu panic; exit 127 |
| `storage-reboot` | — | image yang sama di-boot dua kali; kedua boot harus memuat virtio-blk, mount FAT32, dan lulus D01 (checksum model) |
| `init-exit-diagnosis` | `init=bin/hello` | proses pertama yang **selesai tanpa meminta shutdown** tidak meninggalkan apa pun untuk dijadwalkan. Kernel harus mengatakannya (`ended without requesting shutdown; nothing left to run`) dan berhenti dengan exit 35 — bukan menggantung seperti hang |
| `init-missing-diagnosis` | `init=bin/not_a_program` | `init=` yang salah ketik harus menyebut program yang benar-benar gagal: `cannot start "bin/not_a_program" from initrd: not found`, lalu panic; exit 127 |
| `terminal` | `init=bin/spaceterm` | image yang **sama**, di-boot ke sesi interaktif dan dikendalikan dari **keyboard**: harness menekan tombol lewat monitor QEMU (`sendkey`), jadi jalurnya scan code → IRQ 1 → decoder kernel. Mesin ini tidak punya COM2 sama sekali (log wajib memuat `no COM2 UART`), jadi tiap ketikan pasti datang dari keyboard. Diketik `help`, `status`, `ls /spaceos`, `run hang`, `status`, `stop`, `status`, `quit`; exit 33 |
| `terminal-serial` | `init=bin/spaceterm` | sesi yang sama lewat **konsol serial**: COM2 sebagai pty, harness menulis byte ke sana (IRQ 3). Log wajib memuat `keyboard (IRQ1) and COM2 serial (IRQ3)`. Perintah dan harapan sama dengan `terminal` |

## Matriks kompatibilitas `cargo xtask compat` (ADR-0010)

Image yang sama di-boot pada setiap konfigurasi; semua harus mencapai
`[init] ALL TESTS PASSED` dan exit 33, tanpa `KERNEL PANIC` atau `[init] FAIL`.

| Mesin | QEMU | Marker tambahan |
|---|---|---|
| `lab` | q35, `qemu64`, 4 vCPU, 8 GiB, virtio-blk modern | — (acuan ADR-0003) |
| `q35-1cpu-2g` | 1 vCPU, 2 GiB | `vfs: FAT32 mounted` |
| `i440fx` | `-machine pc`, 2 vCPU, 4 GiB | `vfs: FAT32 mounted` |
| `cpu-max` | `-cpu max` | — |
| `virtio-transitional` | `disable-legacy=off,disable-modern=off` | `vfs: FAT32 mounted` |
| `virtio-small-queue` | `queue-size=4` | `virtio-blk: … queue size 4 (max 4), 2 data pages/request` |
| `no-disk` | tanpa perangkat blok | `virtio-blk: no device present`, `vfs: no block device`, `[init] storage: none`, `[init] SKIP A01` |
| `no-vga` | `-vga none` | `framebuffer: none usable; serial console only` |
| `vmware-vga` | `-vga vmware` | `framebuffer:` |

Dua mesin ini punya gigi yang terbukti: dengan driver virtio sebelum perbaikan
ukuran antrean, `virtio-small-queue` gagal (`read /spaceos/model.slm: bad address`,
3 uji merah); `no-disk` gagal sebelum `init` bisa membedakan "tidak ada disk" dari
"pembacaan gagal".

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
| K02 | 37 uji negatif ABI: buffer kosong beralamat 0, tabel handle penuh lalu pulih, nomor syscall salah, handle salah, jenis objek salah, pointer buruk, hak dipersempit tidak bisa diperluas, transfer endpoint sesama channel ditolak tanpa kehilangan handle, pesan terlalu besar tetap di antrean, peer tertutup, double close | `bin/abi_negative` |
| K02 | IPC echo 3 pesan + transfer handle + `PeerClosed` mengakhiri layanan | `bin/ipc_echo` |
| K02 | `sleep(50 ms)` memajukan `ticks` | — |
| K02 | proses loop tanpa syscall tidak membuat init kelaparan; `kill` → `SIGNAL` | `bin/spin` |
| K03 | kuota 100 halaman: `mem_map` ditolak tepat pada batas, halaman nol, dilepas dan dapat dipakai lagi | `bin/quota` |
| K03 | 50 siklus spawn/IPC/exit → frame bebas dan heap kernel identik | `bin/worker` |
| K03 | 20 siklus "kill saat blocking di `recv`" → peer melihat `PeerClosed`, frame bebas dan heap kernel identik | `bin/ipc_echo` |
| K03 | 20 siklus kill saat `sleep(1 jam)`, 20 siklus kill saat blocking `recv` dengan peer tetap terbuka, 20 siklus kill saat `wait` pada proses yang terus berjalan → frame bebas dan heap kernel identik (tanpa perbaikan: ~180 frame dan ~12 KiB heap bocor per 20 siklus) | `bin/blocker` |
| K02 | tabel handle penuh → `spawn` ditolak `TooManyHandles` dan tidak ada proses yatim | `bin/hello` |
| A01 | tiga model rusak (`badmagic`, `baddims`, `trunc`) ditolak dengan alasan, tanpa crash | `bin/spaceai` |
| A01 | model diverifikasi SHA-256 terhadap manifest saat dimuat ke buffer compute | `bin/spaceai` |
| A01 | 128 token dihasilkan offline lewat Compute ABI dan **identik** dengan baseline host; TTFT, token/detik, working set dan RSS dilaporkan | `bin/spaceai` |
| D01 | SHA-256 guest cocok dengan vektor FIPS (kosong, "abc", sejuta 'a') | `bin/init` |
| D01 | `/spaceos/model.slm` dibaca dari disk guest; ukuran dan SHA-256 cocok dengan `/spaceos/manifest.txt` yang dibuat host | `bin/init` |
| C01 | negosiasi versi (permintaan sebelum `HELLO` → `Denied`, versi salah → `Invalid`), device query, antrean habis → `NoMemory`, submit pada antrean tak dikenal → `BadHandle` | `bin/spacecompute` |
| C01 | buffer bersama: init menulis, layanan membaca; `ADD`, `MATMUL`, `ARGMAX` cocok dengan referensi yang dihitung di init | `bin/spacecompute` |
| C01 | batas: buffer tak dikenal → `BadHandle`, irisan melewati akhir → `Invalid`, irisan terlalu kecil → `MsgSize`, operasi tak dikenal → `NoSys`, tiket tak dikenal → `BadHandle` | `bin/spacecompute` |
| C01 | operasi panjang: `WAIT` singkat → `WouldBlock`+`RUNNING`, `WAIT` lagi → selesai; setelah `CANCEL` → `CANCELLED`, lalu tiket dilupakan | `bin/spacecompute` |
| C01 | tiga siklus layanan (termasuk satu yang sengaja tidak melepas buffer) → frame bebas dan heap kernel identik | `bin/spacecompute` |
| C01 | menulis lewat pemetaan memory object read-only → proses dibunuh `PAGE_FAULT` | `bin/fault` |
| C01 | memory object yang masih dipetakan bertahan setelah handle terakhirnya ditutup: pola ditulis, 1 MiB dialokasikan lalu dilepas untuk mendaur ulang frame, isi pemetaan tetap utuh (tanpa perbaikan: `mapping corrupted at byte 0: 0xaa`) | `bin/init` |
| C01 | dimensi nol pada `SOFTMAX`/`ARGMAX`/`FILL`/`RMSNORM`/`EMBED` → `Invalid` saat submit (tanpa perbaikan: layanan panik saat membaca elemen 0) | `bin/spacecompute` |
| C01 | `BUFFER_RELEASE` atas buffer yang masih dirujuk tiket tertunda → `WouldBlock`; setelah `CANCEL` buffer boleh dilepas | `bin/spacecompute` |
| C01 | 512 permintaan yang masing-masing menyertakan handle transfer → layanan menutupnya, tabel handle tidak habis | `bin/spacecompute` |
| D01 | berkas tidak ada → `NotFound`; menelusuri **melewati** berkas biasa (`/spaceos/manifest.txt/anything`) → `NotFound`; `fs_open` tanpa hak `FS` → `Denied`; baca ke alamat kernel → `Fault`; baca melewati akhir berkas → 0 byte; `fs_stat` pada handle channel → `Denied` | `bin/init` |

### Uji U01 (sesi dan supervisi)

| ID | Uji | Program |
|---|---|---|
| U01 | sesi bertahan melewati empat worker berturut-turut: `crash` (dibunuh `PAGE_FAULT`), `hang` (macet tanpa syscall), `slow` (tidur), `ok` (selesai normal). Setiap kali sesi harus menjawab `STATUS` dan membuka daftar berkas; `STOP` menghentikan worker yang macet maupun yang tidur; `STOP` tanpa worker → `NotFound` | `bin/spaceshell`, `bin/uiworker` |
| U01 | sesi tanpa hak `FS` → daftar berkas `Denied` tetapi job tetap jalan; perintah tak dikenal → `NoSys`; sesi tidak bisa memperluas kapabilitas yang diberikan | `bin/spaceshell` |
| U01 | `fs_list` pada volume guest: `/` memuat `SPACEOS/`, ukuran `MODEL.SLM` cocok dengan `fs_stat`, melist berkas (bukan direktori) → `Invalid`, tanpa hak `FS` → `Denied` | `bin/init` |

Gigi uji ini terbukti: dengan `poll_worker` memakai `wait` yang memblokir (bukan
`wait_nonblocking`), job `hang` mengunci sesi dan skenario acceptance mati karena
time-out, bukan gagal dengan rapi.

Kedua skenario `terminal` menutup sisi lain U01: perintah datang dari **ketikan**,
bukan dari channel kontrol. Boot yang sama membuktikan bahwa `stop` yang diketik
orang menghentikan worker yang tidak pernah memanggil kernel lagi, dan sesi tetap
menjawab `status` sesudahnya — sekali lewat keyboard, sekali lewat serial.

Catatan harness: masukan **tidak** boleh dialirkan lewat chardev socket pada port
serial kedua. OVMF memakai setiap port serial yang ditemukannya sebagai konsol,
dan backpressure chardev socket membuat tulisan konsol firmware gagal sehingga
bootloader panik berulang sebelum kernel sempat jalan. Yang dipakai karena itu
monitor QEMU (untuk keyboard) dan chardev **pty** (untuk COM2); keduanya tidak
mengganggu konsol firmware.

### Uji G01 (agent dan Tool Broker)

| ID | Uji | Program |
|---|---|---|
| G01 | agent tanpa kapabilitas apa pun selain satu channel: `fs_open` miliknya ditolak kernel, lalu ia membaca `task.txt`/`input.txt`, menambal, menulis `output.txt` ke overlay, membacanya kembali, dan check `verify` lulus terhadap `expect.txt` buatan host | `bin/spacebroker`, `bin/spaceagent` |
| G01 | audit operator mencatat pembacaan yang diizinkan, pembacaan di luar scope, percobaan keluar lewat `..`, akses agent ke audit, dan check yang lulus; jumlah penolakan yang dilaporkan broker sama dengan isi audit | `bin/spacebroker` |
| G01 | delapan bentuk jalan keluar ditolak `denied-scope`: `/spaceos/manifest.txt`, `..`, subdirektori, `//`, awalan mirip (`/spaceos/wsx`), path relatif, `/`, `/spaceos`. Tool operator (`AUDIT`, `ATTACH`, `QUIT`) dan tool tak dikenal ditolak `denied-tool` dari sisi agent. Berkas hilang dan check tak dikenal → `failed` (bukan `denied`). Pesan cacat dijawab `MsgSize` dan sesi berlanjut | `bin/spacebroker` |
| G01 | broker bertahan saat agent menghilang tanpa `DONE` (channel tertutup) dan masih melayani audit serta quit | `bin/spacebroker` |

Gigi uji ini terbukti: dengan pemeriksaan scope naif (`path.starts_with(SCOPE)`),
`/spaceos/ws/../stolen.txt` lolos sebagai `allowed` dan dua uji G01 merah.

### Uji L01–L03 (SpaceLink)

| ID | Uji | Program |
|---|---|---|
| L01 | korpus `/spaceos/docs` (4 dokumen) diindeks jadi 7 chunk; kueri `channel` menempatkan `IPC.TXT` di puncak; **setiap** hasil diverifikasi provenance-nya — init membaca ulang rentang byte yang disebut layanan dan menghitung SHA-256-nya sendiri; hasil terurut menurun menurut skor; kueri melewati hasil terakhir → `NotFound` | `bin/spacelink` |
| L02 | `SECRET.TXT` dicabut: kueri `embargo` (kata yang hanya ada di dokumen itu) → `NotFound`; kueri umum `channel`/`quota` tidak lagi memuatnya; bundle tidak memuatnya; **indeks ulang tidak menghidupkannya kembali** dan statistik menunjukkan 3 dokumen hidup, 1 dicabut | `bin/spacelink` |
| L03 | bundle untuk `channel quota` dengan anggaran 400 byte: muat anggaran, setiap entri diverifikasi provenance-nya, jumlah panjang entri sama dengan byte yang dilaporkan, dan digest bundle = SHA-256 atas rangkaian digest entri (dihitung ulang oleh init); kueri yang sama menghasilkan bundle identik; anggaran 1 byte → bundle kosong tanpa error; setelah revokasi digest berubah | `bin/spacelink` |

Gigi uji ini terbukti: bila daftar revokasi tidak dipisahkan dari indeks (sehingga
`INDEX` ulang membaca kembali dokumen yang dicabut), L02 merah pada langkah
"query after re-index".

### Uji P01 (paket dan rollback)

| ID | Uji | Program |
|---|---|---|
| P01 | HMAC-SHA256 dicocokkan dengan **vektor RFC 4231** (kasus 1, 2, 3 dan kunci lebih panjang dari blok) di dalam guest, bukan sekadar "host dan guest sepakat"; perbandingan MAC constant-time diuji menerima yang sama dan menolak yang berbeda satu bit | `bin/init` |
| P01 | dua versi paket dipasang berurutan (v1 → v2, `previous` = 1); payload yang dipasang dibaca kembali dan cocok dengan digest yang dibawa paket | `bin/spacepkg` |
| P01 | empat paket ditolak masing-masing dengan alasannya: satu byte payload dibalik → `Payload`, ditandatangani kunci lain → `Mac`, dipotong setelah header → `Truncated`, bukan paket sama sekali → `Format`. Setelah keempatnya, versi aktif tetap 2 dan riwayat tetap 2 — penolakan tidak mengubah apa pun. Berkas yang tidak ada → `NotFound`, bukan penolakan paket | `bin/spacepkg` |
| P01 | `VERIFY` memeriksa tanpa memasang; memasang versi yang tidak lebih baru ditolak `Invalid`; `ROLLBACK` mengembalikan payload versi 1 **byte demi byte** (bukan membaca ulang berkas), `ROLLBACK` kedua → `NotFound` tanpa mengubah versi aktif, dan setelah rollback versi 2 bisa dipasang lagi | `bin/spacepkg` |

Fixture paket itu sendiri adalah giginya: keempatnya dibuat host dengan implementasi
yang sama (`spaceabi::pkg`) dan masing-masing berbeda dari paket yang sah dalam
tepat satu hal, sehingga verifier yang melewatkan satu pemeriksaan akan menerima
salah satunya.

## Selftest kernel (sebelum user-space)

`kernel/src/selftest.rs`: heap alokasi/bebas tanpa selisih; 64 frame berbeda dan kembali penuh; map/write/translate/unmap halaman kernel; address space user map/cek-akses/tolak-overlap/unmap/drop tanpa selisih frame; pemetaan memory object bersama menahan frame selama masih terpetakan dan mengembalikannya tepat saat pemetaan terakhir hilang; dekoder scan code diberi urutan sungguhan (tombol biasa, shift, **dua** shift ditekan lalu satu dilepas, tombol panah, tombol panah dengan shift palsu, keypad Enter dan `/`, backspace) dan hasilnya dicocokkan byte demi byte.

## Soak K01

`cargo xtask soak --boots 100` menjalankan 100 cold boot skenario acceptance, berhenti pada kegagalan pertama, dan menulis `build/logs/soak/summary.txt` + log per boot. Hasil sesi ini ada di `docs/evidence/soak-summary.txt`.

## Bukti

`docs/evidence/` berisi log serial dari sesi verifikasi (dibersihkan dari escape ANSI OVMF). Bukti harus diperbarui bila perilaku berubah; CI menjalankan `cargo xtask ci` pada setiap push/PR dan soak 100 boot secara manual/terjadwal (`.github/workflows/ci.yml`).
