# Pengujian

Semua uji berjalan **di dalam guest** (kernel + user-space Space OS); host hanya menjalankan QEMU dan membaca log serial serta kode keluar. Tidak ada uji yang bergantung pada Linux di dalam guest.

## Skenario `cargo xtask test`

| Skenario | cmdline | Harapan |
|---|---|---|
| `acceptance` | `init=bin/init` | marker `[kernel] selftest: heap ok`, `[init] Space OS init running`, `[init] ALL TESTS PASSED`; exit 33. `init=` bawaan diucapkan agar uji K02 bisa membaca command line kernel kembali dan memeriksa programnya |
| `stress` | `stress=2` | versi pendek uji stabilitas (ADR-0019): seluruh suite **dua kali dalam satu boot**, putaran chaos dengan pembunuhan acak setelah setiap putaran (termasuk `spacenet` di tengah transfer, lalu `network back`), dan memori setelah putaran 2 **sama persis** dengan setelah putaran 1 (`[stress] pass 2: ok`, tidak ada `LEAK`); exit 33 |
| `panic-diagnosis` | `selftest=panic` | `!!! KERNEL PANIC !!!`, pesan, `backtrace (frame pointers):`, `spacekernel: halted after panic`; exit 127 |
| `kernel-fault-diagnosis` | `selftest=kfault` | `!!! CPU EXCEPTION IN KERNEL MODE: page fault !!!`, `cr2=0xfffff000dead0000`, lalu panic; exit 127 |
| `kernel-stack-overflow-diagnosis` | `selftest=stack` | `!!! CPU EXCEPTION IN KERNEL MODE: double fault !!!` (guard page kernel stack), lalu panic; exit 127 |
| `storage-reboot` | — | image yang sama di-boot dua kali; kedua boot harus memuat virtio-blk, mount FAT32, dan lulus D01 (checksum model) |
| `init-exit-diagnosis` | `init=bin/hello` | proses pertama yang **selesai tanpa meminta shutdown** tidak meninggalkan apa pun untuk dijadwalkan. Kernel harus mengatakannya (`ended without requesting shutdown; nothing left to run`) dan berhenti dengan exit 35 — bukan menggantung seperti hang |
| `init-missing-diagnosis` | `init=bin/not_a_program` | `init=` yang salah ketik harus menyebut program yang benar-benar gagal: `cannot start "bin/not_a_program" from initrd: not found`, lalu panic; exit 127 |
| `terminal` | `init=bin/spaceterm` | image yang **sama**, di-boot ke sesi interaktif dan dikendalikan dari **keyboard**: harness menekan tombol lewat monitor QEMU (`sendkey`), jadi jalurnya scan code → IRQ 1 → decoder kernel. Mesin ini tidak punya COM2 sama sekali (log wajib memuat `no COM2 UART`), jadi tiap ketikan pasti datang dari keyboard. Diketik `help`, `status`, `ls /spaceos`, `run hang`, `status`, `stop`, `status`, `quit`; exit 33 |
| `terminal-serial` | `init=bin/spaceterm` | sesi yang sama lewat **konsol serial**: COM2 sebagai pty, harness menulis byte ke sana (IRQ 3). Log wajib memuat `keyboard (IRQ1) and COM2 serial (IRQ3)`. Perintah dan harapan sama dengan `terminal` |
| `desktop` | `init=bin/spacedesk` | image yang sama di-boot ke **desktop** (ADR-0020) dan dikendalikan dari keyboard lewat monitor QEMU: Super+Enter membuka terminal dan `ls /spaceos` diketik; Super+E membuka file manager dan panah menelusurinya; Super+A membuka Agent Center, `2` menjalankan worker yang crash (`crashed (page fault)`), Alt+Tab ke terminal dan `status` diketik **setelah** crash harus dijawab; `3` menjalankan worker yang macet dan `s` (Stop) harus menghentikannya; `5` menjalankan **model sungguhan** sampai token pertama, `s` harus menghentikannya di antara dua langkah (`[ai] stopped on request`, `[shell] Stop: worker 'infer' ended between two steps`), dan terminal harus menjawab `ls /spaceos/docs` sesudahnya; lalu ubah ukuran, pindah, minimize, workspace 2 dan kembali, kontras tinggi, Alt+F4; Super+Space membuka **Command Center**, `channel` + Enter harus menempatkan `IPC.TXT` teratas, Ctrl+B menyusun context bundle, Ctrl+O membuka file manager dengan `IPC.TXT` terpilih; dan Ctrl+Alt+Delete mematikan mesin. Log wajib memuat `display: leased to pid 1 'bin/spacedesk'` dan setiap langkah itu; screenshot di `build/logs/desktop-*.png`; exit 33 |

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
| `e1000-only` | kartu jaringan e1000 saja (tanpa virtio-net) | `virtio-net: no device present`, `[init] network: none; network tests will be skipped`, `[init] SKIP NET` |

