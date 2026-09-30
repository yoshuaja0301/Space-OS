# ADR-0016 — Jaringan: kernel memindahkan frame, TCP/IP di user space, tujuan sebagai kapabilitas

- Status: diterima
- Tanggal: 2026-09-29
- Konteks: PRD Tahap 3 ("network"), PRD §5 (akses jaringan agent ditentukan kapabilitasnya,
  default deny), roadmap backlog butir 6, prasyarat I01

## Konteks

Roadmap menyebut tiga hal yang harus ada, berurutan, sebelum adapter cloud (I01) bisa diuji
secara jujur: driver virtio-net, stack TCP/IP, dan TLS. ADR ini mencatat keputusan untuk dua
yang pertama, dan satu aturan yang berlaku untuk ketiganya: **program tidak memegang jaringan,
program memegang izin ke tujuan tertentu.**

## Keputusan

**1. Kernel hanya memindahkan frame Ethernet.** Driver virtio-net 1.0 (PCI modern) memakai
transport yang sama dengan virtio-blk. Sisi user melihat empat syscall:

- `SYS_NET_OPEN` butuh hak root `NET` yang baru, dan memberi satu-satunya **lease** atas kartu
  (pemegang kedua mendapat `Busy`). Duplikat handle berbagi lease yang sama; lease lepas saat
  handle terakhir ditutup — termasuk saat pemiliknya dibunuh.
- `SYS_NET_SEND` / `SYS_NET_RECV` memindahkan satu frame 14–1514 byte, dengan hak `WRITE` /
  `READ` pada lease. Panjang, alamat buffer, dan hak diperiksa sebelum apa pun disentuh.
- `SYS_NET_INFO`: MAC, status link, MTU, dan penghitung frame milik perangkat sendiri — yang
  tidak bisa dipalsukan oleh proses mana pun.

Tanpa interupsi: INTx dimatikan dan tick timer 1 ms memeriksa ring penerima lalu
membangunkan penunggunya. Latensinya terbatas satu tick, dan tidak ada badai interupsi
level-triggered pada jalur yang dibagi. Ikut dari sini dua syscall umum: `SYS_WAIT_ANY`
(menunggu channel, proses, dan lease sekaligus, dengan batas waktu — multi-wait dari backlog
butir 4) dan `SYS_CLOCK_REALTIME` (RTC CMOS, dibaca sekali saat boot).

**2. TCP/IP hidup di satu proses user space: `spacenet`**, satu-satunya pemegang lease. Stack
yang dipakai adalah **smoltcp 0.12** (lisensi 0BSD): pustaka yang di-port, seperti yang
diizinkan PRD, bukan tulisan sendiri — alasan yang sama dengan TLS nanti. TCP yang ditulis dari
nol adalah permukaan luas untuk bug halus tanpa manfaat bagi tujuan OS ini. DHCP, ARP, IPv4 dan
TCP berasal dari smoltcp. Resolver DNS milik sendiri karena kecil: codec tanpa alokasi di
`spaceabi::dns`, diuji di host, berbicara **DNS lewat TCP** (RFC 7766 mewajibkan server
melayaninya).

**3. Tujuan adalah kapabilitas.** Program lain tidak pernah mendapat lease. Mereka mendapat
**sesi** dari operator (siapa pun yang menjalankan layanan), dan setiap sesi membawa
*allowlist* `host:port` — nama atau alamat, `*` untuk host mana pun, port 0 untuk port mana
pun, default kosong. Pemeriksaan dilakukan pada **nama seperti yang ditulis klien**, sebelum
pencarian DNS dan sebelum satu paket pun keluar. Nama tidak pernah cocok dengan alamat dan
sebaliknya: mengizinkan `echo.lab.test` bukan mengizinkan `10.0.2.101`. Setiap penolakan
dihitung, per sesi dan total.

**4. Satu koneksi adalah satu channel.** `CONNECT` membawa ujung channel baru; data mengalir
sebagai pesan ≤ 255 byte; menutup channel menutup koneksi — FIN bila semua yang diterima
sudah dibaca, RST bila belum, seperti menutup socket. Kendali aliran adalah antrean channel
itu sendiri: layanan membaca data klien hanya bila TCP punya ruang untuk satu pesan utuh, dan
menyerahkan data masuk hanya secepat antrean klien menerimanya (mundur 2 → 64 ms saat penuh).
Klien yang berhenti membaca hanya menghentikan koneksinya sendiri. Batas: 8 sesi, 8 koneksi,
4 per sesi, 12 socket yang sedang menutup.

**5. Jaringan lab tertutup, dengan pintu yang disebut satu per satu.** QEMU user-mode dengan
`restrict=on`: tidak ada host, tidak ada internet. DHCP-nya memberi alamat saja — tanpa router,
tanpa DNS. Layanan lab adalah aturan `guestfwd=…-cmd:xtask lab <nama>`: QEMU menjalankan
`xtask lab` untuk setiap koneksi, dengan koneksi itu sebagai stdin/stdout, jadi tidak ada port
host yang mendengarkan:

