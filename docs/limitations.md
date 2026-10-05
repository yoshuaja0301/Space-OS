# Keterbatasan dan batas yang diketahui (milestone tahap 1–5, jaringan tahap 3)

Daftar ini adalah bagian wajib setiap milestone (PRD §8). "Belum ada" berarti tidak ada kode, bukan "hampir".

## Kernel

- **SMP tanpa IOAPIC** (ADR-0024): semua CPU yang tercantum *enabled* di MADT menjalankan thread (paling banyak 64), tetapi IRQ perangkat hanya sampai ke CPU boot lewat PIC; virtio dipoll oleh tick PIT. Satu run queue global tanpa afinitas atau penyeimbangan per-CPU. CPU yang tidak menjawab start-up IPI dibiarkan parkir dan dilaporkan.
- **Satu thread per proses**; tidak ada `thread_create`. Penjadwal SMP bersandar pada ini untuk hidup tanpa TLB shootdown (ADR-0024): thread ganda berarti shootdown lebih dulu.
- **Pesan IPC ≤ 256 byte + 1 handle**; data besar memakai memory object (`VMO_CREATE`/`VMO_MAP`, tahap 4). Belum ada `VMO_UNMAP` khusus: pemetaan dilepas lewat `mem_unmap` dengan alamat dan panjang yang sama.
- **Memory object tidak dapat diperbesar, dipotong, atau dipetakan sebagian**; satu objek dipetakan utuh pada alamat yang dipilih kernel.
- `recv`/`wait` sendiri tidak punya timeout; yang punya adalah `SYS_WAIT_ANY` (maksimum 32 handle: channel, proses, lease jaringan), setelah itu `recv` non-blocking. `send` tidak pernah memblokir (antrean 64 → `WouldBlock`), dan belum ada cara **menunggu antrean peer punya ruang** — pengirim yang menemukan antrean penuh harus mencoba lagi (`spacenet` mundur 2 → 64 ms).
- **Stack user tetap 64 KiB**, dipetakan penuh saat spawn; tidak ada demand paging atau pertumbuhan stack.
- **Heap kernel tetap 16 MiB**; kehabisan heap = panic (alloc error), bukan penolakan bertahap.
- **Kuota menghitung halaman user saja**; frame page-table dan objek kernel (thread, channel) belum dibebankan ke proses. Headroom heap kernel dan `try_reserve` mengubah kehabisan heap menjadi error syscall (`NoMemory`), tetapi satu proses masih dapat menghabiskan headroom bersama (ancaman PRD §5 "resource exhaustion" baru ditutup sebagian).
- Siklus referensi antar-channel (endpoint A dikirim lewat channel B dan endpoint B dikirim lewat channel A) tidak dideteksi dan bocor; siklus satu channel ditolak (`Invalid`).
- NMI bersarang (NMI kedua saat handler NMI belum selesai) merusak frame di stack IST NMI.
- Headroom heap kernel 1 MiB menolak alokasi yang dipicu user, tetapi fragmentasi ekstrem masih dapat membuat alokasi internal kernel gagal (panic).
- Hanya **PIC + PIT**; belum ada ACPI/LAPIC/IOAPIC/HPET; RSDP hanya diteruskan.
- Linear map hanya memuat RAM dan framebuffer; MMIO perangkat dipetakan uncached on demand (ADR-0010). Framebuffer sendiri masih write-back lewat linear map (cukup untuk QEMU; perangkat fisik memerlukan write-combining/PAT).
- Granularitas linear map 2 MiB: satu halaman besar yang sebagian RAM dan sebagian MMIO tetap dipetakan write-back seluruhnya. Pada q35/i440fx batas PCI hole sejajar 2 MiB sehingga tidak terjadi.
- Reklamasi memori `BOOTLOADER_RECLAIMABLE` dilakukan segera; UEFI runtime services tidak dipakai (region-nya dibiarkan RESERVED).
- Masukan konsol (IRQ1 keyboard, IRQ3 COM2) masuk ke satu ring buffer dan diambil user space lewat `SYS_CONSOLE_READ` di balik hak root `CONSOLE`; kernel tidak melakukan echo maupun line editing.

## Penyimpanan

