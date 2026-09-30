# ADR-0021 — Worker inferensi di dalam sesi: laporan per token dan Stop kooperatif

- Status: diterima
- Tanggal: 2026-09-30
- Konteks: PRD §3 ("worker inferensi memiliki kuota RAM, batas token dan waktu, serta penghentian
  kooperatif di antara langkah komputasi"), PRD §6 (Agent Center menampilkan rencana, progres, izin,
  konsumsi sumber daya, biaya cloud, preview perubahan, dan tombol Stop), PRD §9 (skenario ujung ke
  ujung pertama: "boot offline, muat model guest, hasilkan teks, hentikan worker, dan pastikan
  terminal tetap hidup"; worker berhenti ≤ 2 detik pada backend CPU); ADR-0011 (sesi `spaceshell`),
  ADR-0009 (`spaceai`), ADR-0020 (desktop)

## Konteks

Sampai sekarang worker yang diawasi sesi adalah `bin/uiworker`: program uji yang bisa selesai,
crash, macet, atau tidur. Itu membuktikan sesi bertahan, tetapi tidak ada model yang benar-benar
menulis teks di bawah pengawasan sesi, dan Stop selalu berarti `kill`. PRD meminta lebih: Stop yang
**kooperatif** — worker berhenti di antara dua langkah komputasi, tidak di tengahnya — dan Agent
Center yang menunjukkan apa yang sedang dikerjakan worker, dengan apa, dan berapa biayanya.

## Keputusan

**1. Job `infer` adalah `spaceai` sungguhan.** `run infer` membuat sesi memunculkan
`bin/spacecompute` dan `bin/spaceai`, lalu memberi worker tepat tiga hal: channel ini (untuk
melapor dan untuk diminta berhenti), satu koneksi compute, dan akses **baca** berkas. Akses baca itu
dibuat sesi dengan mempersempit kapabilitasnya sendiri (`handle_dup` ke `FS | TRANSFER`), jadi
worker selalu memegang **lebih sedikit** dari sesinya, tidak pernah lebih; karena itu root sesi kini
juga membawa hak `DUP`. Sesi tanpa `DUP` atau tanpa `FS` tidak bisa memulai job ini (`Denied`).
Worker tidak bisa spawn, menulis, memakai jaringan, atau melihat proses lain. Layanan compute ikut
diakhiri dan dituai bersama worker.

**2. Sesi diumumkan sebelum kapabilitas apa pun.** Pesan pertama ke worker adalah `session` tanpa
handle; baru sesudahnya `compute` dan `fs`. Dengan begitu `spaceai` tahu sejak awal bahwa ada yang
mengawasi. Tanpa pesan itu (uji A01), `spaceai` berjalan seperti sebelumnya: log dan kode keluar.

**3. Laporan per token.** Setelah setiap token, worker mengirim `JobReport` (token selesai, yang cocok
dengan baseline, target, TTFT, waktu berjalan, halaman memori, dan ekor teks). `send` tidak pernah
memblokir: laporan progres yang tidak muat di antrean dibuang, karena laporan berikutnya membawa
semua yang dibawanya. Laporan **akhir** (selesai, dihentikan, gagal) diulang sampai 200 ms, karena
yang itu harus sampai. Sesi menyerap semuanya tanpa memblokir dan menjawab `PROGRESS` dengan
struktur `Progress` — itu yang dibaca Agent Center dan uji.

**4. Stop meminta dulu, lalu membunuh.** Untuk job yang bisa diminta, sesi mengirim `stop` dan
memberi worker `STOP_GRACE_MS` = 1000 ms untuk keluar sendiri; baru sesudahnya `kill`. `spaceai`
memeriksa permintaan itu **sebelum setiap operasi compute** — antara dua langkah, tidak pernah di
dalam satu — lalu melapor `STOPPED`, menutup layanan compute, dan keluar dengan kode 3. Sesi mencatat
apakah Stop kooperatif dan berapa milidetik. Job lain (uji) tetap langsung dibunuh, seperti ADR-0011.
Stop tetap sinkron: jawabannya datang setelah worker benar-benar pergi.

**5. Agent Center menampilkannya** (PRD §6): tugas, rencana, keadaan, bilah progres dengan token,
TTFT dan token/detik, memori worker terhadap kuotanya, **akses** (baca berkas, satu koneksi compute;
tanpa jaringan, tulis, atau spawn), **biaya** (tidak ada: model berjalan di mesin ini), **perubahan**
(tidak ada: job hanya membaca), dan ekor teks yang ditulis. Tombol `5` memulai model, `S` Stop.

## Bukti

| Klaim | Bukti |
|---|---|
| Model menulis 128 token lewat sesi, semuanya sama dengan baseline, dengan TTFT dilaporkan | U01 "the model writes text in a session, …": `the model wrote 128/128 tokens, all matching the baseline` |
| Stop berhenti di antara dua langkah, ≤ 2 detik, dan teks yang sudah ditulis tetap benar | uji yang sama: `Stop ended the worker between two steps after 8/128 tokens, 11 ms after it was asked`; token yang ditulis = token yang cocok |
| Sesi menjawab dan membuka daftar berkas sesudahnya; tidak ada proses yang tertinggal | uji yang sama: jumlah proses sebelum dan sesudah sesi sama |
| Sesi tidak bisa memberi worker apa yang tidak dimilikinya | U01 "a session cannot exceed …": `infer` tanpa `FS`/`DUP` → `Denied` |
| Di desktop: model menulis, Stop, terminal menjawab | U01 "the model writes text in the Agent Center, …": `worker 'infer' stopped between two steps after 9 of 128 tokens (Stop took 2 ms)`; skenario `desktop` dengan tombol sungguhan dan screenshot `desktop-3-inference-stopped.png` |

## Gigi

Setiap pelemahan dijalankan sendiri terhadap skenario `acceptance`, lalu dikembalikan:

| Pelemahan | Akibat |
|---|---|
| `spaceai` tidak pernah memeriksa permintaan Stop | `the worker did not stop between two steps; it was killed after 1005 ms`; di desktop: `agent: worker 'infer' was killed by Stop after 1002 ms` |
| Sesi langsung membunuh tanpa meminta | `… it was killed after 5 ms`; di desktop: `agent: worker 'infer' was killed by Stop after 3 ms` |

Keduanya juga menunjukkan mengapa uji menuntut "di antara dua langkah" dan bukan hanya "berhenti
≤ 2 detik": pembunuhan setelah 5 ms memenuhi batas waktu, tetapi bukan penghentian kooperatif yang
diminta PRD §3.

## Konsekuensi

- Stop punya dua jalan dan uji membedakannya: yang kooperatif dilaporkan sebagai "berhenti di antara
  dua langkah", yang dibunuh sebagai "dibunuh setelah N ms".
- Latensi Stop dibatasi oleh satu operasi compute (milidetik untuk model referensi) ditambah poll
  sesi (2 ms). Model yang jauh lebih besar dengan operasi yang lebih lama akan memperpanjangnya;
  batas 1 detik dan `kill` tetap menjamin ≤ 2 detik secara keseluruhan.
- Batas yang jujur: pemuatan model (membaca dan menghitung SHA-256) tidak memeriksa Stop — Stop pada
  saat itu baru terlihat di operasi compute pertama sesudahnya, dan bila pemuatan belum selesai dalam
  1 detik, worker dibunuh. Satu worker per sesi; belum ada antrean job,
  batas token atau waktu yang dapat diatur, atau pilihan prompt dan model dari Agent Center.
