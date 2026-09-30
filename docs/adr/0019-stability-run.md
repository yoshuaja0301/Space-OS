# ADR-0019 — Uji stabilitas: seluruh suite berulang dalam satu boot, pembunuhan acak, dan memori yang harus kembali tepat

- Status: diterima
- Tanggal: 2026-09-29
- Konteks: PRD §9 "Ukuran keberhasilan dan pengujian", baris **Stabilitas MVP**: "100 boot; stress 8
  jam tanpa panic", metode "Log otomatis, fault injection dan monitoring memory"; K01 (100 cold boot,
  sudah dipenuhi `xtask soak`), K03 (kuota dan reclamation); ADR-0006 (satu CPU), ADR-0016 (jaringan)

## Konteks

Setiap uji sejauh ini berjalan sekali per boot, lalu mesin dimatikan. Itu membuktikan setiap fitur
bekerja **dari keadaan bersih**. Yang tidak dibuktikannya: apa yang terjadi pada boot yang hidup
berjam-jam — sisa kecil yang tertinggal di kernel setiap kali sebuah proses mati, alamat yang tidak
pernah dipakai lagi, deskriptor yang tidak pernah ditutup di sisi host, koneksi yang tidak pernah
berakhir. Kebocoran yang tumbuh satu frame per menit tidak terlihat dalam satu boot 30 detik, dan
terlihat sebagai crash setelah beberapa hari.

PRD meminta tiga hal dari metode ini: log yang dibuat otomatis, *fault injection*, dan pemantauan
memori. Tiga hal itu diputuskan di sini.

## Keputusan

**1. Beban stress adalah seluruh suite penerimaan, diulang dalam satu boot.** `stress=N` di command
line kernel membuat `init` menjalankan suite N kali; `stress=Nm` terus memulai putaran selama N
menit. Beban buatan (loop yang memanggil satu syscall berjuta kali) hanya menguji jalur yang
dipilih penulisnya; suite penerimaan sudah menyentuh semuanya — proses, IPC, fault di setiap jenis
eksepsi CPU, disk baca dan tulis, compute, inferensi 128 token, sesi, broker, SpaceLink, paket,
jaringan, TLS dan adapter cloud — dan setiap uji sudah tahu jawaban yang benar. Satu putaran
sekitar 28 detik di TCG, jadi delapan jam berarti sekitar seribu putaran, dan lebih dari seratus
ribu uji yang masing-masing harus lulus.

Suite dipindahkan ke `suite(hw, pass)` di `init` tanpa diubah, dengan satu pengecualian yang jujur:
uji persistensi D01 menulis "survived the reboot" hanya pada putaran pertama; pada putaran
berikutnya yang benar adalah "is still there".

**2. Fault injection: yang sudah ada di suite, ditambah putaran chaos dengan waktu acak.** Suite
sendiri sudah menyuntikkan fault di setiap putaran: dua belas jenis eksepsi CPU di ring 3, proses
dibunuh saat `sleep`/`recv`/`wait`/`wait_any`, worker yang crash di bawah sesi, agent yang hilang
di bawah broker, TLS dengan sertifikat buruk, record yang diubah, aliran yang dipotong, penyedia
cloud yang macet, overload, bermusuhan, dan melampaui budget, layanan jaringan dibunuh saat koneksi
terbuka. Semua itu membunuh atau merusak di titik yang **dipilih uji**.

Setelah setiap putaran, putaran chaos menjalankan beban yang tidak pernah berakhir sendiri
(`bin/churn`: dua penulis berkas, pemeta memori, pengoper channel, pemanggil spawn) ditambah
transfer echo lewat `spacenet`, membiarkannya bekerja selama waktu acak (100–1600 ms), lalu
membunuh semuanya **dalam urutan acak, dengan jeda acak** — termasuk layanan jaringan di tengah
transfer. Proses mati di mana pun ia kebetulan berada: di dalam syscall, di antara dua syscall,
di tengah menulis ulang berkas, dengan objek memori terpetakan, dengan anak yang setengah
di-spawn. Seed acaknya diambil dari sumber entropi dan dicetak di awal run. Setelah itu:
setiap korban harus mati karena dibunuh (bukan karena gagal sendiri), koneksi yang layanannya
mati harus melaporkannya, lease perangkat jaringan harus bisa diambil lagi, dan instans
`spacenet` baru harus melayani.

