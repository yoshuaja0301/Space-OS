# ADR-0034 — Kehabisan memori dan model rusak tidak menutup konsol

- Status: diterima
- Tanggal: 2026-10-06
- Konteks: PRD v0.2 A02 ("Model rusak dan OOM worker tidak menutup console"), §8.1 ("OOM ditangani
  melalui penolakan alokasi dan penghentian worker sesuai prioritas. Memori cadangan untuk console,
  broker Stop, serta layanan inti harus ditetapkan melalui pengukuran"); ADR-0011 (sesi dan
  supervisi), ADR-0021 (worker inferensi dan Stop kooperatif)

## Konteks

Sesi (`spaceshell`) sudah terbukti bertahan saat worker crash, macet atau tidur (U01). Yang belum
terbukti adalah dua cara gagal yang paling mungkin bagi worker AI: berkas model yang rusak, dan
worker yang kehabisan memori -- baik memorinya sendiri (kuota) maupun memori seluruh mesin. Untuk
yang kedua pertanyaannya bukan hanya apakah worker berhenti, tetapi apakah konsol masih menjawab
ketika tidak ada satu frame pun yang bebas, dan apakah kernel sendiri tidak pernah berasumsi
mendapat frame.

Dua celah ditemukan saat merancang ujinya. `spaceai` yang menolak model sebelum langkah pertama
keluar dengan kode 1 tanpa memberi tahu sesi alasannya (laporan akhir hanya dikirim dari dalam
loop inferensi). Dan heap setiap program dipetakan saat alokasi pertama, jadi sesi yang belum
pernah mengalokasi bisa mati pada perintah pertamanya di mesin yang penuh.

## Keputusan

1. **Model yang rusak ditolak sebelum satu langkah pun, dan sesi mendengar alasannya.** `spaceai`
   membaca header dulu (format, dimensi, ukuran tensor), lalu mencocokkan ukuran dan SHA-256 dengan
   manifest; model yang lolos header tetapi berbeda satu bit ditolak oleh digest. Kegagalan apa pun
   yang belum dilaporkan dikirim ke sesi sebagai laporan `FAILED` dengan alasannya. Sesi bisa
   menjalankan inferensi pada berkas lain (`infer <path>`, paling panjang 64 byte), yang tetap harus
   model yang dinamai manifest: itu jalan untuk menguji model rusak tanpa merusak disk, bukan jalan
   untuk memuat model yang tidak terverifikasi.
2. **Kehabisan memori adalah penolakan, bukan panic.** Setiap permintaan memori yang tidak bisa
   dipenuhi dijawab `Quota` atau `NoMemory`, dan pemetaan yang setengah jadi dikembalikan seluruhnya.
   Diperiksa untuk setiap pemanggil allocator frame di kernel: pemetaan user (`mem_map`, objek
   memori, segmen dan stack saat spawn), address space baru, kernel stack, page table, semuanya
   mengembalikan error; halaman DMA driver diambil saat driver mulai, dan enumerasi perangkat USB
   yang tidak mendapat halaman gagal dengan pesan. Hanya inisialisasi saat boot dan selftest yang
   boleh `expect`. Heap kernel (16 MiB) dipetakan saat boot dan terpisah dari frame, jadi memori
   mesin yang habis tidak menyentuhnya; kehabisan heap kernel sendiri tetap batasan tersendiri
   (`docs/limitations.md`).
3. **Cadangan konsol diukur, bukan ditebak: semuanya sudah dipetakan.** Sesi memetakan heap-nya
   sebelum melayani apa pun (`heap::map_now`), sehingga melayani perintah tidak pernah meminta frame
   baru. Sesi mencetak berapa halaman yang ia pegang (`the session holds N pages`), dan itulah
   cadangannya.