- Driver blok dan FAT32 berada **di dalam kernel** (ADR-0007), bukan user-space; tanpa IOMMU, driver DMA tetap komponen tepercaya.
- FAT32 satu volume, tanpa cache blok, tanpa mount table; entri long-name dilewati sehingga berkas guest harus bernama 8.3. Menulis ada (ADR-0015) tetapi terbatas: **create-atau-kosongkan dan tulis**, tanpa hapus berkas, tanpa buat direktori, tanpa nama panjang, dan **tanpa jurnal** — kehilangan daya di tengah tulisan bisa meninggalkan FAT dan entri direktori tidak sinkron. Yang bisa ditulis hanya volume data berlabel `SPACEDATA`; `block` membatasi setiap permintaan ke volume itu, jadi ESP tempat boot — di disk lain atau partisi lain — tidak terjangkau (ADR-0025).
- VirtIO memakai polling, satu permintaan pada satu waktu, tanpa interrupt; perangkat yang macet menghasilkan error setelah batas polling, bukan hang, tetapi batas itu membekukan CPU selama beberapa saat.
- Disk: virtio-blk, SATA lewat AHCI, dan NVMe (ADR-0025). Belum ada IDE/PATA (ESP di `i440fx` tetap tidak terlihat), USB, SCSI/virtio-scsi atau RAID; sektor harus 512 byte (disk 4Kn dan namespace NVMe berformat 4096 dilewati dengan pesan); satu perintah pada satu waktu per disk lewat buffer bounce 32 KiB; tanpa hot-plug. Tabel GPT dibaca tanpa memeriksa CRC-nya: label di boot sector yang memutuskan, dan partisi yang keluar dari disk tidak pernah dibaca.
- Hanya perangkat virtio-blk yang menawarkan kapabilitas modern (VIRTIO_F_VERSION_1). Perangkat transisional diterima karena juga menawarkannya; perangkat legacy murni diabaikan dengan pesan, bukan crash.
- Ukuran antrean yang dipakai adalah hasil negosiasi, maksimum 16, dan satu permintaan dipotong agar muat (`size - 2` halaman data, maksimum 8 = 32 KiB). Antrean < 3 deskriptor membuat perangkat ditolak.
- Permintaan yang melewati batas polling membuat perangkat di-reset dan dinonaktifkan permanen; tidak ada percobaan ulang atau pemulihan.

## Komputasi

- Satu koneksi per layanan compute (bootstrap channel-nya); belum ada broker koneksi atau multiplexing klien.
- Frame buffer dibebankan ke kuota **layanan**, bukan klien, jadi klien yang nakal dapat menghabiskan kuota layanannya.
- Submit bersifat sinkron dari sudut pandang klien (satu round trip IPC per operasi); belum ada batching, antrean asinkron, atau event completion.
- Matematika f32 diimplementasikan sendiri (tanpa libm); akurasinya memadai untuk model uji, bukan pustaka numerik umum.
- Backend hanya CPU skalar, tanpa SIMD, tanpa GPU/NPU.

## Inferensi

- Model referensi **tidak dilatih** (bobot dari PRNG ber-seed) dan hanya 115 ribu parameter; keluarannya tidak bermakna sebagai teks. A01 membuktikan pipeline-nya benar dan deterministik, bukan kualitas model.
- Belum ada model terlatih berlisensi, tokenizer sub-word, quantization, batching, atau sampling; dekode greedy dengan KV cache f32 penuh.
- Satu langkah dekode memakai 54 round trip IPC; cukup untuk model uji, bukan untuk throughput.
- Angka kecepatan berasal dari QEMU TCG, bukan perangkat fisik.

## Sesi dan antarmuka