**3. Memantau memori: setelah setiap putaran, mesin harus kembali ke keadaan putaran 1 —
tepat, bukan kira-kira.** Setelah putaran chaos tidak ada proses selain `init`. Maka enam angka
harus sama persis dengan yang tercatat setelah putaran 1: frame bebas, byte heap kernel, proses
hidup, thread hidup, byte heap `init` sendiri, dan jumlah handle `init`. Toleransi nol dipilih
dengan sengaja: kebocoran satu frame per putaran adalah kebocoran, dan ambang apa pun akan
menyembunyikannya. Putaran pertama menjadi acuan karena ia mengisi apa yang memang diisi sekali
(misalnya cache satu sektor FAT32). Jika ada anak dari proses yang dibunuh yang masih keluar,
pengukuran menunggu hingga 2 detik sebelum menyebutnya kebocoran.

Untuk melihat **seberapa dekat** mesin dengan kehabisan, bukan hanya di mana ia berakhir,
`KernelStats` kini membawa jumlah frame bebas terendah dan heap kernel tertinggi sejak boot. Setiap
putaran berakhir dengan satu baris berisi semua angka itu, yang dibaca harness menjadi
`memory.csv` — satu baris per putaran.

**4. Run pertama menemukan kebocoran, dan perbaikannya di kernel.** Dua putaran pertama yang pernah
dijalankan berakhir dengan `LEAK against pass 1: free frames -1`. Penyebabnya: kernel memberi
alamat untuk `mem_map`/`vmo_map` dari kursor yang hanya maju. Proses yang memetakan lalu
melepas tidak memegang apa pun, tetapi setiap 2 MiB alamat baru yang disentuhnya membutuhkan
satu page table baru — dan page table itu baru dibebaskan saat prosesnya keluar. Untuk `init`
itu satu frame setiap putaran; untuk layanan yang hidup berbulan-bulan, pertumbuhan tanpa batas.
Kini rentang yang sudah dilepas dipakai lagi (first fit, dengan satu halaman penjaga di kedua
sisi setiap pemetaan), dan page table yang sudah dibangun di sana ikut dipakai lagi. Dua uji
menahannya: selftest kernel (rentang yang dilepas diberikan lagi, penjaga ada) dan uji K03 baru
(50 siklus map/unmap 2 MiB, anonim dan memory object, berakhir dengan frame bebas yang sama
persis). Tanpa perbaikan, uji K03 itu kehilangan 100 frame dalam 50 siklus — satu page table per
pemetaan.

**5. Log otomatis, dan harness yang aman dijalankan berjam-jam di samping pekerjaan lain.**
`cargo xtask stress --minutes N` mem-boot mesin lab dengan `stress=Nm` dan menulis ke
`build/stress/`: log serial lengkap, log layanan lab, pcap, ringkasan, dan `memory.csv`. Karena run
berlangsung berjam-jam sementara pengembangan jalan terus:

- harness **menyalin dirinya** ke `build/stress/xtask` dan berjalan dari salinan itu — setiap koneksi
  lab menjalankan program itu lagi, dan rebuild di tengah run tidak boleh mengubah siapa yang
  menjawab;
- run punya **otoritas lab sendiri** (`SPACEOS_LAB_PKI`), disk data, dan variabel firmware sendiri,
  sehingga `cargo xtask test` di sebelahnya — yang membuat otoritas baru — tidak mencabut sertifikat
  dari bawah guest yang sedang berjalan;