4. **Satu proses memegang paling banyak 4 GiB** (`MAX_QUOTA_PAGES`, kini bagian ABI; kuota yang
   lebih besar `Invalid`). Di mesin dengan memori lebih dari itu tidak ada satu proses pun yang bisa
   menghabiskannya sendirian; uji mesin penuh memakai worker sebanyak yang diperlukan.
5. **Penghentian sesuai prioritas adalah Stop:** worker yang mengambil semuanya tetap bisa
   dihentikan, dan setiap frame-nya kembali. Kernel tidak membunuh siapa pun sendiri.

## Bukti

| Klaim | Uji |
|---|---|
| Model rusak ditolak dengan alasan, sesi lanjut | `A02`: `infer /spaceos/bitflip.slm` (satu bit di bobot) → `… does not match the manifest`; `infer /spaceos/badmagic.slm` → `model rejected: not a SpaceLM model`; nol token, kode keluar 1, lalu `status`, daftar berkas dan job `ok` dijawab |
| Worker yang kehabisan kuota mati sendirian | `A02`: job `oom` meminta memori sampai kuotanya menolak, lalu panic (kode 101); sesi menjawab sesudahnya |
| Worker yang mengambil seluruh mesin | `A02`: worker dengan kuota bersama di atas memori mesin mengambil setiap frame bebas dan masing-masing melapor `full` saat satu halaman pun ditolak; selama itu sesi menjawab `status` dan daftar berkas, program baru ditolak `NoMemory`, Stop mengakhiri semuanya, dan jumlah frame bebas kembali tepat seperti sebelumnya. Di acceptance (8 GiB): `3 worker(s) took 8162 MiB in 22917 ms and left 0 frames free; … all 2089722 frames came back` -- dua berhenti karena `out of memory`, satu karena kuotanya |
| Cadangan konsol | `the session holds 63 pages` (kode, stack 64 KiB, heap 128 KiB) di setiap sesi, dan itu cukup untuk `status` dan daftar berkas di mesin tanpa frame bebas |

## Gigi

| Pelemahan | Akibat |
|---|---|
| `spaceai` tidak membandingkan digest model dengan manifest | model yang bobotnya berbeda satu bit dimuat dan dijalankan: `exit code 1, 128 tokens, report "token 4 is 216, baseline says 237"` -- keluarannya menyimpang di token keempat |
| Run yang gagal sebelum langkah pertama tidak memberi tahu sesi | `exit code 1, 0 tokens, report ""`: sesi hanya melihat worker berakhir, tanpa alasan |
| Sesi memetakan heap-nya pada alokasi pertama, bukan saat mulai | di mesin yang penuh, daftar berkas adalah alokasi pertama sesi: `panicked at …/alloc.rs`, lalu `listing with no memory left: Err(PeerClosed)` |
| Kernel berasumsi selalu ada frame untuk pemetaan user | `KERNEL PANIC` saat worker pertama mencapai akhir memori (exit 127) |
| Pemetaan yang ditolak tidak mengembalikan frame yang sudah diambilnya | `2089722 frames free … before the workers, 2077509 … after they were stopped`: 12 213 frame (48 MiB) hilang |

## Konsekuensi

- Uji mesin penuh membawa frame bebas mesin ke dasar di setiap putaran acceptance dan stress: titik
  terendah (`frames_free_min`) di ringkasan stress karena itu nol menurut rancangan, bukan karena
  kebocoran. Kebocoran tetap diukur dari frame bebas setelah setiap putaran.
- Belum ada penghentian otomatis worker berprioritas rendah saat memori habis: kernel menolak, dan
  Stop datang dari sesi (atau dari kuota yang lebih kecil).
- Program biasa yang heap-nya habis tetap mati (heap tetap, `panic` → 101); hanya layanan yang
  memetakan semuanya saat mulai yang kebal terhadap mesin yang penuh.
- Satu proses tidak bisa memakai lebih dari 4 GiB, juga untuk model besar: menaikkan batas itu
  adalah keputusan tersendiri.
