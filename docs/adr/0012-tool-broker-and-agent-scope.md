# ADR-0012 — Tool Broker, scope workspace, dan audit agent

- Status: diterima
- Tanggal: 2026-09-16
- Konteks: PRD §7 G01, PRD §5 (ancaman "agent keluar scope"), ADR-0004 (capability), ADR-0007 (ABI file)

## Konteks

G01 berbunyi: agent membaca, menambal dan menguji di dalam sebuah workspace, dan
akses di luar scope ditolak. Dua cara mengerjakannya:

1. Agent memegang kapabilitas file dan **berjanji** hanya menyentuh workspace.
2. Agent tidak memegang kapabilitas file sama sekali, dan setiap efeknya harus
   lewat proses lain yang memeriksanya.

Cara pertama menjadikan "tetap di dalam scope" sifat dari kode agent. Cara kedua
menjadikannya sifat dari **apa yang diserahkan** kepada agent. Hanya yang kedua
yang bisa diuji tanpa mempercayai agent.

## Keputusan

**Agent tidak diberi apa pun selain satu channel.** `bin/spaceagent` lahir dengan
bootstrap berupa endpoint channel ke broker. Panggilan `fs_open` miliknya ditolak
kernel (`Denied`) karena handle itu bukan root — hal pertama yang dilakukan agent
adalah membuktikan itu, supaya batasannya terlihat di log, bukan sekadar
didokumentasikan.

**`bin/spacebroker` memegang satu-satunya kapabilitas file**, itu pun dipersempit
ke `FS` saja: broker tidak bisa spawn, tidak bisa shutdown, tidak bisa membaca
statistik kernel. Ia melayani satu scope, `/spaceos/ws`.

**Aturan scope sempit dan tertutup**, bukan pencocokan awalan. Sebuah path
diterima hanya jika absolut, prefiks workspace cocok komponen-per-komponen (case
insensitive, karena FAT), tidak ada komponen `.` atau `..`, tidak ada komponen
kosong (`//`), dan paling banyak satu komponen setelah workspace. Semua yang
sering dipakai untuk keluar — `..`, `//`, awalan mirip (`/spaceos/wsx`), path
relatif — karena itu bukan "kasus khusus" melainkan path yang memang tidak valid
di sini. Uji menembakkan delapan bentuk sekaligus; dengan pemeriksaan awalan naif
(`path.starts_with(SCOPE)`) uji itu merah.

**Tambalan mendarat di overlay dalam memori.** Volume ter-mount read-only
(ADR-0007), jadi broker menyimpan berkas yang ditulis agent di memori dan
menyajikannya kembali pada pembacaan berikutnya. Agent tidak bisa membedakannya:
ia menulis, membaca ulang, dan check berjalan atas tampilan yang sudah ditambal.
Batasnya jelas: 4 berkas, 8 KiB per berkas, hilang saat broker keluar.

**Audit mencatat setiap panggilan, bukan hanya yang ditolak.** Setiap entri berisi
nomor urut, nama tool, path, dan verdict (`allowed`, `denied-scope`,
`denied-tool`, `failed`). `failed` sengaja dibedakan dari `denied`: berkas yang
tidak ada di dalam scope bukan pelanggaran, dan mencampurnya akan membuat log
tidak bisa dipakai menjawab "apakah agent pernah keluar scope".

**Audit milik operator, bukan agent.** `AUDIT`, `ATTACH` dan `QUIT` hanya
dilayani pada channel operator; dari sisi agent ketiganya `denied-tool` — dan
penolakan itu sendiri masuk audit.

## Konsekuensi

- Satu agent per broker, dan broker melayani agent sampai selesai sebelum
  menjawab operator lagi. Itu menjaga hanya ada satu titik blocking per fase;
  beberapa agent paralel memerlukan desain lain.
- "Menguji" berarti satu check bawaan (`verify`) yang membandingkan hasil tambalan
  dengan berkas harapan yang dibuat host. Belum ada runner uji umum: menjalankan
  proses atas nama agent berarti memberi broker hak `SPAWN`, dan itu keputusan
  terpisah yang belum diambil.
- Tambalan tidak bertahan melewati reboot. Menulis ke disk memerlukan FAT32 yang
  bisa menulis, yang belum ada.
- Audit dibatasi 64 entri; setelah itu panggilan tetap dilayani dan dihitung,
  tetapi tidak dicatat. Untuk jalur produksi log ini harus persisten dan tak
  terbatas.
- Broker bertahan ketika agent hilang: channel tertutup (`PeerClosed`) adalah
  akhir sesi yang normal, dilaporkan ke operator, bukan error.
