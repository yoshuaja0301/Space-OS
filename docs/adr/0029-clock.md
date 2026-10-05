# ADR-0029 — Waktu dari counter, bukan dari hitungan tick

- Status: diterima
- Tanggal: 2026-10-05
- Konteks: PRD §7 MVP stabilitas ("8 jam tanpa panic"), ADR-0019 (uji stabilitas), ADR-0024 (SMP:
  CPU boot memegang waktu), ADR-0028 (AArch64, generic timer)

## Konteks

Uptime kernel — `sched::uptime_ms`, syscall `TICKS`, tenggat sleeper, dan jam dinding (waktu RTC
saat boot ditambah uptime) — adalah jumlah interrupt tick yang diterima CPU boot. Run stress
delapan jam pada `b781c79` memperlihatkan akibatnya: setelah 3 jam 2 menit di host, guest baru
menghitung 2 jam 8 menit. Jam guest berjalan 0,70 kali jam host, dan rasionya tetap dari awal sampai
akhir (putaran 50 dimulai pada 1656 s guest / 2328 s host, putaran 227 pada 7532 s / 10718 s); run
`fa734f5` sebelumnya sama (0,71).

Timer yang diemulasikan QEMU TCG — PIT di x86-64, generic timer di AArch64 — dinaikkan dari thread
QEMU. Bila vCPU CPU boot belum mengambil interrupt sebelum periode berikutnya dimulai, dua tick
menjadi satu interrupt: PIT tidak punya antrean, dan `timer::rearm` AArch64 sengaja melompat maju
alih-alih menembakkan rentetan. Di host empat inti yang menjalankan empat vCPU, tiga dari sepuluh
tick hilang. Perangkat keras sungguhan jarang kehilangan tick, tetapi mesin virtual di host yang
sibuk mengalaminya setiap hari.

Akibatnya bukan hanya angka di log: `sleep(100)` tidur 143 ms waktu nyata, batas waktu jaringan
memanjang, jam dinding guest tertinggal 2,4 jam setelah delapan jam (jam itu dipakai untuk menilai
masa berlaku sertifikat TLS), dan run stress yang meminta 480 menit tidak bisa selesai: guest baru
mencapai 480 menitnya pada jam ke-11,4 host, sementara harness memotong pada 8 jam 20 menit dan
menyebutnya gagal. Run itu dihentikan di putaran 233 — tanpa satu pun kegagalan — dan tidak dipakai
sebagai bukti.

## Keputusan

**1. Tick menentukan kapan melihat; counter menentukan berapa waktu yang lewat.**
`kernel/src/clock.rs`: tick CPU boot membaca counter yang berjalan apa pun yang dilakukan prosesor,
menjumlahkan selisihnya sejak bacaan sebelumnya (modulo lebar counter), dan jumlah tick menjadi
`total × TICK_HZ / frekuensi` (`sched::advance_ticks`). Tick yang terlambat atau tergabung tidak
lagi menghilangkan waktu: tick berikutnya melompat sejauh yang seharusnya, dan sleeper yang
tenggatnya terlewati bangun saat itu juga. Jumlah tick tidak pernah mundur. Kuantum tetap dihitung
per interrupt — itu soal giliran, bukan soal waktu.

**2. Counter per arsitektur.**
- x86-64: **timer ACPI PM**, 3,579545 MHz menurut spesifikasi, sehingga tidak ada yang perlu
  dikalibrasi. TSC lebih murah dibaca, tetapi lajunya harus diukur terhadap sesuatu, dan pengukuran
  di bawah TCG ikut terganggu jeda host — kesalahan 1 % berarti hampir lima menit dalam delapan jam.
  Port diambil dari FADT: `X_PM_TMR_BLK` bila berupa port I/O, kalau tidak `PM_TMR_BLK` dengan
  panjang 4; lebarnya 24 bit, atau 32 bit bila `TMR_VAL_EXT`. Counter ditolak bila ACPI-nya
  "hardware-reduced" atau bila port tidak berubah dalam satu milidetik.
