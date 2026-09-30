# Studi kelayakan akselerator (PRD tahap 6)

Tahap 6 PRD meminta dua hal: **studi kelayakan** dan **driver** untuk satu GPU
terpilih. Dokumen ini mengerjakan yang pertama dan menyatakan dengan jujur bahwa
yang kedua belum dimulai, beserta apa yang harus ada lebih dulu.

Studi ini dibuat dari kode yang ada di repo ini, bukan dari rencana: setiap klaim
tentang "sudah ada" menunjuk berkas yang bisa dibaca.

## 1. Apa yang sudah siap menerima akselerator

Compute ABI v0 (ADR-0008, `abi/spaceabi/src/compute.rs`) sengaja dibentuk supaya
backend bukan bagian dari kontrak:

- **Klien tidak pernah menyebut perangkat.** Ia menyebut buffer, antrean, operasi,
  dan tiket. `DEVICE_QUERY` mengembalikan `flags` yang saat ini bernilai
  `backend::CPU`; menambah `backend::GPU` tidak mengubah satu baris pun di
  `bin/spaceai`.
- **Tensor tidak pernah lewat IPC.** Data ada di memory object yang dipetakan kedua
  sisi (`SYS_VMO_*`). Itu tepat bentuk yang dibutuhkan untuk buffer yang nantinya
  juga harus terlihat oleh perangkat.
- **Eksekusi sudah asinkron di tingkat kontrak.** `SUBMIT` mengembalikan tiket;
  `WAIT` punya tenggat; `CANCEL` benar-benar menghentikan pekerjaan. Backend GPU
  memetakan ini ke fence dan pembatalan perangkat tanpa mengubah ABI.
- **Kegagalan sudah punya tempat.** `status`, `Reject`-style alasan, dan uji
  kontrak C01 sudah menuntut versi salah, handle salah, batas buffer, operasi tak
  didukung, timeout, dan cancel dijawab benar.

Konsekuensinya: **backend GPU adalah proses user-space baru yang bicara ABI yang
sama**, bukan perubahan kernel. Itu hasil desain yang paling berharga dari tahap 4.

## 2. Apa yang belum ada, berurutan

Ini daftar penghalang nyata, bukan daftar keinginan. Urutannya adalah urutan
kerjanya.

### 2.1 Kapabilitas perangkat untuk user space (penghalang utama)

Hari ini satu-satunya driver perangkat ada **di dalam kernel**
(`kernel/src/dev/virtio_blk.rs`, ADR-0007) karena kernel adalah satu-satunya yang
boleh menyentuh MMIO dan DMA. Backend GPU di user space memerlukan tiga kapabilitas
yang belum ada:

| Butuh | Bentuk yang masuk akal di sini | Kenapa belum ada |
|---|---|---|
| Memetakan BAR perangkat | objek kernel `Mmio` dengan hak `MAP`, dipetakan uncached | jendela MMIO sekarang hanya dipetakan kernel (`kernel/src/mm/mmio.rs`) |
| Memori DMA yang alamat fisiknya diketahui | memory object yang bisa "dipin" dan melaporkan alamat fisiknya | `MemoryObject` menyimpan frame tetapi tidak pernah membocorkan alamat fisik ke user space — dan itu **disengaja** |
| Interupsi perangkat | pesan channel dari IRQ, atau objek `Event` yang bisa di-`wait` | kernel belum meneruskan IRQ apa pun ke user space |

Yang ketiga juga menghapus alasan polling di driver blok sekarang.

### 2.2 Tanpa IOMMU, driver DMA tetap tepercaya

Ini titik yang paling penting untuk dinyatakan terus terang. Memberi proses
user-space kemampuan menyuruh perangkat menulis ke alamat fisik pilihannya berarti
memberi proses itu kemampuan menulis ke **seluruh** memori fisik, kernel termasuk.
Tanpa IOMMU (VT-d/AMD-Vi), "driver GPU di user space" **tidak** lebih aman daripada
driver di kernel; ia hanya lebih mudah di-restart.

