# ADR-0036 — Task Service: tugas yang bertahan, efek yang tidak ditebak, Stop yang berhenti

- Status: diterima
- Tanggal: 2026-10-06
- Konteks: PRD v0.2 §13 (Task Service dan Agent Runtime), T01 ("State tugas persisten setelah UI
  restart dan reboot"), T02 ("Efek eksternal tak pasti masuk reconciliation, tidak blind retry"),
  G02 ("Stop menghentikan dispatch dan worker sesuai target"), §18 (Stop: dispatch baru berhenti
  maksimal 1 detik); ADR-0015 (penyimpanan yang bisa ditulis), ADR-0021 (worker inferensi dan Stop)

## Konteks

Sampai di sini pekerjaan hidup di proses yang menjalankannya: worker milik sesi, sesi milik
jendela. Menutup jendela, me-restart layanan atau me-reboot mesin menghapus semua yang sedang
dikerjakan, dan tidak ada yang tahu apakah sebuah aksi ke luar mesin (mengirim surel, menagih kartu)
sudah terjadi ketika prosesnya hilang. PRD meminta identitas dan status tugas disimpan di luar
jendela, setiap transisi dengan alasan, waktu dan aktornya, efek yang tidak pasti masuk
`needs_reconciliation` alih-alih diulang buta, retry yang berbatas dengan backoff, concurrency
yang dibatasi, dan Stop yang menghentikan dispatch, meminta pekerjaan aktif berhenti, lalu
mematikan worker bila perlu -- dengan tampilan yang membedakan berhenti diminta, worker berhenti,
dan efek yang sudah terjadi.

## Keputusan

1. **Layanan user-space `spacetask`, klien lewat sesi.** Operator (yang menjalankannya, kini
   `init`) memberinya capability berkas `FS | FS_WRITE` dan membuka sesi untuk klien; setiap sesi
   terikat pada aktornya (`USER` atau `AGENT`) sejak dibuka, jadi klien tidak bisa mengaku orang
   lain. Menutup sesi tidak menyentuh tugas apa pun. ABI-nya `spaceabi::task`.
2. **Jurnal tulis-dulu.** Setiap perubahan ditulis sebagai rekaman 128 byte (magic, nomor urut,
   tugas, jenis, state, aktor, attempt, waktu dinding, teks 88 byte, 8 byte pertama SHA-256 atas
   sisanya) ke `/spaceos/var/tasks0.log` atau `tasks1.log` **sebelum** dijawab, dalam satu
   `fs_write` per perubahan (yang sudah menulis data, FAT dan entri direktori lalu flush). Tulisan
   yang gagal -- disk penuh, error perangkat -- adalah error bagi klien dan tidak mengubah apa pun
   di memori. Rekaman tidak pernah melintasi batas sektor; yang terpotong atau rusak dikenali dari
   checksum-nya, dihitung, dan dilewati, dan rekaman berikutnya ditulis di slot utuh berikutnya.
3. **Dua berkas, epoch di header.** Rekaman pertama setiap berkas adalah header dengan epoch. Jurnal
   adalah berkas dengan header yang sah dan epoch tertinggi. Kompaksi (setelah 1024 rekaman, atau
   atas permintaan operator) menulis keadaan setiap tugas ke berkas lain -- rekamannya dulu, di
   belakang slot header yang masih kosong, baru kemudian header dengan epoch berikutnya. Sampai
   header itu tertulis, jurnal lama tetap jurnalnya, utuh.
4. **Mesin state PRD §13**: `queued`, `running`, `waiting_approval`, `paused`, `succeeded`,
   `failed`, `cancelled`, `needs_reconciliation`, dengan tabel transisi di ABI. Dispatch (`queued`
   → `running`) hanya lewat `CLAIM`; keluar dari `needs_reconciliation` hanya lewat `RECONCILE` atau
   dibatalkan; persetujuan (`waiting_approval` → `running`) hanya oleh `USER`. Tidak ada jalan keluar
   dari state akhir.
5. **Kebijakan restart yang eksplisit.** Saat layanan mulai lagi, tugas yang tercatat `running`
   tidak dianggap selesai, gagal, atau tidak pernah terjadi: tanpa efek terbuka ia `paused`
   (orang melanjutkannya setelah memeriksa), dengan Stop yang belum dijawab ia `cancelled`, dan
   dengan efek yang sudah dimulai tetapi belum dikonfirmasi ia `needs_reconciliation`. Yang
   `queued`, `paused` atau `waiting_approval` tetap di tempatnya. Aktornya `service`, alasannya
   tertulis.
6. **Efek eksternal punya awal dan akhir.** Worker mengumumkan efek dengan kunci (kunci idempotensi
   bila pihak lain mengenalnya) sebelum memulainya dan mengonfirmasi terjadi atau tidaknya setelah.
   Satu efek terbuka per tugas; tugas tidak bisa `succeeded`, `paused` atau `waiting_approval`
   dengan efek terbuka (`Busy`), dan gagal atau dibatalkan dengan efek terbuka menjadikannya
   `needs_reconciliation`. `RECONCILE` menutup efek itu: terjadi (dihitung, tugas `paused` untuk
   dilanjutkan melewatinya) atau tidak (attempt berikutnya bila masih ada, `failed` bila tidak).
7. **Retry dan concurrency berbatas.** Setiap tugas punya batas attempt (bawaan 3, paling banyak
   100). Attempt yang gagal tanpa efek terbuka kembali `queued` setelah backoff 100 ms yang
   berlipat dua setiap kali; attempt terakhir yang gagal adalah `failed`. Paling banyak dua tugas
   `running` sekaligus (`WouldBlock` untuk klaim berikutnya).
8. **Stop (G02).** Stop satu tugas: yang belum berjalan lagi dibatalkan seketika; yang berjalan
   diminta berhenti (`CHECK`), dan sejak itu worker-nya tidak bisa memulai efek baru -- hak yang
   relevan dicabut di sana. Stop semuanya (tugas 0): dispatch berhenti (dicatat di jurnal, jadi
   tetap berhenti setelah restart sampai `RESUME`), setiap tugas yang berjalan diminta berhenti,
   dan yang antre **ditahan, tidak dibatalkan**; jawabannya menghitung terpisah tugas yang diminta
   berhenti, tugas yang ditahan, dan efek yang sudah terjadi. Layanan tidak memiliki worker: worker
   yang tidak berhenti dalam 2 s dimatikan pengawasnya, dan tugasnya -- bila efeknya terbuka --
   menjadi `needs_reconciliation`, bukan `cancelled`.
9. **Retensi.** Layanan memegang 64 tugas. Tugas baru mengambil tempat tugas selesai yang paling
   tua (dicatat sebagai `FORGET`); bila tidak ada yang selesai, `Quota`. Riwayat setiap tugas dibaca
   kembali dari disk (`HISTORY`); kompaksi meringkas riwayat tugas yang dipindahkannya menjadi satu
   rekaman `SNAPSHOT` dengan transisi terakhirnya.
10. **`SYS_FS_OPEN_WRITE` (nomor 41).** Jurnal menambah dirinya sendiri, jadi harus bisa dibuka untuk
    ditulis tanpa dikosongkan. Syscall baru membuka berkas yang ada dengan hak `READ | WRITE` di
    balik hak root yang sama dengan `SYS_FS_CREATE` (`FS | FS_WRITE`); `NotFound` bila tidak ada,
    `Invalid` untuk direktori, `Denied` pada volume yang menolak tulisan. Ini mengubah satu kalimat
    ADR-0015 ("satu-satunya cara mendapat handle yang bisa ditulis adalah `SYS_FS_CREATE`"); aturan
    "menulis adalah hak tersendiri" tetap.

## Bukti

| Klaim | Uji |
|---|---|
| Tugas bertahan setelah jendela ditutup | `T01`: sesi pertama membuat dan menjeda tugas lalu ditutup; sesi berikutnya melihat judul, pemilik, workspace, state, alasan dan jumlah transisinya, lalu melanjutkan dan membatalkannya |
| Setiap transisi dengan alasan, waktu dan aktor; restart layanan | `T01`: dispatch (service), minta persetujuan (agent), persetujuan oleh agent ditolak `Denied`, disetujui (user), selesai (agent); layanan baru memberikan tugas yang sama dan riwayat yang dibaca dari disk berurutan waktu dengan aktor dan alasan setiap transisi |
| Tugas bertahan setelah reboot | `T01`: setiap pass meninggalkan tugas `running` dengan efek terbuka lalu layanan dibunuh; boot berikutnya (skenario `storage-reboot`, boot 2) menemukannya `needs_reconciliation` oleh `service`, dengan efek dan checkpoint-nya |
| Rekaman terpotong | `T01`: 60 byte pertama sebuah rekaman ditulis di akhir jurnal; layanan menghitungnya rusak, tugasnya tetap `queued`, dan tugas yang ditulis sesudahnya kembali setelah restart |
| Kompaksi | `T01`: kompaksi pindah ke berkas lain dengan epoch berikutnya, tugas tetap sama; setelah restart epoch yang lebih baru dipakai; berkas yang rekamannya tertulis tetapi headernya belum (kompaksi yang terputus) diabaikan |
| Retensi | `T01`: 64 tugas yang belum selesai mengisi tabel dan yang berikutnya `Quota`; setelah satu selesai, tugas baru mengambil tempatnya |
| Efek tak pasti tidak diulang buta | `T02`: layanan dibunuh dengan efek terbuka; setelah restart tugasnya `needs_reconciliation`, tidak diklaim, tidak bisa dikembalikan ke antrean atau dinyatakan selesai, sampai user mengatakan efeknya tidak terjadi; attempt 2 memakai kunci yang sama dan efeknya terjadi sekali |
| Retry berbatas dengan backoff | `T02`: tiga kegagalan; klaim ditolak selama backoff 100 ms lalu 200 ms; setelah attempt ketiga `failed` dan tidak diklaim lagi |
| Gagal dengan efek terbuka | `T02`: `succeeded` dan efek kedua ditolak `Busy`; `failed` menjadi `needs_reconciliation`; dikonfirmasi terjadi → `paused` dengan satu efek terhitung, dilanjutkan tanpa mengulang efeknya |
| Stop menghentikan dispatch ≤ 1 s dan worker | `G02`: dua worker menjalankan dua tugas, klaim ketiga `WouldBlock`; Stop semuanya menghentikan dispatch (klaim berikutnya `Denied`), kedua worker membatalkan tugasnya sendiri, dua tugas antre ditahan (tetap `queued`), efek yang sudah terjadi dihitung |
| Worker yang tidak berhenti | `G02`: worker yang memulai efek lalu macet tidak menjawab Stop; setelah 2 s ia dimatikan, dan tugasnya `needs_reconciliation`, bukan `cancelled` |
| `SYS_FS_OPEN_WRITE` | `D01`: berkas yang dibuka untuk ditulis menyimpan isinya dan tumbuh di ujungnya; tanpa `FS_WRITE` `Denied`, berkas yang tidak ada `NotFound`, direktori `Invalid` |

## Gigi

| Pelemahan | Akibat |
|---|---|
| Efek yang terbuka saat layanan hilang dianggap sudah terjadi (`succeeded` setelah restart) | `T02` merah: `after the restart: task 71 is succeeded (service restarted; effect 'invoice-2026-10-mail' may or may not have happened), expected needs_reconciliation` |
| Rekaman dipercaya tanpa checksum-nya | `T01` merah: `past the tear, after a restart: get task 4: not found` |
| Jawaban diberikan tanpa menulis ke jurnal | `T01` merah: `after a restart: get task 1: not found` |
| Attempt yang gagal langsung bisa diklaim lagi, tanpa backoff | `T02` merah: `attempt 2 was dispatched 6 ms after the failure (0 claim(s) refused first); its backoff is 100 ms` |
| Stop tidak menghentikan dispatch | `G02` merah: `a claim after Stop: accepted, expected permission denied` |
| Dari dua berkas jurnal, yang lebih tua dipercaya | `T01` merah: `after a restart: get task 2: not found` |
| Agent menyetujui pekerjaannya sendiri | `T01` merah: `the agent approving itself: accepted, expected permission denied` |
| Membuka berkas untuk ditulis mengosongkannya | `D01` merah: `open for writing a missing file gave Ok(72961)` |

## Konsekuensi

- Data tugas baru mencakup identitas (ID, pemilik, workspace, induk), eksekusi (state, attempt,
  efek, checkpoint) dan audit (riwayat transisi, efek); scope (capability, tujuan jaringan, kebijakan
  data, kedaluwarsa) dan batas sumber daya (RAM, CPU, token, biaya) belum menjadi bagian tugas.
- Tugas terjadwal dan kebijakan untuk jadwal yang terlewat saat offline (skip, run-once, tanya)
  belum ada.
- Backoff dihitung dari uptime dan tidak dicatat: setelah restart, tugas yang menunggu backoff bisa
  langsung diklaim.
- Layanan tidak memiliki worker; mematikan worker yang tidak berhenti adalah tugas pengawasnya
  (sesi, desktop, `init` dalam uji).
- Belum ada Task Center di desktop; klien saat ini adalah uji `init` dan worker uji.