Sumber entropi ikut bervariasi: `i440fx` tidak punya virtio-rng maupun RDRAND (`qemu64`), jadi
harus mengatakan `[kernel] entropy: none` dan melewati uji TLS; `cpu-max` tidak punya virtio-rng
tetapi punya RDRAND, dan harus lulus uji TLS dengan kunci dari situ; `no-disk` melewati uji TLS
karena sertifikat otoritas lab ada di disk; `virtio-transitional` memakai virtio-rng transisional.

Semua mesin lain membawa kartu virtio-net modern di jaringan lab (lihat di bawah);
`virtio-transitional` memakai kartu transisional dan harus tetap lulus ICMP echo.

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
| K02 | instruksi SSE (`pxor`) → `INVALID_OPCODE`, instruksi x87 (`fld1`) → `NO_FPU`: unit FPU/vektor dimatikan karena kernel tidak menyimpan state-nya per thread, jadi register tidak pernah dibagi antarproses (ADR-0017) | `bin/fault` |
| K02 | TF disetel lalu `syscall`: `#DB` mendarat di ring 0 → kernel selamat, proses dimatikan `DEBUG` | `bin/fault` |
| K02 | `syscall` dengan `rsp` non-kanonik dan dengan `rsp` = alamat kernel → kembali normal, proses keluar 0 | `bin/fault` |
| K02 | ELF rusak dari initrd (`fixtures/bad_entry`, `bad_magic`, `truncated`, `huge_segment`) → `NoExec`/`Quota`, tanpa crash | fixture dibuat `xtask` dari `hello` |
| K02 | pointer kernel ke syscall → `Fault`, tidak didereferensi | `bin/fault` |
| K02 | 49 uji negatif ABI: buffer kosong beralamat 0, tabel handle penuh lalu pulih, nomor syscall salah dan nomor tepat di luar tabel (`nr::COUNT`, ikut bergerak saat syscall ditambah), handle salah, jenis objek salah, pointer buruk, hak dipersempit tidak bisa diperluas, transfer endpoint sesama channel ditolak tanpa kehilangan handle, pesan terlalu besar tetap di antrean, peer tertutup, double close | `bin/abi_negative` |
| K02 | IPC echo 3 pesan + transfer handle + `PeerClosed` mengakhiri layanan | `bin/ipc_echo` |
| K02 | `sleep(50 ms)` memajukan `ticks` | — |
| K02 | proses loop tanpa syscall tidak membuat init kelaparan; `kill` → `SIGNAL` | `bin/spin` |
| K03 | kuota 100 halaman: `mem_map` ditolak tepat pada batas, halaman nol, dilepas dan dapat dipakai lagi | `bin/quota` |
| K03 | 50 siklus spawn/IPC/exit → frame bebas dan heap kernel identik | `bin/worker` |
| K03 | 50 siklus map/unmap 2 MiB (`mem_map` dan memory object) → frame bebas identik: rentang alamat yang dilepas dipakai lagi beserta page table-nya (tanpa perbaikan ADR-0019: `frames 2090101 -> 2090001`, 100 frame dalam 50 siklus) | `bin/init` |
| K03 | 20 siklus "kill saat blocking di `recv`" → peer melihat `PeerClosed`, frame bebas dan heap kernel identik | `bin/ipc_echo` |
| K03 | 20 siklus kill saat `sleep(1 jam)`, 20 siklus kill saat blocking `recv` dengan peer tetap terbuka, 20 siklus kill saat `wait` pada proses yang terus berjalan → frame bebas dan heap kernel identik (tanpa perbaikan: ~180 frame dan ~12 KiB heap bocor per 20 siklus) | `bin/blocker` |
| K02 | tabel handle penuh → `spawn` ditolak `TooManyHandles` dan tidak ada proses yatim | `bin/hello` |
| K02 | `wait_any` pada channel yang diam habis waktunya **setelah** batasnya, bukan sebelum (40–1000 ms untuk 100 ms); indeks handle yang siap dilaporkan, dan batas waktu 0 hanya melihat | `bin/init` |
| K02 | `wait_any` dibangunkan balasan proses lain, oleh proses yang keluar, dan oleh peer yang menutup channel | `bin/ipc_echo`, `bin/hello` |
| K02 | set cacat ditolak: kosong → `Invalid`, `WAIT_MAX + 1` → `Invalid`, handle tertutup → `BadHandle`, handle root dan channel tanpa `RECV` → `Denied`, pointer kernel → `Fault` | `bin/init` |
| K03 | 20 siklus kill saat `wait_any` pada dua antrean tanpa batas waktu, dan 20 siklus dengan batas waktu satu jam → frame bebas dan heap kernel identik, entri timer dilepas saat itu juga | `bin/blocker` |
| K02 | jam dinding (`SYS_CLOCK_REALTIME`, RTC CMOS) jatuh di antara 2024 dan 2100, dan `sleep(50 ms)` memajukannya 50–1000 ms | `bin/init` |
| K02 | `SYS_CMDLINE`: command line kernel terbaca utuh; buffer 1 byte mendapat awalnya dan tetap tahu panjang aslinya; alamat kernel → `Fault`; handle root tanpa `STATS` → `Denied`; program yang disebut `init=` adalah yang berjalan | `bin/init` |
| K02 | `SYS_RANDOM`: 257 byte → `Invalid`, alamat kernel → `Fault`, 0 byte → `Ok(0)`, 16 byte → `Ok(16)` bila ada sumber entropi dan `NotFound` bila tidak (tidak pernah cadangan yang lemah) | `bin/init` |
| K02 | 16 tarikan × 256 byte dari virtio-rng/RDRAND: tidak ada yang identik, chi-square byte ≤ 400 (255 derajat kebebasan), jumlah bit 1 dalam ±600 dari setengah; dilewati tanpa sumber entropi | `bin/init` |
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
| D01 | berkas ditulis dalam tiga bentuk (di dalam satu cluster, melewati batas cluster sehingga harus mengalokasi, lalu ditambal di tengah) lalu dibaca ulang **byte demi byte**; ukuran dari `fs_stat` harus cocok | `bin/init` |
| D01 | menulis adalah hak tersendiri: `FS` tanpa `FS_WRITE` → `Denied`, handle dari `fs_open` → `Denied`, nama di luar 8.3 → `Invalid`, direktori → `Invalid` | `bin/init` |
| D01 | satu penghitung dibaca lalu ditulis satu lebih tinggi tiap boot; `storage-reboot` menuntut boot **kedua** menemukan angka yang ditinggalkan boot pertama (bidang `final_boot_markers` pada skenario) | `bin/init` |