- log dibaca **bertahap** (hanya yang baru, baris utuh saja); log run delapan jam mencapai ratusan
  MB dan membacanya ulang sepuluh kali per detik akan menjadi pekerjaan utama harness;
- stderr QEMU ditulis ke berkas, bukan pipe yang tidak dibaca — dalam hitungan jam pipe itu penuh,
  lalu QEMU berhenti di peringatan berikutnya;
- setiap 60 detik harness mencatat deskriptor berkas dan memori QEMU, serta jumlah layanan lab
  yang hidup: tamu yang sehat di dalam mesin yang pelan-pelan kehabisan deskriptor bukan tamu yang
  sehat.

**6. Hal-hal yang hanya muncul setelah jam ke-sekian.**

- *Uji yang menyalahkan guest atas jeda host.* Run delapan jam pertama gagal di putaran 11:
  `slept only 74 ms: the frame beat the sleep`. Uji NET "frame yang tiba saat penerimanya tidur
  membangunkannya" menyuruh pengirim menghitung mundur 150 ms **sebelum** penerima membaca jam, lalu
  menuntut penerima tidur 100 ms dari itu. Host sedang mengompilasi di sebelahnya; seluruh mesin
  diam ±76 ms di antara dua proses itu, dan uji menganggapnya kesalahan guest — padahal frame memang
  berangkat saat penerima tidur. Kini uji memakai saat frame benar-benar berangkat (dilaporkan
  pengirim): harus ≥ 20 ms setelah penerima mulai tidur, dan pengirim menunggu 300 ms. Yang
  dibuktikan tidak berubah: penerima tidur saat frame tiba dan bangun ≤ 100 ms setelahnya (2 ms di
  sini). Run dimulai ulang dari awal; yang gagal tidak dipakai sebagai bukti. Harness kini juga
  menjalankan dirinya dengan `nice -n -10`, sehingga pekerjaan lain di host tidak membuat vCPU guest
  menunggu.

- *Port yang dipakai ulang.* Seribu putaran berarti lebih dari seratus ribu koneksi TCP; port lokal
  berputar beberapa kali. Analisis pcap kini memperlakukan SYN dari guest dengan nomor urut awal
  yang berbeda sebagai koneksi baru, sehingga sopan-santun koneksi kedua dinilai sendiri (uji
  unit: koneksi kedua yang mengabaikan FIN tetap tertangkap walaupun yang pertama sopan).
- *Koneksi yatim.* Layanan jaringan yang dibunuh tidak bisa menutup koneksinya. Di sisi lab, proses
  echo yang menunggu byte yang tidak akan pernah datang akan menumpuk — satu proses dan satu
  deskriptor di QEMU per pembunuhan, selamanya. Layanan echo kini menutup setelah 30 detik sepi
  dan mencatatnya.
- *Jawaban untuk koneksi yatim.* Run kedua gagal di putaran 4: `FAIL NET: destinations off the
  allowlist are refused before a frame leaves: 1 frame(s) left the device while refusing`. Uji itu
  membaca penghitung perangkat sebelum dan sesudah penolakan. Frame yang terhitung bukan ulah
  penolakan: koneksi milik `spacenet` yang dibunuh di putaran chaos sebelumnya masih hidup di sisi
  peer, peer mengirim ulang, dan `spacenet` yang baru menjawabnya dengan RST — benar menurut TCP,
  dan kebetulan jatuh di jendela ukur. Kini hitungan yang diambil saat **tidak ada** frame masuk
  yang menentukan; hitungan yang disertai frame masuk diulang, sampai lima kali. Frame yang memang
  disebabkan penolakan muncul di setiap percobaan, jadi uji tidak menjadi lunak. Uji "frame yang
  tiba saat penerimanya tidur" dengan alasan yang sama tidur sampai jawaban gateway datang dan
  menyisihkan frame lain yang ikut membangunkannya (dan mengatakan berapa).