| Alamat | Layanan |
|---|---|
| 10.0.2.53:53 | DNS lewat TCP untuk zona lab (ditulis terpisah dari codec guest) |
| 10.0.2.101:7 | echo sampai klien selesai mengirim |
| 10.0.2.102:9 | pergi tanpa membaca → guest menerima RST |
| 10.0.2.101:8 | tidak diteruskan: QEMU menjawab SYN dengan RST (uji `Refused`) |
| 10.0.2.77 | tidak ada apa-apa (uji timeout) |

**6. Yang diperiksa adalah kabelnya, bukan hanya API-nya.** Setiap boot merekam pcap (QEMU
`filter-dump`). Harness menuntut setiap FIN dari peer diakui (ACK) oleh guest, dan melaporkan
jumlah koneksi, penutupan bersih, reset, dan segmen yang dikirim ulang peer. Pemeriksaan ini
menangkap bug nyata yang lolos dari semua uji di dalam guest: socket dibuang saat TIME-WAIT
sebelum ACK tertundanya berangkat — klien menerima data dan EOF dengan benar, tetapi 24 koneksi
dibiarkan menggantung di sisi peer, yang mengirim ulang 48 segmen. Perbaikannya: ACK tidak
ditunda, dan TIME-WAIT ditahan 250 ms. Satu-satunya pengecualian adalah koneksi yang ditinggalkan
layanan yang dibunuh — guest sudah diam ≥ 20 detik di koneksi itu dan tidak pernah menutupnya —
yang dilaporkan, bukan digagalkan (ADR-0019).

## Alternatif yang ditolak

- **TCP/IP di kernel.** Memperbesar basis tepercaya, dan bug stack menjadi crash kernel.
- **Driver di user space dengan MMIO dan DMA.** Tanpa IOMMU, proses yang memprogram DMA bisa
  menulis ke mana saja — tetap tepercaya, hanya lebih sulit dilihat (alasan yang sama dengan
  ADR-0007). Syscall frame menjaga DMA di kernel.
- **Allowlist berdasarkan alamat hasil DNS.** Jawaban DNS berubah; yang diizinkan operator
  adalah nama.
- **virtio-net berbasis interupsi.** Polling 1 ms cukup untuk lab dan tidak butuh kerja sama
  dengan jalur INTx yang dibagi; MSI-X adalah pekerjaan terpisah.

## Bukti

Lima belas uji `NET` di skenario `acceptance` (enam belas sejak ADR-0018: koneksi yang ditutup
saat peer masih mengirim harus di-reset), dilewati pada mesin tanpa kartu
(`compat` `e1000-only`):

- driver: hak `NET` dan lease eksklusif; MAC/link/MTU; ARP dan ICMP echo ke gateway yang dibuat
  tangan (32, 512, dan 1472 byte muatan — frame 1514 byte penuh); frame yang tiba **saat
  penerimanya tidur** membangunkannya (tidur 151 ms, bangun 1 ms setelah frame berangkat);
  permintaan cacat ditolak;
- layanan: DHCP dan HELLO; DNS (termasuk CNAME dan NXDOMAIN); 64 KiB lewat echo TCP dan kembali
  utuh; empat tujuan di luar allowlist ditolak dengan **0 frame terkirim** menurut penghitung
  perangkat; tujuan yang diam habis waktunya; port yang tidak mendengarkan → `Refused` (QEMU
  menjawab SYN ke port lab yang tidak diteruskan dengan RST); RST di tengah koneksi → `Reset`; 20 koneksi tanpa
  kebocoran memori kernel; layanan dibunuh saat koneksi terbuka → klien diberi tahu, lease
  kembali, instans baru melayani dan `QUIT` keluar dengan kode 0.

Giginya terbukti: tanpa bangun dari tick, uji tidur gagal (`woken 2850 ms after the frame
left`); dengan satu byte frame masuk dibalik, uji ICMP gagal (`ICMP checksum does not verify`);
dengan allowlist yang mengabaikan port, port lain dari nama yang diizinkan lolos (`echo.lab.test:8:
connection refused, expected Denied`); dengan RST dilaporkan sebagai akhir aliran, uji reset gagal
(`read gave Ok(0), expected Reset`); rekaman dari build dengan bug TIME-WAIT gagal di pemeriksaan
pcap.

## Konsekuensi

- I01 kini hanya terhalang TLS (dan adapternya sendiri).
- Belum ada: IPv6; UDP untuk klien; socket yang mendengarkan; DNS lewat UDP dan cache DNS;
  lebih dari satu kartu; MSI-X; e1000.
- smoltcp membatasi permintaan ARP satu per detik **untuk seluruh antarmuka**: koneksi pertama ke
  host yang belum dikenal bisa menunggu hingga satu detik bila host lain baru dicari. Di lab,
  tempat setiap layanan punya alamat sendiri, ini terlihat sebagai `connected … in 1007 ms`.
- TIME-WAIT 250 ms, bukan 2 MSL: FIN yang diulang setelah itu dijawab RST oleh smoltcp.
- Perpanjangan sewa DHCP (sewa QEMU 24 jam) belum teruji.
