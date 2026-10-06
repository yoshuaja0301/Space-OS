# ADR-0023 — SpaceLink: satu berkas yang berubah diindeks ulang sendirian

- Status: diterima
- Tanggal: 2026-09-30
- Konteks: PRD §9 (skenario ujung ke ujung kedua: "indeks workspace, ubah satu file, lalu minta
  ringkasan dengan sumber terbaru tanpa full rescan"; freshness SpaceLink p95 ≤ 2 detik untuk satu
  edit kecil), ADR-0013 (SpaceLink), ADR-0015 (penyimpanan yang bisa ditulis)

## Konteks

Sampai sekarang satu-satunya cara SpaceLink melihat perubahan adalah `INDEX` lagi: seluruh folder
dibaca ulang dan dipotong ulang. Itu "full rescan" yang disebut PRD, dan di antara dua `INDEX`,
hasil pencarian dan context bundle memakai teks lama — dengan provenance yang tidak lagi cocok dengan
disk.

## Keputusan

**1. `UPDATE` untuk satu path.** Permintaan baru membaca ulang **satu** dokumen yang sudah
terindeks — tidak ada yang lain — dan membandingkan SHA-256 seluruh isinya dengan yang dicatat saat
terakhir dibaca. Sama: tidak ada yang berubah. Berbeda: chunk lamanya diganti chunk baru **di
tempat yang sama**, sehingga urutan sisa indeks (dan karenanya peringkat untuk skor yang sama) tidak
bergeser, dan bundle yang dibangun sebelumnya dibuang karena menunjuk chunk menurut posisi. Untuk
path itu, `UPDATE` melakukan persis yang dilakukan `INDEX` untuk setiap path.

**2. Yang tidak bisa dibaca lagi tidak dijamin lagi.** Bila berkas itu tidak bisa dibaca —
misalnya tumbuh melewati 8 KiB — chunk-nya dibuang dan galat itulah jawabannya: teks lama dengan
digest yang tidak lagi cocok dengan disk lebih buruk daripada tidak ada jawaban. `UPDATE` berikutnya
yang berhasil memasukkannya kembali.

**3. Revokasi tetap menang.** Dokumen yang dicabut tidak dibaca oleh `UPDATE` sama sekali, dan tetap
tidak muncul. Path yang tidak terindeks → `NotFound`: `UPDATE` tidak menambah dokumen ke indeks.

**4. Jawabannya menyebut apa yang dibaca, tetapi uji tidak bergantung padanya.** `value` 1 bila
berubah, `value2` jumlah byte yang dibaca — ukuran dokumen itu, bukan korpus — `value3` jumlah chunk
di indeks, `total` jumlah chunk dokumen itu. Itu keterangan layanan tentang dirinya sendiri. Bukti
bahwa tidak ada full rescan datang dari luar: berkas kedua diubah pada saat yang sama tanpa disebut,
dan indeks harus tetap memegang teks **lamanya** sampai berkas itu disebut sendiri — full rescan
pasti membacanya.

**5. Siapa yang memanggil.** Kernel belum punya notifikasi perubahan berkas, jadi yang tahu ada
perubahan — yang menulis berkas itu — yang memanggil `UPDATE`. Itu cocok untuk jalur agent (broker
menulis tambalan, lalu memberi tahu indeks) dan untuk uji; pemantauan otomatis belum ada.

## Bukti

| Klaim | Bukti |
|---|---|
| Satu berkas yang berubah diindeks ulang sendirian, ≤ 2 detik (PRD §9) | L01 "a changed file is re-indexed on its own, …": `/spaceos/ws/FRESH.TXT` diubah; `UPDATE` melaporkan berubah setelah membaca 109 byte — ukuran berkas itu — dalam 3 ms pada run bukti |
| Tanpa full rescan, dilihat dari luar layanan | uji yang sama: `LATER.TXT` diubah pada saat yang sama; setelah `UPDATE` untuk `FRESH.TXT`, `lighthouse` (teks lama `LATER.TXT`) masih ditemukan dan `beacon` (teks barunya) belum, sampai `LATER.TXT` di-`UPDATE` sendiri |
| Pencarian berikutnya menjawab dari teks baru, dengan provenance yang cocok dengan disk | uji yang sama: `harbour` menemukan `FRESH.TXT` dan digest-nya dicocokkan dengan byte di disk; `tide` (teks lama) tidak lagi ditemukan; bundle baru dipimpin chunk baru |
| Tanpa perubahan, tidak ada perubahan; path yang tidak terindeks ditolak; yang dicabut tidak dibaca | uji yang sama: `UPDATE` kedua → 0; `NOPE.TXT` → `NotFound`; `LATER.TXT` dicabut lalu diubah → `UPDATE` membaca 0 byte dan teks barunya tidak muncul |
| Berkas yang tidak bisa dibaca lagi kehilangan chunk-nya | uji yang sama: `FRESH.TXT` ditulis 9000 byte → `UPDATE` → `MsgSize`, dan `harbour` tidak lagi ditemukan; setelah kembali kecil, `UPDATE` memasukkannya lagi |
| Ikut uji stabilitas | setiap putaran `cargo xtask stress` menjalankan uji ini (ADR-0019) |

## Gigi

Setiap pelemahan `bin/spacelink` dijalankan sendiri terhadap skenario `acceptance`, lalu
dikembalikan:

| Pelemahan | Akibat |
|---|---|
| `UPDATE` mengindeks ulang seluruh folder — dan tetap melaporkan hanya ukuran berkas itu sebagai byte yang dibaca | `updating /spaceos/ws/FRESH.TXT touched /spaceos/ws/LATER.TXT: query "lighthouse": not found` — angka yang dilaporkan layanan lolos, pemeriksaan dari luar tidak |
| Berkas yang berubah dilaporkan berubah, tetapi chunk-nya tidak diganti | `query "harbour": not found` |
| Chunk baru membawa digest yang bukan milik byte-nya (satu bit dibalik) | `digest mismatch for /spaceos/ws/FRESH.TXT [0..109]` |
| Berkas yang tidak bisa dibaca lagi tetap memegang chunk lamanya | `after a failed update: /spaceos/ws/FRESH.TXT still appears in results for "harbour"` |
| Berkas yang dicabut tetap dibaca | `updating a revoked file gave Ok(1), changed 1, 51 bytes read` |
| Setiap `UPDATE` dianggap perubahan | `an unchanged file was reported as changed` |

## Konsekuensi

- Freshness diukur pada korpus kecil lab (beberapa berkas di `/spaceos/ws`), bukan fixture 10.000
  berkas ≤ 100 MiB dari PRD: batas SpaceLink sekarang 16 dokumen dan 128 chunk (ADR-0013).
- `UPDATE` membaca seluruh dokumen untuk membandingkan digest: `SYS_FS_STAT` hanya melaporkan ukuran,
  bukan waktu modifikasi, jadi tidak ada jalan pintas yang bisa dipercaya.
- Berkas **baru** di folder yang diindeks tidak masuk lewat `UPDATE`; itu tetap urusan `INDEX`.
  Menghapus berkas belum bisa dilakukan di guest (belum ada syscall-nya).
- Command Center (ADR-0022) belum memanggil `UPDATE`: indeksnya tetap dibangun saat jendela dibuka.
- Pembacaan dokumen kini menutup handle berkasnya di setiap jalan keluar, galat termasuk; sebelumnya
  galat `fs_stat`/`fs_read` meninggalkan satu handle per kegagalan — sepele untuk `INDEX` yang gagal
  sekali, tidak untuk `UPDATE` yang dipanggil berulang.
