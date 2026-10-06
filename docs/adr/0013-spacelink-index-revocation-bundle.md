# ADR-0013 — SpaceLink: indeks, revokasi, dan context bundle

- Status: diterima
- Tanggal: 2026-09-16
- Konteks: PRD §7 L01–L03, ADR-0007 (ABI file), ADR-0012 (broker dan audit)

## Konteks

L01–L03 meminta SpaceLink bisa mengindeks korpus, mencabut (revoke) sebuah
dokumen, dan menghasilkan context bundle. PRD menandainya bergantung pada repo
SpaceLink yang tidak tersedia di sini (PRD §10). Yang tetap bisa dikerjakan — dan
yang sebenarnya menentukan apakah fitur ini layak dipercaya — adalah kontraknya:

- Bundle yang diberikan ke model **berasal dari mana**, dan bisakah pemanggil
  memeriksanya sendiri?
- Ketika sebuah dokumen dicabut, apakah ia benar-benar hilang, atau hanya turun
  peringkat?

Keduanya bisa dibuktikan tanpa repo SpaceLink, dan keduanya akan tetap berlaku
apa pun mesin pemeringkatnya nanti.

## Keputusan

**Provenance diverifikasi pemanggil, bukan dipercaya.** Setiap chunk yang
dikembalikan membawa path sumber, rentang byte (`offset`, `len`), dan SHA-256 atas
byte itu persis. Uji L01 membaca ulang rentang tersebut lewat `SYS_FS_READ`,
menghitung digest-nya sendiri, dan membandingkan. Kalau layanan berbohong soal
asal-usul, uji merah.

**Revokasi adalah kontrak, bukan pembersihan.** `REVOKE` menyimpan path ke daftar
revokasi **dan** membuang chunk dokumen itu dari memori. Daftar revokasi terpisah
dari indeks, sehingga `INDEX` ulang tidak menghidupkannya kembali: dokumen yang
dicabut dibaca sebagai nol chunk. Uji L02 memastikan tiga hal sekaligus — kueri
langsung (`embargo`), kueri umum yang dibagi dengan dokumen lain (`channel`), dan
bundle — tidak lagi memuatnya, lalu mengindeks ulang dan mengulang pemeriksaan.

**Bundle punya digest yang dapat direproduksi.** Digest bundle adalah SHA-256 atas
rangkaian digest chunk-nya, berurutan. Dua bundle identik persis ketika isinya
identik dan urutannya sama, jadi "bundle yang sama" bisa dinyatakan tanpa
membandingkan isinya. Anggaran byte ditegakkan: chunk yang tidak muat dilewati,
dan anggaran yang terlalu kecil menghasilkan bundle kosong, bukan error.

**Peringkat sengaja leksikal dan deterministik.** Skor = jumlah kemunculan tiap
istilah kueri di dalam chunk (case insensitive); seri dipecah oleh posisi chunk di
korpus. Tidak ada embedding, tidak ada TF-IDF, tidak ada tokenizer. Alasannya:
peringkat yang bergantung pada model membuat uji L03 ("bundle yang sama untuk
kueri yang sama") tidak bisa dipegang, sementara properti yang diuji di sini
bukan kualitas retrieval melainkan kontraknya.

**Layanan hanya memegang hak `FS`.** Sama seperti broker (ADR-0012): SpaceLink
tidak bisa spawn, tidak bisa shutdown, tidak bisa membaca statistik kernel.

## Konsekuensi

- Batas keras: 16 dokumen, 128 chunk, 8 KiB per dokumen, 8 entri per bundle,
  chunk 192 byte. Korpus yang lebih besar terpotong, dan pemotongannya dicetak.
- Indeks dan daftar revokasi ada di memori layanan; keduanya hilang saat layanan
  keluar. Revokasi yang bertahan melewati reboot memerlukan penyimpanan yang bisa
  ditulis, yang belum ada.
- Chunk disimpan lengkap di memori supaya kueri tidak menyentuh disk lagi. Itu
  membuat kueri murah dan indeks mahal; korpus besar memerlukan indeks terbalik,
  bukan pemindaian linear.
- Teks yang dikembalikan per balasan dipotong pada 128 byte (batas pesan IPC).
  Pemanggil yang butuh chunk utuh membacanya dari berkas memakai provenance —
  yang memang cara yang diinginkan.
- `QUERY` mengembalikan satu hasil per panggilan (dengan `total`), jadi menelusuri
  N hasil butuh N round trip. Cukup untuk korpus sebesar ini.