- *FIN yang datang setelah pembunuhan.* Di tengah pekerjaan ini `storage-reboot` sekali gagal di
  pemeriksaan kabel: `FIN never acknowledged 10.0.2.53:53`. Uji NET "layanan jaringan yang dibunuh
  bisa diganti" membunuh `spacenet` beberapa milidetik setelah pencarian nama; separuh penutupan
  koneksi DNS dari server datang setelah itu dan tidak ada yang menjawabnya. Itu koneksi kedua yang
  bukan pokok uji tersebut, jadi uji kini menunggu 200 ms sebelum membunuh; pemeriksaan kabel tetap
  menghitung FIN yang tidak dijawab di setiap skenario.
- *FIN untuk koneksi yang ditinggalkan.* CI sekali gagal di skenario `stress` pendek: `the guest
  never acknowledged the FIN of 1 connection(s) … 10.0.2.15:56127 <- 10.0.2.101:7`, padahal guest
  sendiri menulis `STRESS PASSED`. Itu koneksi echo milik `spacenet` yang dibunuh di putaran chaos:
  layanan echo menyerah setelah 30 detik sepi dan menutup, dan saat FIN-nya datang tidak ada
  layanan jaringan yang berjalan (uji pemulihan menjalankan `spacenet` sebentar lalu `QUIT`), jadi
  tidak ada yang bisa menjawab — mesin tanpa layanan jaringan sama dengan kabel yang dicabut. Di
  run panjang, layanan berikutnya menjawab FIN yang diulang itu dengan RST; di run pendek boot bisa
  selesai lebih dulu. Harness kini membedakan: FIN yang tidak dijawab di koneksi yang **sudah
  ≥ 20 detik tidak disentuh guest dan tidak pernah ditutupnya** dilaporkan sebagai koneksi yang
  ditinggalkan; FIN yang tidak dijawab di koneksi lain tetap gagal — di setiap skenario, dan kini
  juga di run stress panjang. Direproduksi dengan guest yang menunggu 45 detik tanpa layanan
  jaringan setelah putaran chaos terakhir: harness lama gagal pada rekaman itu (`FINs never
  acknowledged by the guest: 10.0.2.15:55846 <- 10.0.2.101:7`, FIN datang 30,8 detik setelah
  segmen terakhir guest), yang baru lulus dan menyebut koneksi itu; rekaman bug TIME-WAIT (24
  koneksi, ADR-0016) tetap gagal utuh.
- *Antrean yang tumbuh di beberapa CPU.* Run bukti pertama setelah SMP (ADR-0024) gagal di
  skenario `stress` pendek: `pass 2: LEAK against pass 1: kernel heap bytes +32`. Tidak ada objek
  yang hilang: +32 byte adalah tepat satu langkah antrean pointer yang tumbuh dari 4 ke 8 slot, dan
  run queue penjadwal tumbuh pada saat pertama kali lebih dari empat thread siap bersamaan — di satu
  CPU itu selalu terjadi di putaran pertama, di empat CPU bisa di putaran mana pun. Uji benar
  menolaknya: heap yang tidak kembali tetap heap yang tidak kembali. Kini run queue dan daftar
  sleeper diberi ruang untuk 256 thread saat boot (ADR-0024 keputusan 10), jadi ukuran heap tidak
  lagi bergantung pada kapan puncak konkurensi terjadi; empat run `stress` berturut-turut dan run
  bukti sesudahnya lulus tanpa kebocoran.
- *`SYS_CMDLINE`.* `init` perlu membaca `stress=`. Command line kernel kini bisa dibaca lewat handle
  root dengan hak `STATS` (cara kernel dikonfigurasi bukan urusan program yang tidak diberi hak
  itu). K02 menuntut isinya utuh, buffer pendek mendapat awalnya dan tetap tahu panjang aslinya,
  alamat kernel ditolak, dan handle tanpa `STATS` ditolak. Skenario `acceptance` kini boot dengan
  `init=bin/init` (bawaan yang diucapkan), dan uji memeriksa bahwa program yang disebut `init=`
  memang yang sedang berjalan.