### Uji U01 (sesi dan supervisi)

| ID | Uji | Program |
|---|---|---|
| U01 | sesi bertahan melewati empat worker berturut-turut: `crash` (dibunuh `PAGE_FAULT`), `hang` (macet tanpa syscall), `slow` (tidur), `ok` (selesai normal). Setiap kali sesi harus menjawab `STATUS` dan membuka daftar berkas; `STOP` menghentikan worker yang macet maupun yang tidur; `STOP` tanpa worker → `NotFound` | `bin/spaceshell`, `bin/uiworker` |
| U01 | sesi tanpa hak `FS` → daftar berkas `Denied` tetapi job tetap jalan; job `infer` → `Denied` (sesi tidak bisa memberi worker akses baca yang tidak dimilikinya); perintah tak dikenal → `NoSys`; sesi tidak bisa memperluas kapabilitas yang diberikan | `bin/spaceshell` |
| U01 | `fs_list` pada volume guest: `/` memuat `SPACEOS/`, ukuran `MODEL.SLM` cocok dengan `fs_stat`, melist berkas (bukan direktori) → `Invalid`, tanpa hak `FS` → `Denied` | `bin/init` |
| U01 | `console_read` tanpa hak `CONSOLE` → `Denied`, ke memori kernel → `Fault`, dan tidak pernah memblokir | `bin/init` |
| U01 | satu tombol ditekan oleh controller sendiri (`debug_op::PS2_INJECT`, perintah 8042 0xD2): byte itu harus melewati IRQ 1, pengurasan controller, decoder, ring, lalu sampai ke `console_read` sebagai `a` | `bin/init` |
| U01 | ring masukan diluapkan dengan sengaja (`debug_op::CONSOLE_FLOOD`, 264 byte ke ring 256 byte): pembacaan berikutnya → `DataLoss` **sebelum** byte apa pun, laporan itu tidak memakan byte, dan byte yang selamat masih berupa potongan pola yang berurutan | `bin/init` |
| U01 | desktop mengambil layar: ukurannya bukan 0×0, lease kedua → `Busy`, handle root tanpa `DISPLAY` → `Denied`; desktop keluar dengan kode 0 dan layar bisa di-lease lagi | `bin/spacedesk`, `bin/init` |
| U01 | jendela dari keyboard (lewat API operator, penangan yang sama dengan tombol sungguhan): dua jendela, fokus di yang terbaru; Alt+Tab memindah fokus; Alt+→ menggeser 32 px dan Alt+Shift+↓ menambah tinggi 32 px (aplikasi menggambar ulang di buffer baru); Super+M meminimize dan fokus pindah; Ctrl+Alt+→ ke workspace kosong dan Ctrl+Alt+← kembali; jendela yang di-minimize kembali saat dipilih; Alt+F4 menutup file manager dan jendelanya hilang | `bin/spacedesk`, `bin/deskapps` |
| U01 | **inti U01 di desktop**: terminal, file manager dan Agent Center terbuka; worker dibuat crash dari Agent Center (`crashed (page fault)`); terminal menjawab `status` yang diketik setelahnya; file manager masih menelusuri volume; worker yang macet dihentikan Stop dalam ≤ 2 detik (PRD §9); tiga jendela masih ada dan desktop terus menampilkan frame baru | `bin/spacedesk`, `bin/deskapps`, `bin/spaceshell`, `bin/uiworker` |
| U01 | desktop yang **dibunuh** dengan jendela terbuka: aplikasinya pergi sendiri, layar kembali ke konsol, dan desktop baru bisa mulai | `bin/spacedesk`, `bin/init` |
| U01 | **PRD §9, skenario ujung ke ujung pertama, lewat sesi** (ADR-0021): job `infer` menulis 128/128 token yang semuanya sama dengan baseline dan melaporkan TTFT; dijalankan lagi, setelah ≥ 8 token Stop harus berhenti **di antara dua operasi compute** (`cooperative`, bukan dibunuh), dalam ≤ 2 detik, dengan token yang ditulis = token yang cocok; sesi lalu menjawab `status` dan membuka `/spaceos`; jumlah proses sebelum dan sesudah sesi sama (layanan compute ikut dituai) | `bin/spaceshell`, `bin/spaceai`, `bin/spacecompute` |
| U01 | skenario yang sama **di desktop**: Agent Center, tombol `5`, tunggu ≥ 8 token, `S`; Agent Center harus berkata `stopped between two steps` dalam ≤ 2 detik dengan token = cocok, lalu terminal menjawab `status` | `bin/spacedesk`, `bin/deskapps`, `bin/spaceai` |
| L01 | **Command Center** (ADR-0022): ketik `channel`, Enter → `/spaceos/docs/IPC.TXT` teratas; path, rentang byte dan digest yang **ditampilkan** dicocokkan dengan SHA-256 byte yang dibaca uji sendiri dari disk; Ctrl+B → context bundle; Ctrl+O → file manager baru dengan `IPC.TXT` terpilih | `bin/spacedesk`, `bin/deskapps`, `bin/spacelink` |

