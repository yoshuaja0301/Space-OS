# ADR-0008: Objek memori bersama dan Space Compute ABI versi 0

Status: Diterima — 2026-09-16

## Konteks

PRD §4 menetapkan Compute ABI v0: tipe dan ukuran eksplisit, opaque handle, aturan ownership, error code, timeout, cancellation, negosiasi versi, tanpa pointer mentah lintas proses. Primitive usulan: `device_query, buffer_create, buffer_map, queue_create, submit, wait, cancel, release`. PRD juga menegaskan pemuatan model dan generate adalah API **AI Runtime di atas layanan Compute, bukan syscall kernel**, dan kernel tidak memuat tensor.

## Keputusan

1. **Kernel hanya menambah satu objek: memory object.** `SYS_VMO_CREATE(len)` mengalokasikan frame yang dibebankan ke kuota pembuatnya dan mengembalikan handle (`READ|WRITE|MAP|TRANSFER|DUP`). `SYS_VMO_MAP(handle, flags)` memetakannya ke pemanggil (`READ_ONLY` memetakan tanpa tulis meskipun handle punya `WRITE`); `SYS_VMO_SIZE` melaporkan ukurannya. Frame dimiliki objek: pemetaan datang dan pergi, frame dibebaskan saat handle terakhir ditutup, dan kuota pembuat dikembalikan saat itu. Kernel tidak tahu apa pun tentang tensor.
2. **Layanan compute berjalan di user space** (`bin/spacecompute`). Kontrol berupa pesan `repr(C)` berukuran tetap lewat channel; data besar berada di memory object yang dipetakan kedua sisi. Jadi tidak ada pointer yang menyeberangi proses dan tidak ada tensor yang melewati channel.
3. **Negosiasi versi wajib**: `HELLO` harus menjadi pesan pertama; versi yang berbeda ditolak `Invalid`; permintaan sebelum `HELLO` ditolak `Denied`.
4. **Status memakai kode error ABI kernel** (`0` sukses, selain itu negasi `Error`), jadi klien mendekode error layanan persis seperti error syscall.
5. **Eksekusi bertahap** agar `WAIT` benar-benar bisa timeout dan `CANCEL` benar-benar bisa menghentikan pekerjaan: satu tiket dieksekusi dalam langkah berbatas (`STEP_UNITS`, satu baris keluaran untuk MATMUL), kemajuan disimpan, dan tenggat diperiksa di antara langkah. Tiket yang kehabisan waktu melaporkan `WouldBlock` + `RUNNING` dan dapat dilanjutkan; tiket yang dibatalkan melaporkan `CANCELLED` lalu dilupakan.
6. **Validasi di waktu submit**: jenis operasi tak dikenal → `NoSys`; buffer tak dikenal → `BadHandle`; irisan di luar buffer → `Invalid`; irisan lebih kecil dari yang dibutuhkan dimensi → `MsgSize`. Operasi yang sudah masuk antrean karenanya tidak pernah menyentuh memori di luar batas.
7. **Satu koneksi satu klien.** Layanan dijalankan per klien (bootstrap channel-nya adalah koneksinya). Broker koneksi dan multiplexing adalah pekerjaan Developer Preview.
8. **Backend CPU** mengimplementasikan operasi yang dibutuhkan satu arsitektur model (fill, copy, add, mul, matmul, rmsnorm, softmax, silu-mul, rope, embed, argmax) plus `SPIN` yang hanya ada agar timeout dan cancellation dapat diuji secara deterministik. GPU/NPU adalah backend tambahan kelak; operasi yang tidak tersedia harus mengembalikan error, bukan diam-diam diabaikan.

## Konsekuensi

- Klien membayar satu round trip IPC per operasi; tensor tidak disalin. Untuk model kecil pada tahap 5 ini memadai; batching dan submit asinkron menyusul.
- Kuota: frame buffer dibebankan ke layanan (pembuat), bukan klien. Layanan karenanya memerlukan kuota sebesar seluruh buffer yang dilayaninya, dan klien yang nakal dapat membuat layanan kehabisan kuota — ditulis di `docs/limitations.md`.
- Matematika f32 (exp, ln, sin, cos, sqrt) diimplementasikan sendiri karena tidak ada libm; akurasinya cukup untuk model uji dan menjadi bagian dari baseline yang dipatok.
