# ADR-0010 — Matriks kompatibilitas mesin dan degradasi anggun

- Status: diterima
- Tanggal: 2026-09-16
- Konteks: ADR-0003 (profil lab QEMU), ADR-0007 (storage stack)

## Konteks

ADR-0003 memaku satu profil lab (q35, TCG, 4 vCPU `qemu64`, 8 GiB, OVMF 4M) supaya
hasil uji dapat diulang. Profil itu menjawab "apakah build ini benar", tetapi tidak
menjawab "apakah build ini jalan di mesin lain". Dua hal membuat pertanyaan kedua
nyata:

1. Bug yang hanya muncul di konfigurasi lain tidak akan pernah terlihat. Driver
   virtio-blk sempat memakai konstanta `QUEUE_SIZE` (16) sebagai modulus ring dan
   mengirim sampai 10 deskriptor per permintaan, padahal ukuran antrean adalah
   hasil negosiasi. Di profil lab perangkat menawarkan 256 sehingga hasil
   negosiasi selalu 16 dan bug itu tidak pernah terpicu.
2. Perangkat keras boleh tidak ada. Mesin tanpa disk atau tanpa GOP bukan mesin
   rusak; sistem harus tetap boot dan mengatakan apa yang hilang.

## Keputusan

**Matriks dijalankan, bukan diasumsikan.** `cargo xtask compat` mem-boot *image
yang sama* pada setiap konfigurasi di bawah dan memeriksa marker log serta kode
keluar. Perintah ini bagian dari `cargo xtask ci`, jadi setiap push menjalankannya.

| Mesin | Konfigurasi | Yang dibuktikan |
|---|---|---|
| `lab` | q35, `qemu64`, 4 vCPU, 8 GiB, virtio-blk modern | profil ADR-0003, acuan |
| `q35-1cpu-2g` | 1 vCPU, 2 GiB | tidak ada asumsi jumlah CPU atau besar RAM; linear map ikut mengecil |
| `i440fx` | `-machine pc`, 2 vCPU, 4 GiB | chipset lain: PCI host bridge, peta MMIO dan IRQ berbeda |
| `cpu-max` | `-cpu max` | fitur CPU tambahan tidak mengubah perilaku; tidak ada ketergantungan pada `qemu64` |
| `virtio-transitional` | `disable-legacy=off,disable-modern=off` | perangkat transisional (device id 0x1001) tetap dinegosiasikan lewat kapabilitas modern |
| `virtio-small-queue` | `queue-size=4` | indeks ring memakai ukuran hasil negosiasi dan permintaan dipotong agar muat (4 deskriptor → 2 halaman data) |
| `no-disk` | tanpa `virtio-blk` | tanpa penyimpanan sistem tetap boot; uji berbasis disk **dilewati**, bukan gagal |
| `no-vga` | `-vga none` | tanpa GOP konsol jatuh ke serial saja |
| `vmware-vga` | `-vga vmware` | adaptor tampilan lain tidak membuat boot gagal |

**Degradasi anggun punya jalur yang terdefinisi.** Perangkat yang hilang menempuh
urutan yang sama di setiap lapis: laporkan, lanjut, dan beri tahu user space.

- `virtio-blk: no device present` → `vfs: no block device; file system unavailable`.
- `KernelStats.volume_sectors` bernilai 0 ketika tidak ada volume ter-mount. Itu
  satu-satunya cara user space membedakan "mesin ini tidak punya disk" dari
  "pembacaan gagal"; `fs_open` yang mengembalikan `NotFound` tidak membedakan.
- `init` memakai nilai itu untuk melewati uji D01/A01 dengan alasan tercetak
  (`SKIP …(no disk on this machine)`) dan tetap selesai dengan `ALL TESTS PASSED
  (36/36, 3 skipped)`, exit 33.

**Linear map hanya memetakan RAM.** Bootloader dulu memetakan `0..phys_map_end`
write-back, sehingga BAR perangkat punya dua pemetaan dengan tipe memori berbeda
(write-back di linear map, uncached di jendela MMIO kernel). SDM menyatakan
kombinasi itu undefined, dan pembacaan spekulatif lewat alias write-back dapat
menyentuh register perangkat. Sekarang bootloader hanya memetakan deskriptor UEFI
yang berjenis RAM plus framebuffer; MMIO dipetakan on demand oleh kernel saja.

## Konsekuensi

- Waktu CI bertambah ~2 menit (sembilan boot tambahan). Itu harga yang dibayar
  agar klaim "kompatibel" punya bukti.
- Matriks ini *bukan* uji perangkat keras fisik. Semua masih QEMU/TCG; H01 dan H02
  tetap terbuka (lihat `docs/limitations.md`).
- Menambah mesin berarti menambah satu entri `MACHINES` di `xtask/src/main.rs`.
  Marker khusus per mesin ditulis di entri itu, bukan tersebar di kode uji.
- Uji yang dilewati dihitung terpisah dari yang lulus. Sebuah konfigurasi yang
  melewati uji tidak boleh terbaca seolah-olah menjalankannya.