- `spaceshell` mengawasi **satu** worker; belum ada tabel job atau penjadwalan beberapa job paralel.
- Stop kooperatif (ADR-0021) diperiksa `spaceai` sebelum setiap operasi compute, tetapi tidak selama pemuatan model (baca + SHA-256): Stop pada saat itu baru terlihat sesudahnya, dan bila pemuatan belum selesai dalam 1 detik, worker dibunuh. Job uji selalu dibunuh.
- Loop sesi memakai polling 2 ms. Waktu ADR-0011 ditulis belum ada multi-wait; sekarang ada (`SYS_WAIT_ANY`, ADR-0016), tetapi `spaceshell` belum dipindahkan ke sana.
- Masukan datang dari keyboard PS/2 (scan code set 1, tata letak US) dan COM2. Setiap tombol menjadi *event* untuk desktop (`SYS_INPUT_READ`), tetapi aliran **byte** untuk terminal hanya berisi karakter cetak, Enter, Backspace, Tab dan Esc (plus Enter dan `/` keypad): panah, F1–F12, Home/End tidak mengetik apa pun, dan tekanan dengan Ctrl/Alt/Super juga tidak — jadi tidak ada Ctrl+C sebagai byte `0x03`. Caps Lock, Num Lock dan lampu keyboard tidak ditangani. Shift palsu yang menyertai tombol panah dibuang; kalau didekode, keyboard akan tersangkut huruf besar.
- Line editor sesi hanya mengenal karakter cetak dan backspace; tidak ada riwayat perintah atau penyuntingan di tengah baris.
- Ring masukan konsol berisi 256 byte dan membuang yang paling tua saat penuh. Kehilangan itu tidak didiamkan: pembacaan berikutnya mendapat `DataLoss` sebelum byte yang selamat, sekali per episode, dan `spaceshell` membuang baris yang sedang diketik. Yang tidak ada adalah kendali aliran — tidak ada cara memberi tahu pengirim agar berhenti, jadi tempelan yang lebih cepat dari pembacanya tetap kehilangan byte, hanya saja dengan suara. Satu interupsi COM2 juga hanya mengambil 4096 byte; lebih dari itu menjeda interupsinya sampai pembacaan konsol berikutnya, jadi masukan tertunda, bukan hilang selamanya.
- Masukan konsol adalah **satu antrean global**, bukan milik satu proses: pembacaan bersifat merusak, jadi dua proses yang sama-sama memegang hak `CONSOLE` akan saling memakan ketikan. Modelnya adalah satu sesi memiliki konsol; belum ada pemilik konsol yang ditegakkan kernel.
- `SYS_FS_LIST` mengembalikan maksimum 64 entri per panggilan dan tidak punya kursor; direktori yang lebih besar terpotong tanpa cara melanjutkan. Entri `.` dan `..` ikut dikembalikan apa adanya.

## Desktop (ADR-0020)

- **Hanya keyboard.** Belum ada mouse atau pointer apa pun (PS/2 mouse, virtio-input, USB HID); jendela dipindah dan diubah
  ukurannya dengan pintasan, 32 px per langkah.
- **Hanya framebuffer UEFI (GOP)** yang ditinggalkan firmware, pada resolusi pilihan firmware (1280×800 di QEMU). Belum ada
  virtio-gpu, ganti mode, atau lebih dari satu layar. Mesin tanpa GOP tidak bisa menjalankan desktop (`display_open` →
  `NotFound`); uji desktop dilewati dengan alasan, dan sesi teks lewat serial tetap ada.
- Software rendering yang menyusun ulang **seluruh** layar setiap ada perubahan (tanpa damage region, tanpa vsync — tearing
  mungkin terlihat).
- Satu huruf: Noto Sans Mono bitmap 16 px, **Latin dasar saja**; karakter lain tampil sebagai `?`. Tanpa scaling.
- Kontras tinggi (Super+H) hanya untuk bingkai, top bar dan dock; isi aplikasi tetap di tema biasa. Kontras yang diperiksa saat build hanya untuk pasangan warna yang didaftarkan di kode; teks yang digambar klien di atas gambar (belum ada) tidak bisa diperiksa dengan cara itu.
- Aplikasinya sempit: terminal adalah sesi `spaceshell` dengan perintahnya (`help`, `status`, `ls`, `run`, `stop`, `clear`);
  file manager hanya **menelusuri** (belum membuka, menyalin atau menghapus); Agent Center menjalankan model
  referensi lewat `spaceai` (ADR-0021) dan worker uji — belum `spaceagent`, pilihan prompt atau model, batas
  token/waktu yang bisa diatur, atau job cloud (jadi baris biayanya selalu "tidak ada").
- Command Center (ADR-0022) hanya mengindeks `/spaceos/docs`, saat jendelanya dibuka: berkas yang berubah sesudahnya tidak terlihat sampai jendela dibuka lagi. Paling banyak lima hasil, cuplikan dari awal chunk, dan tidak ada revokasi dari desktop (layanannya hanya bisa membaca).
- Belum ada pengaturan model, notifikasi, login atau layar kunci; satu pengguna.
- Paling banyak 12 jendela (4 yang sedang dimulai); setiap klien dilayani paling banyak 16 pesan per putaran; judul dan
  deskripsi jendela paling panjang 180 byte.
- Ctrl+Alt+Delete hanya mematikan mesin bila desktop adalah sesinya (`init=bin/spacedesk`); desktop yang dijalankan proses
  lain menyerahkan keputusan itu kepada operatornya.

