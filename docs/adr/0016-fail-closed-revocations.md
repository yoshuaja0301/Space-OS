# ADR-0016 — Revokasi gagal tertutup saat penyimpanan bermasalah

- Status: diterima
- Tanggal: 2026-09-29
- Persyaratan: PRD §3, §5, §7 L02 dan skenario kegagalan §9
- Pemilik teknis: layanan `spacelink`; pemilik bukti: penggerak uji guest `init`
- Memperbarui ADR-0013 dan ADR-0015 untuk penyimpanan revokasi.

## Masalah yang dibuktikan

Loader lama mengabaikan kegagalan baca, UTF-8 rusak, dan ukuran berlebih. `HELLO`
tetap berhasil. Kegagalan menyimpan `REVOKE` atau `FORGET` hanya mencetak peringatan.
Tiga uji negatif di guest membuktikan respons sukses ketika seharusnya `Invalid`
atau `Denied`. Selain itu, `fs_create` mengosongkan file sebelum `fs_write`: error
tulis biasa dapat meninggalkan daftar kosong yang tampak sah setelah restart.

## Keputusan

1. `HELLO` membuang indeks dan bundle lama, lalu memuat serta memvalidasi seluruh
   daftar revokasi sebelum membuka akses. Kesalahan membuka, membaca, memvalidasi,
   atau mengalokasikan daftar dikembalikan ke operator; seluruh retrieval ditolak.
   Operator dapat memperbaiki store dan mengulangi `HELLO` pada proses yang sama.
2. Daftar dibatasi 16 path, masing-masing maksimum 48 byte, ASCII, absolut, tanpa
   komponen kosong, `.` atau `..`, backslash, atau karakter kontrol. Akhir setiap
   record wajib newline. Path indeks mengikuti aturan yang sama agar alias tidak
   melewati pencocokan revokasi. Kapasitas penuh menghasilkan `Quota`.
3. `REVOKE` memblokir dokumen dan membuang bundle sebelum menyimpan. Error simpan
   diteruskan, bukan diubah menjadi sukses. Larangan dalam proses tetap berlaku;
   reattach tidak boleh memuat daftar disk yang menghilangkan larangan tersebut.
   `FORGET` baru menghapus daftar memori setelah penyimpanan berhasil.
4. File teks `/spaceos/var/revoked.txt` dipertahankan. File pendamping
   `/spaceos/var/revoked.txn` kosong selama penggantian; sesudah tulisan lengkap,
   file itu berisi tepat 32 byte SHA-256 seluruh file teks. Urutan operasi:
   kosongkan marker → ganti file teks → tulis digest commit. Masing-masing operasi
   memakai jalur create/write/flush dari ADR-0015.
5. Marker kosong, ukuran salah, digest tidak cocok, atau file teks hilang ketika
   marker ada membuat layanan menolak akses. Ini menjaga keadaan tertutup saat
   proses berhenti atau penulisan gagal sesudah truncation. Tidak ada fallback ke
   daftar kosong atau snapshot lama. File teks lama tanpa marker tetap dapat
   dibaca; penyimpanan berikutnya menambahkan marker.

## Bukti penerimaan

`user/init/src/link_security.rs` berjalan di guest melalui `cargo xtask test`:

- UTF-8 rusak, record terpotong, path relatif, dan store terlalu besar ditolak;
- reattach dengan capability tanpa hak baca menghapus akses ke bundle yang sudah
  berisi dokumen sensitif; perbaikan store dan reattach memulihkan layanan;
- capability read-only membuat `REVOKE` dan `FORGET` mengembalikan error; indeks
  ulang dan reattach tidak membuka dokumen yang sudah diblokir dalam proses;
- proses baru menerima dua keadaan kegagalan nyata pada disk: marker kosong
  sesudah file teks dikosongkan, serta marker commit lama dengan file teks terpotong.
  Keduanya menolak retrieval; snapshot yang diperbaiki dapat dimuat kembali.

Skenario terakhir menyimulasikan keadaan disk pada batas kegagalan melalui syscall
file nyata, bukan memutus listrik atau menyuntik error perangkat. Uji reboot dan
matriks kompatibilitas tetap dijalankan untuk regresi.

## Batas

Marker ini bukan jurnal FAT32 atau cadangan data. Store yang tidak selesai memerlukan
perbaikan operator dengan daftar revokasi yang benar; layanan tidak menebaknya.
Jaminan durabilitas tetap bergantung pada flush perangkat. Tanpa jurnal filesystem,
kerusakan metadata akibat kehilangan daya tetap di luar jaminan ini.

Digest mendeteksi inkonsistensi, bukan autentikasi terhadap pemegang `FS_WRITE`.
Penghapusan kedua file tidak dapat dibedakan dari instalasi pertama dalam format
lama. L01 (lifecycle indeks inkremental), L03 (anggaran tokenizer model), dan
layanan izin multi-identitas belum diselesaikan oleh ADR ini.
