# ADR-0020 — Desktop: layar sebagai lease, jendela sebagai memori klien, dan manajemen jendela dari keyboard

- Status: diterima
- Tanggal: 2026-09-30
- Konteks: PRD §6 "Desktop dan perangkat" (compositor dan toolkit untuk layanan grafis Space OS;
  MVP grafis dengan software rendering dan framebuffer; dock dan pengelola jendela dengan fokus,
  resize, minimize, perpindahan workspace dan navigasi keyboard; Agent Center dengan tombol Stop;
  file manager, terminal dan recovery tanpa AI atau jaringan; aksesibilitas dengan kontras, fokus
  yang terlihat, dan semantik kontrol untuk API otomasi); PRD §7 **U01** ("Desktop, terminal, file
  manager dan Stop dapat dipakai ketika worker inferensi crash"); PRD §3 (jalur recovery tetap ada
  ketika desktop gagal); ADR-0011 (layanan sesi `spaceshell`), ADR-0004 (kapabilitas)

## Konteks

Sampai sekarang "desktop" di U01 adalah konsol teks framebuffer yang digambar kernel. Itu cukup
untuk membuktikan bahwa sesi bertahan saat worker crash, tetapi bukan desktop: tidak ada jendela,
tidak ada pengelola jendela, dan kernel sendiri yang menggambar. Yang diputuskan di sini adalah
**siapa memegang layar**, **bagaimana sebuah program menunjukkan jendelanya tanpa bisa menyentuh
jendela lain**, dan **bagaimana semua itu diuji tanpa orang di depan mesin**.

## Keputusan

**1. Layar adalah lease, satu pemegang, dan kernel mengambilnya kembali.** Hak root baru
`DISPLAY` dan `SYS_DISPLAY_OPEN` menyerahkan layar kepada satu server tampilan dalam bentuk objek
memori atas halaman-halaman framebuffer (`MemoryKind::Display`): memori perangkat, tidak pernah
diserahkan ke alokator frame. Selama objek itu atau pemetaan mana pun darinya masih hidup, konsol
kernel hanya menulis ke port serial. Saat yang terakhir hilang — server keluar, crash, atau dibunuh
— kernel membersihkan layar, mengambilnya kembali, dan mengatakannya (`[kernel] display: returned
to the console`). Panic kernel **selalu** mengambil layar lebih dulu, sehingga pesan panic yang
terlihat, bukan desktop yang membeku. Lease kedua ditolak `Busy`; handle tanpa `DISPLAY` ditolak
`Denied`. `SYS_DISPLAY_INFO` melaporkan ukuran, stride, format (BGRX/RGBX) dan offset piksel (0,0).

**2. Keyboard sebagai event, bukan hanya byte.** Dekoder scan code kini juga menghasilkan event
(`SYS_INPUT_READ`, hak `CONSOLE` yang sama dengan `SYS_CONSOLE_READ`): tombol mana, ditekan atau
dilepas, dengan Shift/Ctrl/Alt/Super yang sedang ditahan, termasuk tombol extended (panah, Super,
Ctrl/Alt kanan, keypad). Tekanan dengan Ctrl, Alt atau Super **tidak mengetik apa pun**, baik di
event maupun di aliran byte terminal: pintasan tidak pernah bocor menjadi teks. Event yang hilang
karena ring penuh dilaporkan (`kind::LOST`) sebelum event yang selamat.

**3. Sebuah jendela adalah memori milik kliennya.** Klien membuat objek memori `w × h × 4` byte,
menggambar di sana, dan menyerahkan handle-nya bersama `CREATE`. Server (`bin/spacedesk`) memeriksa
bahwa itu memang objek memori sebesar yang diklaim, memetakannya **read-only**, dan menyusunnya
dengan yang lain. Klien tidak memegang apa pun yang menyentuh layar atau jendela lain; server tidak
pernah menulis ke memori klien. Resize adalah percakapan: server mengirim `CONFIGURE`, klien
menggambar ulang di buffer baru dan mengirim `RESIZED`; sampai itu tiba, bingkai berukuran baru
berisi buffer lama. Klien yang mati atau menutup channel-nya kehilangan jendelanya; klien yang tidak
pergi dalam 2 detik setelah diminta menutup, dihentikan. Setiap klien dilayani paling banyak 16
pesan per putaran, jadi yang membanjiri tidak membuat yang lain kelaparan.