Gigi uji desktop (ADR-0020), masing-masing dijalankan sendiri: lease yang tidak eksklusif →
`a second lease on the screen was granted`; kernel yang tidak mengambil layar kembali → keempat uji
desktop gagal di `hello: resource busy` (lease yang diambil `init` untuk memeriksa layar tidak
pernah kembali); Stop yang tidak melakukan apa pun → `window 3 never said "'hang' stopped"`;
tombol yang dikirim ke jendela pertama, bukan ke yang fokus → `window 3 never said "crashed (page
fault)"; last: "agent: no worker has run; …"`.

Gigi uji tulis terbukti: dengan `virtio_blk::write_sectors` diubah menjadi no-op yang
melaporkan sukses, uji byte-demi-byte gagal dengan `open: not found` — berkas yang
"berhasil dibuat" tidak pernah ada di disk.

Gigi uji ini terbukti: dengan `poll_worker` memakai `wait` yang memblokir (bukan
`wait_nonblocking`), job `hang` mengunci sesi dan skenario acceptance mati karena
time-out, bukan gagal dengan rapi. Uji tombol juga: dengan satu byte sengaja
ditinggalkan di buffer keluaran 8042 sebelum IRQ 1 dibuka — persis serah terima
firmware yang `ps2::init()` cegah — uji itu gagal dengan `no key press arrived from
the PS/2 controller`, dan keyboard memang tuli sepanjang boot. Uji kehilangan masukan juga: dengan
`sys_console_read` kembali menyerahkan byte tanpa melaporkan yang hilang, uji itu
gagal dengan `read after an overflow gave Ok(32), expected DataLoss`.

Kedua skenario `terminal` menutup sisi lain U01: perintah datang dari **ketikan**,
bukan dari channel kontrol. Boot yang sama membuktikan bahwa `stop` yang diketik
orang menghentikan worker yang tidak pernah memanggil kernel lagi, dan sesi tetap
menjawab `status` sesudahnya — sekali lewat keyboard, sekali lewat serial. Jawaban
`status` itu sendiri ikut dituntut (`worker idle`, `worker running (hang)`,
`worker stopped (hang), last exit code -1`), bukan hanya baris yang dicat sesi
setelah setiap perintah.

