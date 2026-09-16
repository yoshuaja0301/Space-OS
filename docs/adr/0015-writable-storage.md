# ADR-0015 — Penyimpanan yang bisa ditulis

- Status: diterima
- Tanggal: 2026-09-16
- Konteks: PRD §7 D01, roadmap backlog butir 5, ADR-0007 (storage stack dan ABI file)

## Konteks

Roadmap menyebut penyimpanan yang bisa ditulis sebagai **satu-satunya penghalang**
untuk tiga hal sekaligus: tambalan agent yang persisten (G01), revokasi yang bertahan
reboot (L02), dan instalasi serta rollback paket (P01). Selama volume hanya bisa
dibaca, ketiganya hanya bisa dibuktikan di dalam memori satu boot — yang artinya
yang dibuktikan adalah logikanya, bukan janjinya.

## Keputusan

**Volume data bisa ditulis; volume boot tidak bisa dijangkau sama sekali.** Satu-satunya
perangkat blok yang diikat kernel adalah disk virtio. ESP yang di-boot firmware bukan
perangkat virtio, jadi tidak ada tulisan dari sini yang bisa mencapai bootloader atau
image kernel. Itu bukan kebijakan yang ditegakkan pengecekan path — itu konsekuensi
dari perangkat mana yang ada, dan karena itu tidak bisa dilanggar oleh bug di FAT32.

**Menulis adalah hak tersendiri.** `FS` membaca volume; `FS_WRITE` mengubahnya. Proses
yang dipercaya membaca tidak otomatis dipercaya menulis ulang. Handle berkas juga
membawa izinnya sendiri: `SYS_FS_OPEN` tidak pernah memberi hak `WRITE`, jadi satu-satunya
cara mendapat handle yang bisa ditulis adalah lewat `SYS_FS_CREATE`.

**"Create atau kosongkan", bukan sunting di tempat.** Semua pemanggil di atas lapisan ini
menulis berkas utuh, jadi itulah operasi yang disediakan. `SYS_FS_CREATE` membuat berkas
kosong atau mengosongkan yang sudah ada; `SYS_FS_WRITE` menulis pada offset dan menumbuhkan
berkas bila perlu.

**Tiga aturan yang dipegang lapisan FAT32:**

1. **Setiap salinan FAT diperbarui.** Volume yang FAT-nya tidak sama adalah volume yang
   bisa dibaca berbeda oleh implementasi lain.
2. **Cluster baru dinolkan sebelum menjadi milik berkas.** Cluster itu masih menyimpan apa
   pun yang ditinggalkan pemilik sebelumnya, dan pembaca sebuah lubang akan melihatnya.
3. **Nama yang tidak muat 8.3 ditolak, bukan dipotong.** Driver ini tidak menulis entri
   nama panjang; berkas yang namanya bukan yang diminta lebih buruk daripada permintaan
   yang ditolak.

**Durabilitas diminta, bukan diasumsikan.** Driver menegosiasikan `VIRTIO_BLK_F_FLUSH` bila
ditawarkan dan mengirim flush setelah setiap perubahan metadata. Tanpa fitur itu, "tertulis"
hanya berarti "sudah diserahkan ke host", dan itu dikatakan apa adanya. `VIRTIO_BLK_F_RO`
tidak dinegosiasikan melainkan dicatat: perangkat yang menyatakan dirinya read-only membuat
setiap tulisan ditolak di depan (`Denied`), bukan dikirim lalu gagal diam-diam.

## Bukti

Tiga uji D01 baru, semuanya dilewati pada mesin tanpa disk:

- berkas ditulis dalam tiga bentuk (di dalam satu cluster, melewati batas cluster sehingga
  harus mengalokasi, lalu menambal di tengah) dan dibaca ulang **byte demi byte**, dengan
  ukuran dari `fs_stat` harus cocok;
- hak dibuktikan terpisah: `FS` tanpa `FS_WRITE` → `Denied`, handle dari `fs_open` → `Denied`,
  nama panjang → `Invalid`, direktori → `Invalid`;
- satu berkas penghitung dibaca lalu ditulis satu lebih tinggi setiap boot. Skenario
  `storage-reboot` menuntut boot kedua **menemukan angka yang ditinggalkan boot pertama** —
  sesuatu yang tidak bisa dipalsukan oleh pembukuan di memori.

Giginya terbukti: dengan `write_sectors` diubah menjadi no-op yang melaporkan sukses, uji
pertama gagal dengan `open: not found`.

## Konsekuensi

- Yang terbuka: ketiga layanan (agent, SpaceLink, paket) sekarang **bisa** menyimpan
  keadaannya. Yang belum dikerjakan: memindahkannya ke sana. Sampai itu selesai, catatan
  jujur di `docs/requirements.md` berbunyi "belum dipindahkan", bukan lagi "tidak mungkin".
- Yang tidak ada: jurnal. Kehilangan daya di tengah `write` bisa meninggalkan FAT dan entri
  direktori tidak sinkron. Untuk volume data uji itu bisa diterima; untuk penyimpanan
  sungguhan, jurnal atau filesystem copy-on-write adalah pekerjaan tersendiri.
- Yang tidak ada: menghapus berkas, membuat direktori, dan nama panjang.
