# ADR-0025 — Disk SATA (AHCI) dan NVMe, dan volume data yang dipilih dari labelnya

- Status: diterima
- Tanggal: 2026-09-30
- Menggantikan: argumen "ESP bukan perangkat virtio" di ADR-0015 (jaminannya tetap, alasannya berganti)
- Konteks: PRD §6 dan tahap 7 (matriks perangkat keras), pertanyaan pengguna "banyak yang tidak
  compatible dan belum bisa membaca yang lain", ADR-0007 (storage stack), ADR-0010 (matriks),
  ADR-0015 (penyimpanan yang bisa ditulis), ADR-0024 (SMP; IRQ hanya ke CPU boot)

## Konteks

Satu-satunya driver blok adalah virtio-blk, jadi disk data harus berupa perangkat virtio. PC
sungguhan menyimpan disknya di pengendali SATA (AHCI) atau NVMe — di mesin `q35` QEMU pun disk
boot sudah berada di AHCI bawaan. Sistem yang tidak bisa membaca disk semacam itu tidak bisa
dipasang di mana pun selain VM yang disiapkan khusus.

ADR-0015 menjamin kernel tidak pernah menulis ke bootloader atau image kernel dengan satu
alasan: ESP bukan perangkat virtio, jadi tidak terlihat. Begitu kernel punya driver AHCI, ESP
**terlihat** — di `q35` ia ada di port 0 pengendali yang sama. Jaminan itu harus dibuat ulang
dengan cara yang tidak bergantung pada perangkat mana yang kebetulan ada.

## Keputusan

**1. Tiga driver, satu lapisan.** `virtio_blk`, `ahci` dan `nvme` masing-masing menemukan
disknya sendiri. `block` memilih volume data dari semua disk itu dan menjadi satu-satunya yang
dilihat `fs`: sektor 512 byte, baca, tulis, flush.

**2. Volume data dipilih dari isinya, bukan dari pengendalinya.** Volume data adalah sistem
berkas FAT32 berlabel `SPACEDATA`, baik yang mengisi seluruh disk (yang dibuat `xtask`) maupun
di dalam satu partisi GPT atau MBR (disk yang juga memuat ESP — bentuk sistem yang terpasang).
Disk pertama yang memuatnya dipakai; setiap disk lain disebut di log beserta isinya dan tidak
disentuh lagi.

**3. Setiap permintaan dibatasi ke volume.** `fs` menghitung sektor dari awal volume; `block`
menambahkan awal partisi dan menolak permintaan yang melewati akhir volume **sebelum** driver
melihatnya. Partisi lain di disk yang sama — ESP di atas segalanya — tidak terjangkau oleh apa
pun yang dilakukan `fs`, termasuk oleh bug di FAT32. Itulah pengganti argumen ADR-0015.

**4. AHCI.** Untuk setiap pengendali (kelas PCI 01/06/01): minta firmware melepasnya bila ia
menyatakan memegangnya (BIOS/OS handoff), mode AHCI, interrupt HBA mati. **Setiap** port
dihentikan (ST, lalu FRE): command list dan area FIS yang diprogram firmware ada di memori yang
sudah diambil kernel, dan perangkat tidak boleh menulis ke sana lagi. Port dengan disk SATA
(DET = 3, signature 0x101) mendapat satu halaman kontrol (command list, FIS yang diterima, satu
command table) dan buffer bounce delapan halaman; disk ditanya IDENTIFY (LBA48 wajib, sektor
logis harus 512 byte). Transfer READ/WRITE DMA EXT, durabilitas FLUSH CACHE EXT. Kesalahan task
file memulai ulang engine port dan menggagalkan perintah itu saja; timeout mengeluarkan port
dari pemakaian untuk selamanya, karena penyelesaian yang terlambat akan dikira milik perintah
berikutnya. Pengendali yang hanya menjangkau 4 GiB pertama tidak diberi buffer di atasnya.

**5. NVMe.** Untuk setiap pengendali (01/08/02): reset (CC.EN = 0 — sekaligus mengakhiri apa pun
yang ditinggalkan firmware), admin queue, IDENTIFY pengendali dan namespace, satu pasang I/O
queue. Hanya namespace dengan blok 512 byte tanpa metadata yang dipakai; yang lain disebut dan
dibiarkan. PRP1, PRP2 atau daftar PRP untuk delapan halaman; FLUSH. Timeout mengeluarkan
pengendali dari pemakaian.