**7. Versi pendeknya ikut di setiap run uji.** Skenario `stress` (`stress=2`) masuk ke
`cargo xtask test`: dua putaran penuh plus chaos, memori kembali tepat ke putaran 1. Kebocoran
seperti di butir 4 kini tertangkap di CI, bukan setelah delapan jam.

## Bukti

Run delapan jam pada commit `d8693de` dimulai 2026-09-30 00:43 UTC dan masih berjalan saat
ADR ini di-commit; ringkasan dan CSV-nya masuk ke `docs/evidence/` ketika selesai, dan baris
pertama tabel ini baru berlaku sejak saat itu. Yang sudah ada: skenario `stress` pendek lulus di
setiap `cargo xtask test`.

| Klaim | Bukti |
|---|---|
| 8 jam tanpa panic, setiap putaran lulus | `docs/evidence/stress-summary.txt` (catatan guest sendiri dan pemeriksaan harness), `docs/evidence/stress-memory.csv` |
| Memori kembali tepat setelah setiap putaran | enam angka di setiap baris `memory.csv` sama dengan baris pertama; `init` sendiri menandai `LEAK` bila tidak |
| Fault injection terjadi | setiap putaran: baris `pass N chaos: … killed after … ms: …` dengan urutan dan waktu pembunuhan, termasuk `spacenet at … ms (mid-transfer …)`; kutipan log di `docs/evidence/stress-first-pass.log` dan `stress-last-pass.log` |
| Host tidak ikut bocor | baris `host:` di ringkasan: deskriptor dan memori QEMU, layanan lab hidup |

## Gigi

Setiap pelemahan dijalankan sendiri-sendiri terhadap skenario `acceptance` atau `stress`:

| Pelemahan | Akibat |
|---|---|
| Kursor alamat yang hanya maju (`paging.rs` sebelum ADR ini), selftest baru | kernel panic saat boot: `a freed range was not reused` |
| Kursor yang sama, selftest lama | `acceptance`: `FAIL K03: … leak over 50 cycles: frames 2090101 -> 2090001`; `stress`: `pass 2: 2 test(s) failed; LEAK against pass 1: free frames -107` |
| Beban chaos yang berhenti sendiri (`churn mem` keluar setelah 20 putaran) | `pass 2 chaos FAILED: mem ended with Ok(ExitStatus { … code: 99 … }) before it was killed` |
| `init` tidak menutup channel kendali korban chaos | `LEAK against pass 1: kernel heap bytes +3160, init handles +5` |

Pemeriksaan lain di putaran chaos — koneksi layanan yang mati harus berhenti memberi data
(`a connection of the killed service kept delivering data`), jaringan harus kembali (`the network
did not come back`) — tidak dilemahkan secara terpisah.

## Konsekuensi

- Stabilitas kini diukur pada hal yang sama dengan yang diterima: suite penerimaan penuh, bukan beban
  sintetis.
- Kernel memakai ulang alamat user yang dilepas. Pointer basi di program user kini bisa mengenai
  pemetaan baru, bukan fault — seperti di sistem lain; halaman penjaga tetap memisahkan pemetaan
  yang hidup.
- Batas yang jujur: sampai ADR-0024 stress ini berjalan di satu CPU; sejak itu di empat, jadi
  konkurensi antar-CPU ikut diuji (skenario `stress` pendek di setiap run uji); QEMU/TCG,
  bukan perangkat fisik; perangkat keras yang *mengembalikan error* (disk yang gagal membaca) belum
  disuntikkan; tekanan memori sampai habis tidak diuji di sini (K03 menguji kuota, bukan kehabisan
  RAM mesin).
