# ADR-0011 — Layanan sesi (`spaceshell`) dan aturan supervisi

- Status: diterima
- Tanggal: 2026-09-16
- Konteks: PRD §7 U01, ADR-0004 (capability), ADR-0007 (ABI file)

## Konteks

U01 berbunyi: desktop, terminal, file manager dan tombol **Stop** tetap hidup
ketika worker inferensi crash. Yang sebenarnya diminta bukan "punya GUI", tetapi
satu properti: **tidak ada keadaan worker yang boleh membuat sesi berhenti
melayani.** Worker bisa menghitung, tidur di dalam syscall, atau macet dalam loop
yang tidak pernah masuk kernel lagi. Ketiganya harus berakhir sama: Stop bekerja,
daftar berkas masih bisa dibuka, dan sesi tetap menjawab.

Satu proses pengawas yang memanggil `wait` pada anaknya adalah justru bug yang
dimaksud persyaratan ini: pengawas ikut terkunci selama worker hidup. Karena
kernel hanya punya satu thread per proses dan belum punya `select`/multi-wait,
aturan bentuk loop-nya harus ditetapkan, bukan diserahkan pada gaya penulisan.

## Keputusan

**`spaceshell` adalah proses user-space yang memiliki sesi.** Ia memegang
tampilan konsol, daftar berkas, dan masa hidup worker. Worker berjalan di address
space sendiri dengan kuota sendiri (`bin/uiworker` pada uji), jadi crash-nya
adalah peristiwa kernel biasa: proses mati, pemiliknya tidak.

**Loop sesi tidak pernah memblokir pada worker.** Dua aturan:

1. `recv` pada channel kontrol memakai mode non-blocking, lalu `sleep(2 ms)` bila
   kosong. Sesi yang menganggur tidak membakar CPU, Stop tetap terasa seketika.
2. Worker dipanen dengan `SYS_WAIT` bendera `NONBLOCK` (baru pada ABI ini):
   mengembalikan `WouldBlock` selama anak masih hidup, bukan tidur. Ini satu-satunya
   alasan bendera itu ada.

`STOP` memakai `kill` lalu `wait` biasa: targetnya sudah mati saat itu, jadi
`wait` tidak bisa menggantung. `kill` bekerja pada worker yang macet karena
preemption timer sudah ada (ADR-0006) — worker yang tidak pernah memanggil syscall
tetap dijadwalkan keluar.

**Sesi hanya sekuat kapabilitas yang diserahkan kepadanya.** `spaceshell` lahir
hanya dengan channel kontrolnya. Front end mengirim `HELLO` beserta satu handle
root yang **sudah dipersempit** ke `SPAWN | FS`. Konsekuensinya bisa diuji: sesi
tanpa hak `FS` menjawab `Denied` untuk daftar berkas tetapi tetap menjalankan
job, dan tidak ada sesi yang bisa mematikan mesin, membaca statistik kernel, atau
menyuntik fault kernel.

**`SYS_FS_LIST` melengkapi ABI file.** Tanpa membaca isi direktori, "file manager"
tidak punya arti. Syscall ini mengembalikan `DirEntry` (nama 8.3, flag direktori,
ukuran), maksimum 64 per panggilan, di balik hak root `FS` yang sama dengan
`FS_OPEN`. Entri `.` dan `..` milik subdirektori FAT ikut dikembalikan apa adanya;
menyembunyikannya adalah urusan tampilan, bukan urusan ABI.

**Masukan konsol punya jalurnya sendiri.** Kernel mengumpulkan byte dari dua
sumber ke satu ring buffer: keyboard PS/2 (IRQ 1, scan code set 1) dan UART kedua
COM2 (IRQ 3). `SYS_CONSOLE_READ` menyerahkannya ke user space di balik hak root
**`CONSOLE`** yang terpisah dari semua hak root lain — layanan sesi butuh
ketikan dan tidak butuh yang lain, dan tidak ada proses lain yang boleh membacanya.
Syscall itu **tidak pernah memblokir**: ia dipanggil dari loop yang sama, jadi
memblokir di sana akan menjadi satu lagi cara sesi berhenti melayani.

Kernel tidak melakukan echo dan tidak mengenal baris. Ia menyerahkan byte; sesi
yang memutuskan apa itu baris dan apa yang ditampilkan. Semantik terminal adalah
urusan user space.

COM2 dipilih, bukan COM1, karena COM1 membawa log keluar dari mana saja termasuk
handler panic; mencampur masukan ke port yang sama berarti masukan bisa tertahan
oleh log, atau sebaliknya. Mesin tanpa COM2 (setiap skenario uji selain
`terminal-serial`) tetap boot: probe gagal, kernel mencetak "no COM2 UART", dan
keyboard tetap bekerja.

Satu temuan yang perlu dicatat karena mahal ditemukan ulang: **masukan tidak boleh
dialirkan lewat chardev socket pada port serial kedua.** OVMF menjadikan setiap
port serial yang ditemukannya bagian dari konsolnya; chardev socket bisa menolak
tulisan, tulisan konsol firmware gagal, dan `uefi::println!` panik — lalu handler
panik mencoba mencetak dan panik lagi, jadi bootloader mati berulang sebelum kernel
sempat jalan. Chardev **pty** tidak punya backpressure itu dan aman; keyboard lewat
monitor QEMU tidak menyentuh subsistem serial sama sekali.

**Boot langsung ke sesi.** `init=` pada command line kernel memilih proses user
pertama, jadi image yang sama bisa dipakai untuk acceptance run (`bin/init`) atau
untuk sesi interaktif (`init=bin/spaceterm`) tanpa build ulang. `bin/spaceterm`
membuka satu sesi, menyerahkan root yang dipersempit ke `SPAWN|FS|CONSOLE`, lalu
hanya menunggu — ia menahan hak `SHUTDOWN` untuk dirinya sendiri, sehingga sesi
tidak bisa mematikan mesin.

## Konsekuensi

- Polling 2 ms adalah kompromi yang disengaja karena belum ada `select`, `recv`
  bertimeout, atau notifikasi exit lewat channel. Bila salah satunya ada, loop ini
  yang berubah, bukan kontraknya.
- "Desktop" di sini adalah konsol teks framebuffer yang sudah ada (`SYS_LOG`
  menulis ke serial dan framebuffer). **Belum ada window manager atau GUI**;
  yang ada adalah terminal, daftar berkas, dan Stop — ketiganya bisa diketik
  orang, dan skenario `terminal` mengetikkannya sungguhan lewat COM2.
- Line editor di dalam sesi hanya mengenal karakter cetak dan backspace. Tidak ada
  riwayat perintah, penyuntingan di tengah baris, atau urutan escape (tombol panah
  diabaikan alih-alih ditebak).
- Peta scan code hanya set 1 tata letak US dan hanya tombol yang dibutuhkan baris
  perintah. Tombol yang tidak dipetakan tidak menghasilkan apa-apa.
- Bendera `NONBLOCK` pada `SYS_WAIT` memakai argumen ketiga yang sebelumnya
  diabaikan. Nilai bendera lain ditolak `Invalid` supaya penambahan berikutnya
  tidak diam-diam berubah arti.
- Satu worker per sesi. Beberapa job paralel memerlukan tabel worker dan
  penjadwalan di dalam shell; belum ada.