**4. Semua manajemen jendela ada di keyboard** (PRD §6: "navigasi keyboard"):

| Tombol | Aksi |
|---|---|
| Alt+Tab / Alt+Shift+Tab | fokus ke jendela berikut/sebelumnya di workspace ini (yang di-minimize kembali saat dipilih) |
| Alt+panah | pindahkan jendela 32 px (bilah judul selalu tetap di layar) |
| Alt+Shift+panah | ubah ukuran 32 px; aplikasi menggambar ulang pada ukuran baru |
| Super+M / Alt+F9 | minimize |
| Alt+F4 / Super+Q | tutup |
| Ctrl+Alt+←/→ | workspace sebelumnya/berikut (empat) |
| Ctrl+Alt+Shift+←/→ | bawa jendela yang fokus ke workspace sebelah |
| Super+Enter, Super+E, Super+A, Super+Space (juga Alt+F1/F2/F3/F5) | terminal, file manager, Agent Center, Command Center (ADR-0022) |
| Super+H | kontras tinggi |
| Ctrl+Alt+Delete | matikan mesin (hanya bila desktop adalah sesi, `init=bin/spacedesk`) |

Fokus mengangkat jendela ke atas dan terlihat (bingkai aksen 2 px). Jendela baru ditempatkan di
posisi yang paling sedikit menutupi jendela lain.

**Kontras dipegang oleh build, bukan oleh mata** (PRD §6: "kontras, fokus yang terlihat"). Setiap
warna teks compositor diperiksa terhadap setiap latar tempat ia digambar — 4,5:1 (WCAG AA) di tema
biasa, 7:1 (AAA) di kontras tinggi — dan bingkai fokus 3:1 terhadap sekitarnya; aplikasi memegang
teksnya sendiri pada 4,5:1, termasuk label Stop. Pemeriksaannya `const` (`gfx::contrast_x100`),
jadi warna yang membuat sesuatu tidak terbaca tidak bisa dikompilasi. Pemeriksaan ini lahir dari
cacat nyata: screenshot kontras tinggi pertama menunjukkan angka workspace dan tile dock putih di atas
kuning (1,43:1), dan label Stop putih di atas merah (2,78:1). Status yang ditunjukkan warna juga
dikatakan dengan tanda: jendela yang di-minimize muncul di dock sebagai `_ Nama`.

**5. Aplikasi dengan otoritas minimum.** `bin/deskapps` adalah empat aplikasi dalam satu program;
server memberi tahu mode mana dan menyerahkan satu kapabilitas yang dibutuhkan mode itu:

| Aplikasi | Diberi | Isinya |
|---|---|---|
| Terminal | `SPAWN \| FS \| DUP` | sesi `spaceshell` sendiri: `help`, `status`, `ls`, `run`, `stop`, `clear` |
| Agent Center | `SPAWN \| FS \| DUP` | sesi `spaceshell` sendiri untuk worker inferensi: model sungguhan (5) dengan tugas, rencana, progres, memori, akses, biaya dan perubahan (ADR-0021); worker uji (1–4); **Stop** (S) |
| File manager | `FS` | menelusuri volume: atas/bawah, Enter membuka folder, Backspace naik; bisa dibuka di path tertentu (ADR-0022) |
| Command Center | `SPAWN \| FS \| DUP`, lalu dilepas | `spacelink` sendiri yang hanya bisa membaca: cari, asal setiap hasil, status indeks, context bundle, tampilkan di Files (ADR-0022) |

Karena worker milik sebuah **sesi**, bukan milik jendela yang menampilkannya — dan bukan milik
desktop — worker yang crash atau macet tidak bisa menyeret apa pun bersamanya.

**6. Satu API untuk otomasi dan uji.** Server yang dijalankan proses lain menerima operatornya lewat
channel bootstrap (`OP_*`): jalankan aplikasi, tekan tombol (lewat penangan yang sama dengan
keyboard), baca keadaan setiap jendela (posisi, ukuran, fokus, minimize, workspace, frame yang
sudah ditampilkan), dan minta sebuah jendela **menjelaskan dirinya dengan kata-kata** — setiap
aplikasi menjawab apa yang ditampilkannya (`files: /spaceos, 12 entries, selected MANIFEST.TXT
(134 bytes)`). Itu "semantik kontrol untuk API otomasi" dari PRD, dan itu cara uji menggerakkan
desktop tanpa orang.