Perintah pertama diketik dengan **koreksi di dalamnya**: `helpp` lalu backspace.
Kalau penghapusannya tidak bekerja, perintahnya bukan `help` dan jawabannya tidak
muncul — jadi jalur backspace (decoder kernel, editor baris sesi, dan penghapusan sel
di konsol framebuffer) diuji oleh perintah biasa, bukan oleh uji yang berdiri sendiri.

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
| G01 | tambalan mendarat di volume, bukan di memori broker: setelah broker keluar, `init` membuka `/spaceos/ws/output.txt` dengan handle-nya sendiri dan isinya harus sama dengan berkas harapan | `bin/init`, `bin/spacebroker` |
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
| L02 | revokasi hidup lebih lama daripada prosesnya: layanan pertama mencabut lalu di-`QUIT`, layanan **kedua** mengindeks korpus dari nol tanpa diberi tahu apa pun, dan dokumen itu tetap hilang | `bin/init`, `bin/spacelink` |
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
| P01 | instalasi hidup lebih lama daripada prosesnya: layanan pertama memasang lalu di-`QUIT`, layanan **kedua** dijalankan tanpa diberi tahu apa pun, dan nama, versi, digest serta payload-nya harus sama — satu-satunya penghubung keduanya adalah volume | `bin/init`, `bin/spacepkg` |

Fixture paket itu sendiri adalah giginya: keempatnya dibuat host dengan implementasi
yang sama (`spaceabi::pkg`) dan masing-masing berbeda dari paket yang sah dalam
tepat satu hal, sehingga verifier yang melewatkan satu pemeriksaan akan menerima
salah satunya.

### Uji NET (jaringan, ADR-0016)

Semua dilewati (bukan digagalkan) pada mesin tanpa kartu virtio-net.

| ID | Uji | Program |
|---|---|---|
| NET | lease butuh hak `NET` dan eksklusif (`Busy` untuk pemegang kedua); duplikat berbagi lease, dan lease lepas hanya setelah handle terakhirnya ditutup | `bin/init` |
| NET | perangkat melaporkan MAC unicast bukan nol, link up, MTU 1500, frame maksimum 1514 | `bin/init` |
| NET | ARP buatan tangan: gateway 10.0.2.2 menjawab dengan MAC-nya | `bin/init` |
| NET | ICMP echo buatan tangan ke gateway dengan muatan 32, 512 dan 1472 byte (frame 1514 byte penuh) kembali utuh, dengan checksum IPv4 dan ICMP diverifikasi | `bin/init` |
| NET | frame yang tiba **saat penerimanya tidur** membangunkannya: anak mengirim 150 ms setelah `init` tidur di `wait_any`, dan `init` harus bangun ≤ 100 ms setelah frame berangkat (terukur: tidur 151 ms, bangun 1 ms setelahnya) | `bin/blocker` |
| NET | frame 13 byte → `Invalid`, 1515 byte → `MsgSize`, frame atau buffer di alamat kernel → `Fault`, buffer terima < 1514 → `Invalid`, lease tanpa `WRITE`/`READ` → `Denied`, handle channel → `Denied` | `bin/init` |
| NET | `spacenet` dijalankan, diberi lease, dan mendapat 10.0.2.15/24 lewat DHCP (tanpa router dan DNS: jaringan lab tertutup); HELLO sesi melaporkan alamat, DNS lab, dan MAC perangkat yang sama | `bin/spacenet` |
| NET | DNS lewat TCP: `echo.lab.test` → 10.0.2.101, `alias.lab.test` (CNAME) → alamat yang sama, huruf besar-kecil tidak berpengaruh, `nosuch.lab.test` → `NotFound` | `bin/spacenet` |
| NET | 64 KiB pola tak berulang dikirim ke layanan echo sambil membaca, `shutdown`, lalu dibaca sampai EOF: setiap byte cocok (pola dihitung, tidak disimpan — heap `init` 128 KiB) | `bin/spacenet` |
| NET | nama di luar daftar, port lain dari nama yang diizinkan, dan **alamat** dari nama yang diizinkan ditolak `Denied`, begitu juga pencarian nama di luar daftar; 4 penolakan dihitung dan **0 frame** keluar menurut penghitung perangkat | `bin/spacenet` |
| NET | tujuan yang tidak pernah menjawab (10.0.2.77) → `TimedOut` setelah 400–3000 ms untuk batas 500 ms | `bin/spacenet` |
| NET | port yang tidak mendengarkan (alamat layanan echo, port 8) → `Refused`: QEMU menjawab SYN-nya dengan RST | `bin/spacenet` |
| NET | layanan yang pergi tanpa membaca → pembacaan berikutnya `Reset` | `bin/spacenet` |
| NET | koneksi ke character generator (`chargen.lab.test:19`, terus mengirim) dibaca 2 KiB lalu ditutup: data yang tiba sesudahnya dijawab RST, jadi layanan berhenti — harness menuntut log layanan `the guest went away` (RFC 1122 §4.2.2.13, ADR-0018) | `bin/spacenet` |
| NET | 20 koneksi (hubung, kirim `ping`, `shutdown`, baca sampai EOF) → frame bebas dan heap kernel identik | `bin/spacenet` |
| NET | `spacenet` dibunuh saat koneksi terbuka: pembacaan klien → `PeerClosed`, sesi → `PeerClosed`, lease bisa dibuka lagi; instans baru melayani satu koneksi dan `QUIT` keluar dengan kode 0, lalu lease kembali | `bin/spacenet` |

