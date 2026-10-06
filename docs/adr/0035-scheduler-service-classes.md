# ADR-0035 — Kelas layanan scheduler: yang ditunggu orang berjalan lebih dulu

- Status: diterima
- Tanggal: 2026-10-06
- Konteks: PRD v0.2 §8.2 ("Scheduler awal bersifat preemptive dan mempunyai prioritas atau kelas
  layanan untuk menjaga kendali pengguna"), K02 ("Preemption menghentikan loop CPU tak
  kooperatif"); ADR-0024 (SMP, satu run queue), ADR-0021 (worker inferensi dan Stop)

## Konteks

Penjadwal adalah round robin murni dengan satu run queue untuk semua CPU. Loop tak berujung tidak
bisa mengunci sistem (quantum 10 ms), tetapi semua yang siap berjalan antre di belakang yang lain
dengan bobot yang sama. Worker inferensi yang memakai setiap CPU membuat sesi, terminal dan desktop
menunggu giliran: dengan dua pekerjaan berat per CPU, satu pesan bolak-balik sesi menunggu dua quantum
penuh di setiap arah. PRD meminta kelas layanan justru untuk menjaga kendali pengguna.

## Keputusan

1. **Tiga kelas, satu run queue per kelas:** `INTERACTIVE` (yang ditunggu orang: konsol, desktop,
   sesi), `NORMAL` (program yang tidak ditunggu orang tetapi bukan beban; tersedia bagi pemanggil
   yang memintanya) dan `BACKGROUND` (beban: worker inferensi dan pekerjaan yang dijalankan sesi).
   Kelas adalah milik proses (`sched_class` di ABI, `SpawnArgs.class`, `SelfInfo.class`);
   thread-nya diantrekan menurut kelas itu. `init` berjalan `INTERACTIVE`, dan layanan yang
   dijalankannya (jaringan, SpaceLink, broker, desktop) mewarisi kelas itu karena melayani sesi.
2. **Anak tidak pernah lebih mendesak dari induknya.** `INHERIT` (0, nilai lama `_pad`) memberi anak
   kelas induknya, jadi pemanggil lama tidak berubah perilakunya; kelas yang lebih mendesak dari
   induk ditolak `Denied` sebelum handle bootstrap diambil, kelas yang tidak ada `Invalid`.
3. **Yang paling mendesak dulu, round robin di dalam kelas.** CPU yang memilih mengambil kelas
   paling mendesak yang punya thread siap; thread yang sedang berjalan hanya diganti oleh kelas yang
   sama (quantum habis) atau yang lebih mendesak.
4. **Preemption segera, bukan di akhir quantum.** Thread yang diantrekan di kelas yang lebih mendesak
   dari yang dijalankan suatu CPU membuat CPU itu dibangunkan IPI (yang menjalankan kelas paling
   rendah); handler IPI menjadwal ulang bila ada yang lebih mendesak. Tick juga memeriksanya. CPU yang
   sendiri membangunkan thread yang lebih mendesak menyerahkan CPU-nya paling lambat di tick
   berikutnya.
5. **Tidak ada yang menunggu selamanya.** Kelas yang punya thread siap tetapi sudah 100 ms tidak
   mendapat CPU (`STARVE_TICKS`) mendapat pilihan berikutnya, yang paling rendah lebih dulu. Ukuran
   waktu, bukan jumlah pilihan: IPC interaktif yang sering memblok tidak membuat kelas rendah
   menyela setiap beberapa pesan.
6. **Sesi menjalankan pekerjaannya di `BACKGROUND`:** worker uji, `spaceai` dan `spacecompute`
   miliknya. Pemeriksaan kesehatan sesi (`cmd::CHECK`) tetap di kelas sesi.

## Bukti

| Klaim | Uji |
|---|---|
| Aturan kelas | `K02`: `init` berjalan `interactive`; kelas 4 → `Invalid`; dari proses `background` (`bin/sched rules`): anak `interactive`/`normal` → `Denied` dan handle bootstrap tetap miliknya, anak tanpa kelas dan anak `background` berjalan `background` |
| Kendali pengguna di bawah beban | `K02`: 2 spinner `background` per CPU; 200 pesan bolak-balik `init` ↔ `bin/ipc_echo` (keduanya `interactive`): p50 ≤ 2 ms dan p95 kurang dari satu quantum (10 ms); tanpa kelas setiap pesan menunggu quantum penuh. Ekornya adalah host yang tidak menjadwalkan vCPU yang sibuk (QEMU TCG, 4 vCPU di 4 core host): p99 1–6 ms antar-run |
| Preemption segera | `K02`: pesan bolak-balik tidak membutuhkannya -- pengirim memblok dan CPU-nya mengambil thread siap yang paling mendesak. Thread yang dibangunkan timer tidak diserahi CPU oleh siapa pun: dengan 2 spinner `background` per CPU, `init` tidur 1–4 ms sebanyak 100 kali dan harus bangun terlambat p90 ≤ 2 ms (tanpa preemption segera ia menunggu quantum pertama yang habis: p90 5 ms) |
| Tidak ada kelaparan | `K02`: 2 spinner `interactive` per CPU; program `background` tetap selesai (≤ 3 s) |

## Gigi

| Pelemahan | Akibat |
|---|---|
| Satu run queue untuk semua kelas: kelas diabaikan saat thread diantrekan | `K02` merah: pesan bolak-balik interaktif p50 26 ms, p95 29 ms di samping spinner `background` (batas p50 ≤ 2 ms, p95 < 10 ms) |
| Kelas yang sudah 100 ms tidak mendapat CPU tidak pernah diberi giliran | `K02` merah: program `background` tidak selesai dalam 3000 ms di samping spinner `interactive` |
| Preemption segera dimatikan: IPI dan tick hanya menjadwal ulang di akhir quantum | `K02` merah: thread interaktif yang dibangunkan timer terlambat p50 2 ms, p90 5 ms (batas p90 ≤ 2 ms); pesan bolak-balik tetap cepat (p50 0 ms, p95 1 ms) karena pengirim memblok dan menyerahkan CPU-nya |
| Anak boleh lebih mendesak dari induknya | `K02` merah: `bin/sched rules` memulai anak `interactive` dari proses `background` |

## Konsekuensi

- Pekerjaan `BACKGROUND` mendapat paling sedikit sekitar satu quantum per 100 ms per kelas yang
  menunggu ketika kelas di atasnya memenuhi semua CPU; itu cukup untuk maju, bukan untuk throughput.
- Kelas tidak mengubah kuota memori atau hak: hanya urutan di CPU.
- Belum ada prioritas di dalam kelas, pewarisan prioritas lewat IPC, atau deadline. Layanan yang
  melayani klien interaktif sebaiknya berjalan di kelas klien itu.