**7. Diuji dua arah.** Di dalam guest, empat uji U01 menjadi operator desktop. Dari luar, skenario
`desktop` mem-boot `init=bin/spacedesk` dan menekan tombol sungguhan lewat monitor QEMU (scan code →
IRQ 1 → dekoder → event → pengelola jendela), dengan screenshot di setiap titik penting
(`build/logs/desktop-*.png`).

## Bukti

| Klaim | Bukti |
|---|---|
| Satu lease; tanpa `DISPLAY` ditolak; layar kembali setelah desktop keluar | U01 "the desktop takes the screen, and one display server at a time" |
| Fokus, pindah, ubah ukuran (aplikasi menggambar ulang), minimize, workspace, tutup — dari keyboard | U01 "windows take the keyboard, …" |
| Worker crash (page fault), worker macet dihentikan Stop (43 ms dalam run bukti; PRD §9 meminta ≤ 2 detik), terminal menjawab perintah yang diketik setelah crash, file manager tetap bisa ditelusuri, desktop terus menampilkan frame | U01 "the desktop, terminal, file manager and Stop keep working while inference workers crash" |
| Desktop yang dibunuh dengan jendela terbuka mengembalikan layar, desktop baru bisa mulai | U01 "a desktop that is killed gives the screen back, and a new one starts" |
| Jalur keyboard sungguhan: pintasan, teks ke terminal, Stop, kontras tinggi, Ctrl+Alt+Delete | skenario `desktop`; screenshot `docs/evidence/desktop-*.png` |
| Mesin tanpa framebuffer melewati uji yang membutuhkan layar dengan alasan, tidak gagal | `compat` mesin `no-vga`: 109/109, 6 skipped (keempat uji itu, ditambah Agent Center dan Command Center) |
| Desktop ikut uji stabilitas | setiap putaran `cargo xtask stress` menjalankan keempat uji itu (ADR-0019) |

## Gigi

Setiap pelemahan dijalankan sendiri, lalu dikembalikan. Empat yang pertama dijalankan terhadap
skenario `acceptance` dan membuatnya gagal (exit 1); dua yang terakhir bahkan tidak bisa dibangun:

| Pelemahan | Akibat |
|---|---|
| Lease tidak eksklusif (`SYS_DISPLAY_OPEN` selalu memberi) | `a second lease on the screen was granted` |
| Kernel tidak mengambil layar kembali saat objek memori terakhirnya hilang | keempat uji desktop gagal di `hello: resource busy`: `init` memeriksa ada-tidaknya layar dengan mengambil lease lalu menutupnya, dan lease itu tidak pernah kembali |
| Stop di Agent Center tidak melakukan apa pun | `window 3 never said "'hang' stopped"; last: "agent: worker 'hang' running; Stop ready; 18 commands served"` |
| Tombol dikirim ke jendela pertama, bukan ke jendela yang fokus | `window 3 never said "crashed (page fault)"; last: "agent: no worker has run; Stop has nothing to stop; 22 commands served"` — ketikan untuk Agent Center jatuh ke terminal |
| Teks di atas aksen kontras tinggi kembali putih | `error[E0080]: evaluation panicked: CONTRAST: on_accent on accent is too faint to read` |
| Label Stop kembali putih di atas merah | `error[E0080]: evaluation panicked: the Stop label is too faint to read` |

## Konsekuensi

- Kernel tidak lagi menggambar ketika ada server tampilan; log lengkap tetap di port serial.
- Tidak ada yang dibagi antara jendela kecuali lewat server, dan server hanya membaca.
- Batas yang jujur: **tanpa mouse** (hanya keyboard, sesuai prioritas PRD, tetapi resize/pindah
  dengan mouse belum ada); belum ada pengaturan model di desktop; kontras tinggi hanya untuk bingkai, top bar dan dock — isi aplikasi tetap di tema biasa
  (yang memenuhi 4,5:1); belum ada
  scaling; satu huruf (Noto Sans Mono bitmap 16 px, Latin dasar saja); penyusunan ulang seluruh layar setiap perubahan
  (tanpa damage region); framebuffer UEFI saja (belum virtio-gpu); tata letak keyboard US.