**6. Menunggu perangkat diukur dengan TSC.** Driver berjalan sebelum timer ada, dan perintahnya
berjalan di bawah spinlock dengan interrupt mati, jadi tick tidak bisa dipakai. Laju TSC belum
diketahui sepagi itu; menganggapnya 5 GHz — lebih cepat dari TSC x86 mana pun — membuat setiap
batas waktu paling sedikit sepanjang yang diminta. Satu perintah boleh 30 detik (flush di disk
lambat), jauh di bawah penantian yang dianggap deadlock oleh spinlock CPU lain.

**7. Polling, seperti virtio (ADR-0007).** Tanpa interrupt atau MSI: IRQ perangkat hanya sampai
ke CPU boot (ADR-0024), dan satu perintah pada satu waktu cukup untuk beban MVP.

## Bukti

| Klaim | Bukti |
|---|---|
| Disk data di SATA, disk boot di port sebelahnya | compat `sata-data`: `ahci: 00:1f.2 port 0: "QEMU HARDDISK", 64 MiB, AHCI 1.0` dan port 1 yang sama; `block: AHCI 00:1f.2 port 0 (64 MiB): MBR with 1 partition(s), none labelled SPACEDATA; not used`, `block: AHCI 00:1f.2 port 1 (64 MiB) holds the data volume (the whole disk)`, `vfs: FAT32 mounted from AHCI 00:1f.2 port 1`, `ALL TESTS PASSED (117/117, 0 skipped)` |
| Disk data di NVMe | compat `nvme-data`: `nvme: 00:02.0: "QEMU NVMe Ctrl", NVMe 1.4, 1 usable namespace(s) among IDs 1-16`, `block: NVMe 00:02.0 namespace 1 (64 MiB) holds the data volume (the whole disk)`, `ALL TESTS PASSED (117/117, 0 skipped)` |
| Label, bukan urutan, yang memilih; partisi membatasi | compat `sata-gpt`: disk GPT 130 MiB dengan ESP ber-FAT32 `SPACEOS` di partisi 1 dan volume data di partisi 2 → `block: AHCI 00:1f.2 port 1 (130 MiB) holds the data volume (GPT partition 2)`, `vfs: FAT32 mounted from AHCI 00:1f.2 port 1 (512 byte clusters, 64 MiB volume)`, `ALL TESTS PASSED (117/117, 0 skipped)` — tulisan uji tetap di dalam partisinya |
| ESP tidak dikira volume data | compat `no-disk`: `block: no disk holds a volume labelled SPACEDATA`, `vfs: no data volume` |
| Image GPT sah | uji unit `gpt_data_disk_is_valid`: kedua header dan tabel lolos CRC-32, partisi 2 sama byte demi byte dengan volume |

## Gigi

Setiap pelemahan dijalankan sendiri terhadap mesin `sata-gpt`, lalu dikembalikan:

| Pelemahan | Akibat |
|---|---|
| Label diabaikan: FAT32 mana pun dianggap volume data | `block: AHCI 00:1f.2 port 0 (64 MiB) holds the data volume (MBR partition 1)` — **ESP itu sendiri dipasang sebagai volume yang bisa ditulis**, dan `TESTS FAILED: 40 of 117` (berkas volume data tidak ada di sana) |
| Awal partisi diabaikan: sektor dihitung dari awal disk | `vfs: no usable FAT32 volume` — boot sector yang terbaca adalah MBR pelindung GPT; mesin berjalan tanpa penyimpanan dan harness menolaknya karena `vfs: FAT32 mounted from AHCI 00:1f.2 port 1` tidak muncul |

Pelemahan pertama adalah alasan keputusan 2 dalam satu baris: tanpa label, kernel memilih disk
pertama yang tampak seperti volume — di q35 itu disk boot.

## Konsekuensi

- Disk USB, IDE/PATA (PIIX di `i440fx`: ESP-nya tetap tidak terlihat), SCSI/virtio-scsi dan RAID
  belum didukung; disk seperti itu tidak terlihat sama sekali, jadi juga tidak bisa ditulis.
- Satu perintah pada satu waktu per disk, lewat buffer bounce 32 KiB: throughput jauh di bawah
  kemampuan NVMe. Cukup untuk model uji dan volume data MVP.
- Tidak ada hot-plug; disk yang dicabut membuat perintahnya timeout dan disk itu keluar dari
  pemakaian.
- Perangkat dengan sektor 4 KiB (4Kn, atau namespace NVMe berformat 4096) belum dipakai.
- Partisi dibaca tanpa memeriksa CRC tabel GPT: labelnya di boot sector yang menentukan, dan
  partisi yang keluar dari disk tidak pernah dibaca.
