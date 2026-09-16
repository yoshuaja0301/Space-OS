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

## Konsekuensi

- Polling 2 ms adalah kompromi yang disengaja karena belum ada `select`, `recv`
  bertimeout, atau notifikasi exit lewat channel. Bila salah satunya ada, loop ini
  yang berubah, bukan kontraknya.
- "Desktop" di sini adalah konsol teks framebuffer yang sudah ada (`SYS_LOG`
  menulis ke serial dan framebuffer). Belum ada window manager, dan **belum ada
  masukan keyboard ke user space**: perintah sesi datang lewat channel kontrol,
  bukan dari orang yang mengetik. U01 karena itu tetap *experimental*, bukan
  *verified* — yang dibuktikan adalah ketahanan sesinya.
- Bendera `NONBLOCK` pada `SYS_WAIT` memakai argumen ketiga yang sebelumnya
  diabaikan. Nilai bendera lain ditolak `Invalid` supaya penambahan berikutnya
  tidak diam-diam berubah arti.
- Satu worker per sesi. Beberapa job paralel memerlukan tabel worker dan
  penjadwalan di dalam shell; belum ada.