Gigi uji ini terbukti: dengan tick yang tidak lagi membangunkan penunggu kartu, uji
tidur gagal (`woken 2850 ms after the frame left`) padahal uji ARP dan ICMP tetap
hijau — jawaban QEMU sudah ada di ring sebelum penerimanya sempat tidur, jadi hanya
uji ini yang benar-benar melewati jalur bangun. Dengan satu byte setiap frame masuk
di atas 100 byte dibalik, uji ICMP gagal (`ICMP checksum does not verify`). Dengan
allowlist yang mengabaikan port, `echo.lab.test:8` lolos dari pemeriksaan (`connection
refused, expected Denied`); dengan RST yang dilaporkan sebagai akhir aliran, uji reset
gagal (`read gave Ok(0), expected Reset`).

### Uji TLS (ADR-0017)

Setiap kasus berjalan di proses `bin/tlsprobe` sendiri, dengan sesi `spacenet` yang hanya boleh
ke `tls.lab.test:<port>` dan handle root yang hanya bisa membaca berkas (sertifikat otoritas lab
ada di `/spaceos/tls/labca.der`). Dilewati tanpa kartu, tanpa sumber entropi, atau tanpa disk.

| ID | Uji | Layanan lab |
|---|---|---|
| TLS | TLS 1.3 dengan ChaCha20-Poly1305: 16 KiB pola tak berulang bolak-balik utuh, `close_notify` dua arah | 10.0.2.103:443 |
| TLS | sama, dengan AES-128-GCM (server hanya menawarkan suite itu) | :449 |
| TLS | sama, dengan AES-256-GCM | :450 |
| TLS | sertifikat kedaluwarsa kemarin → `expired` | :444 |
| TLS | sertifikat untuk `other.lab.test` → `wrong-name` | :445 |
| TLS | sertifikat yang ditandatangani sendiri → `unknown-issuer` | :446 |
| TLS | satu bit dibalik di record data pertama → `bad-record` | :447 |
| TLS | jawaban sebagian lalu tutup tanpa `close_notify` → `truncated` | :448 |

Layanan TLS lab memakai rustls + ring di host. Selain jawaban guest, skenario `acceptance`
menuntut **log server** (`build/logs/acceptance.lab.log`) berisi handshake dengan ketiga suite,
echo 16384 byte yang ditutup dengan `close_notify` dua arah, dan alert yang menamai setiap
penolakan: `CertificateExpired`, `BadCertificate`, `UnknownCA`, `BadRecordMac`. Gigi uji ini
ada di ADR-0017: verifier yang menerima semua sertifikat, jam yang dimajukan 31 hari, dan EOF yang
dianggap akhir bersih masing-masing membuat run gagal.

### Uji I01 (adapter cloud, ADR-0018)

`init` menjalankan `bin/spacebroker` dan `bin/spacecloud`, menyerahkan trust anchor, endpoint
`api.cloud.test:443`, sesi yang hanya boleh ke sana, channel broker, dan budget 50000 µ$ (harga
$3/$15 per juta token, 3 percobaan), lalu bertanya sebagai klien. Penyedianya **tiruan**
(`xtask lab cloud`, 10.0.2.100:443, Messages API lewat HTTP/1.1 + SSE di atas TLS 1.3); nama
model memilih skripnya.

| ID | Uji | Model tiruan |
|---|---|---|
| I01 | sebelum kredensial → `not-ready`; kunci salah → `auth` (401) dalam **1** percobaan; kunci dari `/spaceos/cred/cloud.key` → jawaban | `lab-echo` |
| I01 | jawaban tiba dalam 4 potong berjarak 150 ms: potongan pertama ≥ 150 ms (satu celah) sebelum akhir — jawaban yang ditahan tiba sekaligus (0 ms), sedangkan host yang sibuk bisa merapatkan dua potong (286 ms sekali di `cpu-max` yang berbagi CPU) tetapi tidak melipat tiga celah menjadi kurang dari satu; token × harga = biaya, dan belanja adapter naik tepat sebesar itu | `lab-echo` |
| I01 | model membaca `/spaceos/ws/input.txt` lewat broker (`allowed`) dan mengutip barisnya; tanpa alat ditawarkan, tidak ada panggilan | `lab-tool` |
| I01 | model minta `/spaceos/cred/cloud.key`: broker menolak (`denied-scope`), jawaban memuat penolakan dan **bukan** kuncinya | `lab-exfil` |
| I01 | tenggat 1500 ms atas jawaban yang macet → `timeout` dalam 1400–4000 ms, ask berikutnya dilayani | `lab-stall` |
| I01 | penyedia selalu 529 → `overloaded` setelah tepat 3 percobaan dan 3 koneksi | `lab-overloaded` |
| I01 | event bersarang 120 tingkat (JSON sah, di bawah batas parser 128) → `protocol` sebelum parser menyelaminya — stack program ini 64 KiB — dan ask berikutnya dilayani | `lab-hostile` |
| I01 | penyedia melaporkan 400 token output untuk `max_tokens` 100 → `over-budget`, diputus, ditagih tepat 400 token | `lab-runaway` |
| I01 | ask yang bisa berbiaya 60000 µ$ → `budget`, **0 frame** menurut penghitung perangkat, 0 koneksi, belanja tetap | (tidak pernah sampai) |
| I01 | ask local-only → `local-only`, **0 frame**, 0 koneksi | (tidak pernah sampai) |
| I01 | adapter keluar 0; audit broker memuat baca workspace `allowed` dan baca kredensial `denied-scope` | — |