Maka ada dua jalur jujur, dan pilihannya harus eksplisit:

1. **Dengan IOMMU**: kernel memprogram domain IOMMU per proses driver, dan
   kapabilitas DMA menjadi benar-benar terbatas. Ini yang sesuai dengan janji
   PRD §5 soal isolasi, dan ini pekerjaan tersendiri (ACPI DMAR, tabel halaman
   IOMMU, konteks per perangkat).
2. **Tanpa IOMMU**: driver GPU tetap di kernel, seperti virtio-blk hari ini, dan
   ditulis sebagai komponen tepercaya dengan konsekuensi yang dicatat.

Rekomendasi: **jalur 2 untuk prototipe, jalur 1 sebelum ada klaim isolasi apa pun.**

### 2.3 Sinkronisasi dan model memori

Buffer Compute hari ini adalah frame biasa yang dipetakan write-back. Untuk
perangkat diperlukan: halaman yang koheren (atau flush eksplisit), fence antara
tulisan CPU dan baca perangkat, dan aturan siapa pemilik buffer selama operasi
berjalan. ABI sudah punya tempatnya (tiket), tetapi semantiknya belum ditulis.

## 3. Kandidat perangkat

| Kandidat | Kelebihan | Kekurangan |
|---|---|---|
| **virtio-gpu (virgl/venus)** | jalur transport sudah dikenal repo ini (virtio 1.0 modern sudah jalan di `virtio_blk.rs`), ada di QEMU, tidak butuh firmware vendor | akselerasi compute-nya lewat host; yang diuji bukan GPU asli, dan API-nya besar (Vulkan/GL) |
| **Perangkat compute virtio buatan sendiri** | kecil, cocok persis dengan Compute ABI, bisa diuji penuh di QEMU dengan perangkat kustom | bukan perangkat nyata; membuktikan jalur ABI, bukan kinerja |
| **GPU diskrit (AMD/Intel) native** | satu-satunya yang membuktikan angka kinerja sungguhan | butuh perangkat keras fisik (H01, belum ada di sini), firmware, dan ribuan baris inisialisasi mode/ring/power |

Rekomendasi untuk langkah berikutnya: **perangkat compute virtio buatan sendiri**,
justru karena ia memisahkan dua pertanyaan yang sering tercampur — "apakah jalur
kapabilitas, DMA, dan fence kita benar" (bisa dijawab di QEMU, hari ini) dari
"apakah GPU ini cepat" (butuh H01). Menjawab yang pertama lebih dulu membuat
pekerjaan GPU asli jadi porting, bukan penemuan.

## 4. Urutan kerja yang disarankan

1. Objek kernel `Mmio` + hak `MAP` untuk BAR perangkat; pindahkan virtio-blk ke
   user space sebagai pembuktian jalur (dan hilangkan satu komponen tepercaya).
2. Penerusan IRQ ke user space (objek `Event` atau pesan channel); hapus polling.
3. Memory object yang bisa dipin dengan alamat fisik, **hanya** di balik hak baru
   yang terpisah, plus catatan eksplisit bahwa tanpa IOMMU hak itu setara root.
4. Perangkat compute virtio kustom + backend `spacegpu` yang bicara Compute ABI v0;
   jalankan uji kontrak C01 yang **sama persis** terhadapnya.
5. IOMMU (DMAR), lalu ulangi 3 dengan batas yang sungguhan.
6. Baru setelah itu: GPU fisik (tergantung H01).

## 5. Kesimpulan

Kelayakannya baik dan penghalangnya bukan di Compute ABI: kontraknya sudah
mengabstraksikan backend, dan uji kontraknya sudah ada dan lulus. Penghalangnya
adalah **kapabilitas perangkat di user space** dan **IOMMU**, keduanya pekerjaan
kernel, dan keduanya berguna di luar GPU (driver blok, jaringan, input).

Driver GPU belum dimulai, dan tidak akan ditulis di lingkungan ini karena tidak ada
perangkat keras untuk memverifikasinya (lihat H01 di `docs/requirements.md`).