## Agent dan Tool Broker

- Tambalan agent mendarat di volume (ADR-0015) lewat broker, hanya di dalam workspace; berkas dibatasi 8 KiB, dan tulisan tidak berjurnal (lihat Penyimpanan).
- "Menguji" berarti satu check bawaan (`verify`) yang membandingkan hasil dengan berkas harapan. Belum ada runner uji umum — menjalankan proses atas nama agent berarti memberi broker hak `SPAWN`, dan itu belum dilakukan.
- Satu agent per broker; broker melayani agent sampai selesai sebelum menjawab operator lagi.
- Audit log dibatasi 64 entri dan hanya ada di memori: panggilan setelah itu tetap dilayani dan dihitung, tetapi tidak dicatat, dan seluruh log hilang saat broker keluar.
- Scope adalah satu direktori dengan kedalaman satu; belum ada beberapa scope, pola, atau hak per-berkas.

## SpaceLink

- Peringkat **leksikal**: jumlah kemunculan istilah kueri per chunk, seri dipecah oleh posisi. Tidak ada embedding, TF-IDF, stemming, atau tokenizer.
- Batas keras: 16 dokumen, 128 chunk, 8 KiB per dokumen, 192 byte per chunk, 8 entri per bundle.
- Indeks hanya ada di memori layanan dan dibangun ulang saat layanan mulai; **daftar revokasi ada di disk** (`/spaceos/var/revoked.txt`, ADR-0015) dan bertahan melewati matinya layanan maupun reboot.
- Kesegaran per berkas (`UPDATE`, ADR-0023) harus **diminta** oleh yang mengubah berkas: belum ada notifikasi perubahan berkas dari kernel, jadi berkas yang diubah tanpa `UPDATE` tetap memegang teks lamanya sampai `INDEX` berikutnya. Berkas baru hanya masuk lewat `INDEX`. `UPDATE` membaca seluruh berkas untuk membandingkan digest (belum ada waktu modifikasi di `SYS_FS_STAT`). Command Center belum memakainya.
- Kueri memindai seluruh chunk secara linear; belum ada indeks terbalik.
- Satu hasil per panggilan (dengan `total`), jadi menelusuri N hasil butuh N round trip; teks per balasan dipotong 128 byte (batas pesan IPC).
- Repo SpaceLink dari PRD §10 tidak tersedia di lingkungan ini; yang diimplementasikan adalah kontrak L01–L03, bukan mesin retrieval SpaceLink yang dimaksud PRD.

## Paket

- Autentikasi memakai **HMAC-SHA256**, bukan tanda tangan kunci publik, dan **kunci rilis ada di dalam image**. Siapa pun yang bisa membaca image bisa membuat paket yang sah; yang diberikan adalah integritas terhadap pihak tanpa kunci, bukan distribusi tepercaya (ADR-0014).
- Store paket ada di disk (`/spaceos/var/pkgstore.dat`, ADR-0015), ditulis utuh setelah setiap install dan rollback, tanpa jurnal: kehilangan daya di tengah tulisan bisa merusaknya, dan store yang rusak dibaca sebagai "tidak ada store".
- Riwayat rollback dibatasi 4 versi; yang tertua dibuang saat penuh.
- Payload maksimum 64 KiB dan paket tidak punya struktur internal (bukan arsip): "memasang" berarti menyimpan payload terverifikasi, bukan membongkar berkas.
- Tidak ada dependensi antar paket, batas versi minimum, hook pra/pasca instalasi, atau rotasi kunci.

## Recovery dan pemasangan (ADR-0027)

- Memasang = menulis image `cargo xtask disk-image` dengan `dd` dari sistem lain; belum ada installer yang berjalan di dalam Space OS (butuh tulis ke disk di luar volume data, yang ditolak lapisan `block`).
- Recovery tidak bisa memperbaiki bootloader, kernel atau initrd yang rusak: semuanya di ESP, yang tidak terjangkau dari dalam sistem.
- Hitungan boot ditulis firmware lewat driver FAT-nya sendiri; firmware tanpa driver untuk disk data berarti boot tidak dihitung (dan log mengatakannya).
- `poweroff` mematikan daya lewat ACPI S5 bila DSDT menulis `\_S5` sebagai paket (tanpa interpreter AML, tanpa `_PTS`); mesin yang tidak begitu berhenti di CPU, dan log boot mengatakannya. Belum ada `reboot`.

## Kompatibilitas

