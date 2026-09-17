# ADR-0014 — Format paket, autentikasi, dan rollback

- Status: diterima
- Tanggal: 2026-09-16
- Konteks: PRD §7 P01, ADR-0007 (volume read-only), ADR-0012 (broker dan audit)

## Konteks

P01 meminta paket bertanda tangan dan rollback. Dua hal di lingkungan ini
membatasi bentuk yang jujur:

1. **Tidak ada pustaka kripto yang teraudit.** Menulis sendiri Ed25519 (aritmetika
   medan hingga, pembalikan modular, decoding titik) berarti menaruh kepercayaan
   pada kode kripto yang baru ditulis tanpa review — lebih buruk daripada mengaku
   memakai primitif yang lebih sederhana.
2. **Volume ter-mount read-only** (ADR-0007). Tidak ada tempat untuk memasang
   paket secara persisten.

Keduanya tidak menghalangi bagian yang sebenarnya diuji P01: apakah paket yang
tidak sah **ditolak**, apakah penolakan itu **tidak mengubah apa pun**, dan apakah
rollback benar-benar mengembalikan versi sebelumnya.

## Keputusan

**Autentikasi memakai HMAC-SHA256, bukan tanda tangan kunci publik.** HMAC
ditentukan dalam beberapa baris di atas SHA-256 yang sudah diverifikasi guest
terhadap vektor FIPS 180-4, dan implementasinya diuji di dalam guest terhadap
**vektor RFC 4231** — jadi kebenarannya tidak bersandar pada "host dan guest
sepakat" (yang akan lolos kalau keduanya salah dengan cara yang sama).

Konsekuensi yang harus dinyatakan terang-terangan: kunci rilis ada di dalam image
(`spaceabi::pkg::RELEASE_KEY`). Siapa pun yang bisa membaca image bisa membuat
paket yang sah. Itu memberi **integritas dan autentisitas terhadap pihak yang
tidak punya kunci** — cukup untuk menangkap unduhan rusak, paket yang diutak-atik
di tengah jalan, dan paket dari build lain — tetapi **bukan** distribusi tepercaya.
Kunci publik adalah yang membuat kunci penanda tangan tidak perlu ikut di dalam
image; itu langkah berikutnya, bukan yang ini.

**Header menandatangani dirinya beserta digest payload.** MAC menutupi seluruh
header sampai sebelum field MAC: magic, format, nama, versi, panjang payload, dan
SHA-256 payload. Payload sendiri tidak ikut di-MAC — ia diikat lewat digest-nya.
Satu `const _: () = assert!(SIGNED_BYTES == offset_of!(Header, mac))` menjaga
definisi itu tidak melenceng kalau ada field baru.

**Urutan pemeriksaan adalah bagian dari kontrak**: bentuk → autentikasi → isi.
Paket palsu tidak pernah di-hash seolah-olah tepercaya, dan paket terpotong tidak
pernah dilaporkan sebagai kegagalan MAC. Penolakan menyebut **alasannya**
(`Format`, `Truncated`, `Payload`, `Mac`, `Fields`), karena "paket buruk" tidak
memberi tahu operator apakah unduhannya rusak atau ada yang memalsukan.

**Rollback adalah riwayat, bukan bendera.** Layanan menyimpan payload versi-versi
sebelumnya (maksimum 4) dan `ROLLBACK` menurunkan versi aktif ke entri sebelumnya.
Artinya kembali ke versi lama tidak berarti membaca ulang berkas yang bisa saja
sudah diganti. Memasang versi yang **tidak lebih baru** ditolak: itulah gunanya
`ROLLBACK`, dan aturan ini menjaga riwayat tetap menaik.

**Penolakan tidak mengubah apa pun.** Versi aktif, riwayat, dan payload baru
disentuh setelah paket lolos seluruh verifikasi. Uji memasang empat paket bermasalah
berturut-turut dan menuntut versi aktif tetap 2 sesudahnya.

## Konsekuensi

- Store ada di memori; instalasi tidak bertahan melewati reboot. Persistensi
  menunggu penyimpanan yang bisa ditulis.
- Riwayat dibatasi 4 versi; entri tertua dibuang saat penuh, jadi rollback punya
  kedalaman terbatas.
- Payload dibatasi 64 KiB dan paket tidak punya struktur internal (bukan arsip):
  "memasang" berarti menyimpan payload yang sudah terverifikasi, bukan membongkar
  berkas ke sistem berkas.
- Tidak ada dependensi, versi minimum, atau hook pra/pasca instalasi.
- Rotasi kunci tidak ada: satu kunci, dipatok di sumber.
