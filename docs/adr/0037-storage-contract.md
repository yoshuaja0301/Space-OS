# ADR-0037 — Kontrak penyimpanan: penuh, gagal, dihapus, diganti

- Status: diterima
- Tanggal: 2026-10-06
- Konteks: PRD v0.2 §10.1 (jaminan data: write diterima vs durable, staging dan commit, "disk
  penuh, short write dan I/O error tidak boleh dilaporkan sebagai sukses"), D02 ("Write, flush,
  disk penuh dan crash recovery sesuai kontrak"); ADR-0015 (penyimpanan yang bisa ditulis),
  ADR-0036 (`SYS_FS_OPEN_WRITE`)

## Konteks

Sampai di sini volume data bisa ditulis dan diflush (ADR-0015), tetapi hanya jalan bahagianya yang
teruji. Volume yang penuh dilaporkan sebagai `NoMemory` -- kesalahan yang mengatakan sesuatu yang
lain -- dan sebuah tulisan yang kehabisan ruang di tengahnya sudah menulis sebagian datanya dan
menambah cluster ke rantai berkas sebelum gagal. Kesalahan perangkat sampai ke pemanggil sebagai
`Fault` ("alamat buruk") atau `WouldBlock`. Tidak ada cara mengganti dokumen tanpa jendela di mana
dokumen itu kosong (`SYS_FS_CREATE` mengosongkan dulu), tidak ada cara menghapus berkas, dan tidak
ada yang mencegah berkas dikosongkan selagi proses lain membacanya -- pembacanya lalu membaca
cluster yang sudah kembali ke pool, bisa jadi milik berkas lain.

## Keputusan

1. **Dua kesalahan baru di ABI.** `NoSpace` (22): volume tidak punya ruang untuk yang diminta, dan
   tidak sedikit pun ditulis. `Io` (23): perangkat melaporkan kegagalan atau berhenti menjawab;
   operasinya mungkin sebagian terjadi, tetapi tidak pernah dilaporkan selesai. Lapisan `block`
   menerjemahkan kegagalan setiap driver (virtio-blk, AHCI, NVMe) ke `Io`; penolakan yang bukan dari
   perangkat (permintaan tidak sah, perangkat read-only) tetap kesalahannya sendiri.
2. **Tulisan mengklaim semua clusternya lebih dulu.** Sebelum satu byte ditulis, rantai berkas
   diperpanjang sampai menampung seluruh tulisan. Volume yang tidak punya ruang untuk semuanya
   menolak tulisan itu utuh (`NoSpace`) dan berkasnya tetap seperti semula. Kegagalan sesudahnya
   (data, entri direktori, flush) mengembalikan entri ke keadaan sebelumnya lalu melepaskan cluster
   yang diklaim tulisan itu; bila entri tidak bisa dikembalikan, clusternya tetap terklaim (hilang,
   sampai pemeriksaan mengembalikannya) dan tidak pernah dimiliki dua berkas. Byte yang sudah ditimpa
   di dalam ukuran lama tetap tertimpa -- kesalahannya mengatakan tulisan tidak selesai.
3. **Berkas yang dipegang terbuka tidak dikosongkan, dihapus, atau diganti** (`Busy`). Kernel
   mencatat berkas terbuka menurut letak entri direktorinya sampai handle terakhirnya hilang.
4. **`SYS_FS_REMOVE` (42).** Entri ditandai terhapus dulu, lalu rantainya kembali ke pool: crash
   di antaranya meninggalkan cluster tanpa pemilik, bukan entri yang menunjuk cluster bebas.
5. **`SYS_FS_REPLACE` (43): staging dan commit.** Berkas staging dan target di direktori yang sama.
   Entri staging dihapus dulu; lalu entri target mengambil rantai dan ukuran staging dalam **satu
   tulisan sektor -- commit-nya**; sesudahnya rantai versi lama dibebaskan. Crash atau kesalahan
   sebelum commit meninggalkan versi lama (dan cluster staging hilang); sesudahnya, versi baru (dan
   cluster lama hilang): tidak pernah campuran, tidak pernah dua entri berbagi satu rantai. Tanpa
   target, staging diganti namanya (satu tulisan sektor). Ini klaim atomik untuk satu sektor yang
   ditulis utuh atau tidak sama sekali, bukan atomic rename umum.
6. **`SYS_FS_CHECK` (44).** Menelusuri setiap direktori dari root dan setiap rantai yang disebut
   entrinya, lalu FAT: cluster yang terambil tetapi tidak terjangkau adalah *hilang*, cluster yang
   terjangkau dua kali *cross-linked*. Dengan `repair` (hak `FS_WRITE`), cluster hilang kembali ke
   pool; yang terjangkau tidak pernah diubah.
7. **Injeksi kesalahan untuk uji** lewat `SYS_DEBUG` (hak root `DEBUG`): `FS_SPACE` membatasi berapa
   cluster lagi yang boleh diklaim (disk penuh tanpa mengisinya), `BLOCK_FAIL` membuat tulisan ke-n
   gagal dengan `Io`. `SYS_DEBUG` kini membawa satu argumen.
8. **Durabilitas tetap seperti ADR-0015:** setiap tulisan selesai dengan flush perangkat, jadi
   "diterima" berarti "diflush" bila perangkat mendukung flush; flush yang gagal adalah `Io`.

## Bukti

| Klaim | Uji |
|---|---|
| Disk penuh ditolak utuh | `D02`: dengan ruang untuk 2 cluster lagi, tulisan 5000 byte (10 cluster) → `NoSpace`; berkas tetap 1000 byte dengan isi yang sama, jumlah cluster bebas tidak berubah, tidak ada yang hilang; tanpa batas tulisan yang sama berhasil, dan menghapus berkas mengembalikan setiap cluster |
| Kesalahan perangkat bukan sukses | `D02`: tulisan yang tulisan perangkat pertamanya gagal, dan yang gagal di tengah mengklaim clusternya → `Io` keduanya; berkas tetap 600 byte dengan isi yang sama, tidak ada cluster hilang, tulisan berikutnya berhasil |
| Staging dan commit | `D02`: dokumen diganti versi staging-nya; commit yang tulisannya gagal (`Io`) meninggalkan versi lama dan tepat satu cluster hilang, yang dikembalikan `repair`; dokumen yang dipegang terbuka tidak diganti (`Busy`); antar-direktori `Invalid`; tanpa target, staging diganti namanya |
| Hapus | `D02`: berkas yang dipegang terbuka tidak dihapus atau dikosongkan (`Busy`); setelah dilepas berkas hilang (`NotFound`) dan semua clusternya kembali; direktori `Invalid`, tanpa `FS_WRITE` `Denied` |

## Gigi

| Pelemahan | Akibat |
|---|---|
| Tulisan yang kehabisan ruang menyimpan cluster yang sudah diklaimnya | `D02` merah: `the refused write kept clusters: 124953 free before, 124951 after, 0 lost` |
| Volume penuh dilaporkan sebagai kehabisan memori (`NoMemory`) | `D02` merah: `a write with no room for it gave Err(NoMemory)` |
| Tulisan data yang gagal dilaporkan tertulis | `D02` merah: `a write whose data write, overwriting what is there failed gave Ok(600)` |
| Commit ditulis sebelum entri staging dihapus | `D02` merah: `the old version or the new, never a mix: a failed commit changed the document` |
| Berkas yang dipegang terbuka bisa dihapus | `D02` merah: `a file held open: remove gave Ok(()), create gave Ok(Ok(()))` |
| Kegagalan perangkat dilaporkan sebagai alamat buruk (`Fault`) | `D02` merah: `a write whose first device write failed gave Err(Fault)` |
| Berkas yang dipegang terbuka bisa dikosongkan | `D02` merah: `a file held open: remove gave Err(Busy), create gave Ok(Ok(()))` |

## Konsekuensi

- Pemeriksaan volume membaca seluruh FAT dan setiap direktori; ia dipanggil bila diminta (uji,
  konsol recovery), tidak setiap boot.
- Kehilangan daya di tengah operasi tetap bisa meninggalkan cluster hilang; pemeriksaan
  menemukannya dan `repair` mengembalikannya. FAT32 tetap tanpa jurnal.
- Dua salinan FAT diperbarui berurutan; kegagalan di antara keduanya bisa membuat salinan kedua
  berbeda. Driver ini membaca salinan pertama.
- Belum ada `mkdir`, rename antar-direktori, nama panjang, atau batas ukuran/umur riwayat versi.