- AArch64: hitungan virtual **generic timer** (`CNTVCT_EL0`) pada laju yang dinyatakan
  `CNTFRQ_EL0`, sekurang-kurangnya 56 bit.

**3. Tanpa counter, tetap seperti dulu, dan dikatakan.** Mesin tanpa timer PM menghitung interrupt
dan menyebutnya saat boot: `clock: counting tick interrupts (…); a lost tick is lost time`.

**4. Harness menahan jam guest ke jam host.** `cargo xtask stress` mencatat uptime guest di setiap
baris putaran beserta saat host melihat baris itu. Ringkasan menuliskan rasionya, dan beda lebih dari
2 % pada rentang sepuluh menit atau lebih adalah kegagalan run (`clock_check`). Skenario
`acceptance` dan `arm64-acceptance` serta setiap mesin `compat` menuntut baris `clock:` dengan
counter-nya, sehingga jalur counter yang patah tidak diam-diam kembali ke hitungan tick.

## Bukti

Semua dari sesi verifikasi 2026-10-05; angka lengkap di `docs/evidence/clock-summary.txt`.

| Klaim | Bukti |
|---|---|
| Sebelum keputusan ini jam guest tertinggal | run stress `b781c79` (hitungan tick): 71,2 % dari jam host pada putaran 50, 70,3 % pada putaran 233; run `fa734f5`: 0,71 |
| Kini jam guest sama dengan jam host, juga di bawah beban | `cargo xtask stress --minutes 12` sementara host menjalankan clippy dan uji unit: `clock: 662 s of guest time against 662 s of host time from pass 1 to pass 20 (100.00 %)`; 21 putaran, 2457 uji, 0 putaran gagal, 0 kebocoran |
| Setiap mesin x86-64 punya timer PM | ke-15 mesin `cargo xtask compat` — q35 dan i440fx, 1/2/4 CPU, disk virtio/SATA/NVMe/GPT terpasang, e1000/e1000e, tanpa disk, tanpa VGA — menulis `clock: ACPI PM timer at 3579545 Hz, 24 bits`, dan baris itu kini wajib (baris `jam` di `compat-summary.txt`) |
| AArch64 | `arm64-acceptance`: `clock: generic timer count at 62500000 Hz, 64 bits`, `ALL TESTS PASSED (108/108, 9 skipped)`; `arm64-poweroff` lulus |
| Yang lain tidak berubah | `cargo xtask test` 15/15 skenario, `ALL TESTS PASSED (117/117, 0 skipped)`, skenario `stress` pendek lulus; uji unit host lulus |

## Gigi

| Pelemahan | Akibat |
|---|---|
| Waktu dari hitungan tick (kode `b781c79`, sebelum keputusan ini) | run 8 jam tidak bisa selesai: jam guest 70 % dari host, sehingga 480 menit guest jatuh tiga jam setelah batas harness. Pemeriksaan baru menggagalkan angka run itu: `the guest's clock ran at 70.0 % of the host's from pass 49 to pass 226` (uji unit `clock_check_catches_a_slow_guest`, bersama jam yang 2,3 % terlalu cepat; jam dalam 2 % lulus, dan rentang di bawah sepuluh menit tidak dinilai) |

## Konsekuensi

- Uji yang mengukur waktu kini mengukur waktu nyata. Di host yang sibuk batas waktu guest habis
  lebih cepat dalam hitungan kerja: jam yang lambat dulu memberi kelonggaran sekitar 40 % yang tidak
  pernah disengaja.
- Timer PM 24 bit berputar setiap 4,7 detik. Bila CPU boot tidak menerima tick selama itu (mesin
  virtual yang dihentikan host), satu putaran penuh hilang dari jam; dengan 32 bit, setelah 20 menit.
- Membaca timer PM adalah akses port I/O di setiap tick CPU boot: sekitar satu mikrodetik di
  perangkat keras, 0,1 % dari tick 1 ms.
- Jam dinding tetap waktu RTC (x86-64) atau UEFI (AArch64) saat boot ditambah uptime; belum ada NTP,
  dan jam yang sudah salah sejak firmware tetap salah.