- Matriks `cargo xtask compat` (ADR-0010) mencakup lima belas konfigurasi QEMU: q35 dan i440fx, 1–4 vCPU (semuanya dipakai, ADR-0024), 2–8 GiB, `qemu64` dan `max`, virtio-blk modern/transisional/antrean kecil, disk data di SATA (AHCI) dan di NVMe, satu disk GPT terpasang (ESP + volume data), tanpa disk, kartu jaringan e1000 (82540EM) dan e1000e (82574L), kartu tanpa driver (rtl8139), tanpa VGA, dan VGA vmware. Semua mem-boot image yang sama.
- **Di luar cakupan**: perangkat keras fisik, IOAPIC/MSI (IRQ hanya ke CPU boot), boot legacy BIOS (hanya UEFI), firmware dengan 5-level paging (ditolak dengan pesan), disk selain virtio-blk, SATA (AHCI) dan NVMe, kartu jaringan selain virtio-net dan Intel e1000 (82540EM/82545EM/82574L; mesin `rtl8139-only` melewati uji jaringan), dan filesystem selain FAT32.
- Mesin tanpa disk melewati uji D01/A01 dan melaporkannya sebagai *skipped*; hitungannya terpisah dari yang lulus agar tidak terbaca seolah-olah dijalankan.

## Jaringan

- Driver jaringan (virtio-net dan Intel e1000, ADR-0026) ada **di dalam kernel** dan hanya memindahkan frame; TCP/IP ada di satu proses user space (`spacenet`, smoltcp — pustaka yang di-port, ADR-0016). Satu kartu, satu lease: pemegang lease adalah satu-satunya yang bisa memakai jaringan.
- Polling pada tick 1 ms, tanpa interupsi dan tanpa MSI-X: latensi menerima hingga satu tick, dan throughput terbatas oleh 32 buffer penerima dan satu frame per syscall.
- **IPv4 saja**; tidak ada IPv6. Klien TCP saja: tidak ada socket yang mendengarkan, tidak ada UDP untuk program lain. DNS hanya lewat TCP, tanpa cache; DNS lewat UDP belum ada.
- smoltcp membatasi permintaan ARP **satu per detik untuk seluruh antarmuka**: koneksi pertama ke host yang belum dikenal bisa tertunda hingga satu detik bila host lain baru dicari.
- TIME-WAIT dipersingkat menjadi 250 ms (bukan 2 MSL) agar socket yang menutup tidak menahan buffer 16 KiB berdetik-detik; FIN yang diulang peer setelah itu dijawab RST.
- Batas: 8 sesi, 8 allowlist entri per sesi, 8 koneksi (4 per sesi), pesan data 255 byte. Keadilan antar-sesi hanya sebatas batas per sesi itu.
- Jaringan lab tertutup (QEMU `restrict=on`): DHCP-nya tidak memberi router maupun DNS, dan semua layanan uji dibuat `xtask lab`. **Tidak ada uji terhadap internet.** Perpanjangan sewa DHCP (sewa QEMU 24 jam) belum teruji.

## TLS dan entropi

- TLS 1.3 saja (ADR-0017): tanpa TLS 1.2, tanpa resumption dan 0-RTT, tanpa sertifikat klien. Server yang hanya berbicara TLS 1.2 tidak bisa dicapai.
- **Tidak ada root CA bawaan dan belum ada trust store sistem**: setiap program menyerahkan sendiri otoritas yang dipercayanya. Di lab itu satu otoritas yang dibuat baru setiap build data disk. Tidak ada pemeriksaan pencabutan (CRL/OCSP) maupun Certificate Transparency.
- Penyedia kripto `rustls-rustcrypto` berlabel *alpha* dan belum diaudit sebagai satu kesatuan; primitifnya crate RustCrypto yang luas dipakai. Batas record AES-GCM yang tidak diberikannya dipasang oleh `spacetls`. RSA hanya dipakai untuk verifikasi (kunci publik), jadi advisory Marvin (RUSTSEC-2023-0071) tidak berlaku.
- Kripto berjalan **tanpa instruksi vektor**: unit FPU/SSE dimatikan karena kernel tidak menyimpan state-nya per thread. Handshake 40–70 ms di TCG; throughput dibatasi AES/ChaCha perangkat lunak dan pesan channel 255 byte.
- Waktu untuk memeriksa sertifikat berasal dari RTC yang dibaca sekali saat boot, tanpa sinkronisasi jaringan. Jam yang maju membuat sertifikat terlihat kedaluwarsa (gagal tertutup); jam yang mundur akan menerima sertifikat yang sudah kedaluwarsa sejak itu.
- Tanpa virtio-rng dan tanpa RDRAND tidak ada entropi sama sekali: `SYS_RANDOM` menjawab `NotFound` dan semua yang membutuhkan kunci dilewati. Tidak ada kumpulan entropi (pool) atau DRBG di kernel; setiap permintaan diisi langsung dari perangkat.