Harness menuntut sisi penyedia juga (`lab_must_contain`): 401, `streamed 4 pieces`,
`tool_result … 119 bytes`, penolakan alat, klien yang pergi dari `lab-stall` dan `lab-runaway`, dan
`attempt 3 answered 529`; dan menolak run yang log lab-nya memuat (`lab_must_not_contain`)
`nobody stopped it`, `THE CREDENTIAL LEAKED`, `attempt 4 answered`, `lab-budget` atau `lab-local`.
Gigi setiap klaim ada di `docs/evidence/cloud-summary.txt`.

## Gerbang build: tidak ada instruksi FPU atau vektor

`cargo xtask build` mendekode setiap instruksi di segmen executable kernel dan semua program
(`xtask/src/nofpu.rs`, iced-x86) dan gagal bila ada instruksi x87, MMX, SSE, AVX, AES-NI,
PCLMUL, SHA, FXSR atau XSAVE, register vektor/FPU sebagai operand, atau byte yang bukan
instruksi. Unit-unit itu dimatikan kernel, jadi instruksi seperti itu di kernel berarti panic
dan di program berarti program mati. Pengecualiannya tepat dua instruksi yang disengaja di
`bin/fault` (`fld1`, `pxor`); pemeriksa terbukti menemukan keduanya.

## Gerbang build: kontras warna desktop

`spacedesk` dan `deskapps` memeriksa palet mereka saat dikompilasi (`gfx::contrast_x100`, rumus
WCAG): setiap warna teks terhadap setiap latar tempat ia digambar — 4,5:1 di tema biasa, 7:1 di
kontras tinggi — dan bingkai fokus serta tombol Stop 3:1 terhadap sekitarnya. Warna yang gagal
menghentikan build dengan nama pasangannya, misalnya `CONTRAST: on_accent on accent is too faint to
read` (warna lama: putih di atas kuning, 1,43:1) atau `the Stop label is too faint to read` (putih di
atas merah, 2,78:1).

## Jaringan lab dan pemeriksaan kabel

Kartu setiap mesin tersambung ke jaringan QEMU user-mode dengan `restrict=on`: guest
tidak bisa mencapai host maupun internet. Satu-satunya pintu adalah aturan
`guestfwd` yang menjalankan `xtask lab <nama>` untuk setiap koneksi, dengan koneksi
itu sebagai stdin/stdout (`xtask/src/lab.rs`): DNS lewat TCP di 10.0.2.53:53, echo di
10.0.2.101:7, character generator di 10.0.2.101:19, layanan yang me-reset koneksinya di
10.0.2.102:9, layanan TLS di 10.0.2.103 (lihat uji TLS), dan penyedia cloud tiruan di
10.0.2.100:443 (lihat uji I01). Port lain di alamat
lab dijawab QEMU dengan RST (uji `Refused`), dan tidak ada yang
menjawab di 10.0.2.77. Catatan layanan lab ditulis ke `build/logs/<skenario>.lab.log`.

Setiap boot juga direkam ke `build/logs/<skenario>.pcap` (QEMU `filter-dump`) dan
diperiksa harness (`xtask/src/pcap.rs`): **setiap FIN dari peer harus di-ACK oleh
guest**, kecuali koneksinya di-reset. Hasilnya dicetak pada baris PASS, misalnya
`tcp: 60 connections, 58 closed cleanly, 1 reset, 0 segment(s) the peer sent again`.
Gigi pemeriksaan ini terbukti pada bug sungguhan: build yang membuang socket saat
TIME-WAIT sebelum ACK tertundanya berangkat lulus semua uji di dalam guest, tetapi
rekamannya gagal (`FINs never acknowledged by the guest: …` untuk 24 koneksi, 48
segmen dikirim ulang peer). `cargo xtask tcpcheck <file.pcap>` memeriksa rekaman
yang dibuat sendiri, misalnya dengan `SPACEOS_PCAP=<file> cargo xtask run`.