## Adapter cloud (I01)

- **Hanya diuji terhadap penyedia tiruan** di jaringan lab (`xtask lab cloud`, ADR-0018). Belum pernah berbicara dengan layanan sungguhan: tidak ada yang bisa dicapai dari lingkungan ini, dan untuk itu masih dibutuhkan trust store dengan root CA publik serta jaringan di luar `restrict=on`. Label integrasinya *experimental*.
- Bagian Messages API yang dipakai saja: satu giliran pengguna per ask (≤ 176 byte, satu pesan channel), tanpa riwayat percakapan dari klien, tanpa gambar atau dokumen, satu alat (`read_file`), tanpa tulis lewat model.
- Token dan biaya berasal dari laporan penyedia; reservasi memakai estimasi input yang sengaja berlebih (byte/3), jadi budget bisa menolak ask yang sebenarnya muat. Harga dimasukkan operator, bukan dibaca dari penyedia.
- Satu ask pada satu waktu untuk hingga 4 klien; ask yang panjang menahan yang lain.
- Kredensial dipegang di memori adapter dan tidak dihapus saat keluar. Di disk (`/spaceos/cred/cloud.key`) ia terbaca oleh siapa pun yang memegang `FS` atas volume — `init` dan broker; broker hanya melayani workspace. Belum ada penyimpanan kredensial terenkripsi.

## AArch64 (ADR-0028)

- Hanya diuji di QEMU `virt` (GICv3, Cortex-A72) dengan AAVMF; belum di papan fisik.
- IRQ perangkat (konsol PL011) hanya ke CPU boot; CPU lain dihentikan saat panic dengan SGI biasa, jadi CPU yang memegang spinlock dengan interrupt dimask berhenti setelah melepasnya (tanpa pseudo-NMI).
- Model memori ARM lebih lemah dari x86: penjadwal menyerahkan thread antar-CPU hanya lewat spinlock, tetapi TCG di host x86 tidak memperlihatkan pengurutan ulang yang hanya terjadi di perangkat keras ARM.
- Firmware harus berjalan di EL1 dan memakai indeks `MAIR_EL1` EDK2 (0 Device-nGnRnE, 3 write-back); firmware di EL2 — umum di papan fisik — ditolak bootloader dengan pesan. Hanya GICv3 (bukan GICv2).
- Belum ada keyboard (tanpa PS/2; virtio-input dan USB belum ditulis) dan belum ada framebuffer di `virt` (`ramfb`/virtio-gpu belum): desktop dan uji keyboard dilewati. Masukan konsol hanya lewat UART PL011.
- Pembagian dengan nol tidak menjebak di AArch64 dan single-step tidak bisa diminta dari EL0: dua uji K02 itu dilewati dengan alasan.
- DMA dianggap koheren dengan cache (benar untuk PCIe di `virt`); SoC yang DMA-nya tidak men-snoop cache butuh pemeliharaan cache yang belum ada. Penghalang `dsb sy` sebelum memberi tahu perangkat ada, tetapi di TCG tidak bisa dibuktikan perlu.
- Overflow stack kernel tidak punya stack terpisah untuk dilaporkan (x86-64 punya IST).

## Belum ada (tahap berikutnya)

- VirtIO input dan virtio-gpu, mouse (sisa tahap 3): masukan hanya keyboard PS/2 dan COM2, layar hanya framebuffer UEFI.
- Space Guard sebagai layanan, tanda tangan kunci publik untuk paket, adapter cloud terhadap penyedia sungguhan (5A).
- GPU (6).

## Verifikasi

- Semua bukti berasal dari **QEMU TCG** dengan profil ADR-0003 (x86-64) dan `virt` (AArch64, ADR-0028); belum pernah dicoba pada mesin fisik.
- Soak 100 boot (K01) dijalankan pada satu host; stress 8 jam (PRD §9) belum dijalankan.
- Tidak ada fuzzing syscall; uji negatif bersifat contoh, bukan eksplorasi acak.