## Uji unit host

`cargo xtask unit` (juga bagian `cargo xtask ci`) menjalankan uji unit `spaceabi` —
tata letak pesan jaringan tanpa padding implisit, pencocokan allowlist, codec DNS
(kueri, jawaban, rantai CNAME, pointer kompresi yang bermusuhan) — dan `xtask`
sendiri, termasuk server DNS lab, analisis pcap (port yang dipakai ulang adalah koneksi
baru, dan koneksi kedua yang mengabaikan FIN tetap tertangkap) dan pembaca baris putaran stress.

## Selftest kernel (sebelum user-space)

`kernel/src/selftest.rs`: heap alokasi/bebas tanpa selisih; 64 frame berbeda dan kembali penuh; map/write/translate/unmap halaman kernel; address space user map/cek-akses/tolak-overlap/unmap/drop tanpa selisih frame, pemetaan anonim dipisah halaman penjaga dan rentang yang dilepas diberikan lagi; pemetaan memory object bersama menahan frame selama masih terpetakan dan mengembalikannya tepat saat pemetaan terakhir hilang; dekoder scan code diberi urutan sungguhan (tombol biasa, shift, **dua** shift ditekan lalu satu dilepas, tombol panah, tombol panah dengan shift palsu, keypad Enter dan `/`, backspace) dan hasilnya dicocokkan byte demi byte.

## Uji stabilitas `cargo xtask stress` (ADR-0019)

`cargo xtask stress --minutes 480` mem-boot mesin lab satu kali dengan `stress=480m`. `init`
menjalankan seluruh suite penerimaan berulang-ulang selama waktu itu; setelah setiap putaran:

- **putaran chaos**: `bin/churn` dalam empat mode (dua penulis berkas, pemeta memori/memory object,
  pengoper channel dan handle, pemanggil spawn) plus transfer echo lewat `spacenet`, dibiarkan
  bekerja 100–1600 ms, lalu dibunuh dalam urutan acak dengan jeda acak 0–40 ms. Setiap korban harus
  mati karena dibunuh; koneksi yang layanannya mati harus melaporkannya; lease jaringan harus bisa
  diambil lagi dan `spacenet` baru harus melayani (`network back`);
- **pemantauan memori**: frame bebas, heap kernel, proses, thread, heap `init` dan handle `init`
  harus **sama persis** dengan setelah putaran 1 (menunggu hingga 2 detik untuk proses yang masih
  keluar). Setiap putaran berakhir dengan satu baris angka, termasuk frame bebas terendah dan heap
  kernel tertinggi sejak boot.

Harness berjalan dari salinannya sendiri (`build/stress/xtask`) dengan otoritas lab, disk data dan
variabel firmware sendiri, sehingga `cargo xtask test` boleh dijalankan di sebelahnya. Ia membaca log
secara bertahap, mencetak baris `[stress]` dengan cap waktu, mencatat deskriptor dan memori QEMU
serta layanan lab yang hidup setiap 60 detik, lalu menulis ke `build/stress/`:

| Berkas | Isi |
|---|---|
| `stress.log`, `stress.lab.log`, `stress.pcap` | log serial lengkap, log layanan lab, rekaman jaringan |
| `summary.txt` | catatan guest sendiri, putaran, memori setelah setiap putaran dan terendah/tertinggi selama run, TCP, lab, host, putusan |
| `memory.csv` | satu baris per putaran: lulus/total, detik, dan kesembilan angka memori |

Run gagal bila: kode keluar bukan 33, guest tidak menulis `STRESS PASSED`, log memuat `KERNEL PANIC`,
`[init] FAIL`, `LEAK` atau `[churn]`, layanan lab menulis salah satu baris terlarang (kredensial
bocor, percobaan keempat, aliran yang tidak dihentikan, ask yang seharusnya ditolak sampai ke
penyedia), angka memori putaran mana pun berbeda dari putaran 1, atau guest berjalan kurang dari
waktu yang diminta. FIN peer yang tidak pernah dijawab **dilaporkan tetapi tidak menggagalkan**:
koneksi milik `spacenet` yang dibunuh memang berakhir begitu; sopan-santun TCP dinilai ketat di
setiap skenario lain.

## Soak K01

`cargo xtask soak --boots 100` menjalankan 100 cold boot skenario acceptance, berhenti pada kegagalan pertama, dan menulis `build/logs/soak/summary.txt` + log per boot. Hasil sesi ini ada di `docs/evidence/soak-summary.txt`.

## Bukti

`docs/evidence/` berisi log serial dari sesi verifikasi (dibersihkan dari escape ANSI OVMF). Bukti harus diperbarui bila perilaku berubah; CI menjalankan `cargo xtask ci` pada setiap push/PR dan soak 100 boot secara manual/terjadwal (`.github/workflows/ci.yml`).
