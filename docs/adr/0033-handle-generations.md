# ADR-0033 — Handle membawa generasi slotnya

- Status: diterima
- Tanggal: 2026-10-06
- Konteks: PRD v0.2 K04 ("Invalid/stale handle dan transfer hak berlebih ditolak"), §8.3 ("Handle
  bersifat lokal ke proses dan harus tahan reuse yang salah"); ADR-0004 (ABI syscall v0 dan handle)

## Konteks

Sampai sekarang nilai handle adalah indeks slot di tabel handle proses, dan slot kosong dipakai lagi
oleh handle berikutnya. Handle yang sudah ditutup -- atau sudah dipindahkan ke proses lain lewat
pesan atau spawn -- karena itu tidak mati: angkanya menunjuk apa pun yang berikutnya menempati slot
itu. Bug "tutup dua kali" di sebuah layanan tidak gagal, melainkan menutup objek orang lain; komentar
di `spaceshell` sampai harus memperingatkan bahwa menutup handle yang sudah dikonsumsi bisa menutup
"sesuatu yang lain sama sekali". Uji negatif ABI yang ada menolak handle yang salah jenis, hak yang
diperluas dan transfer yang terlarang, tetapi tidak bisa menolak handle basi karena kernel tidak
bisa membedakannya.

## Keputusan

1. **Handle = generasi slot di bit atas, indeks slot di 8 bit bawah** (`MAX_HANDLES` = 256). Setiap
   slot mencatat generasinya; mengosongkan slot (close, atau handle yang berpindah proses)
   menaikkan generasinya. Handle hanya berlaku bila slot yang dinamainya berisi dan masih di
   generasi yang sama; selain itu `BadHandle`.
2. Generasi berputar di bawah 2²⁴ − 1, sehingga `handle::INVALID` (semua bit 1) tidak pernah menjadi
   handle sungguhan. Slot yang sama harus dikosongkan 16 777 215 kali sebelum sebuah angka lama bisa
   hidup kembali.
3. Handle bootstrap tetap 0: slot 0, generasi 0. ABI tidak berubah bentuk (`Handle` tetap `u32`), dan
   program yang memperlakukan handle sebagai nilai buram tidak perlu diubah.

## Bukti

`bin/abi_negative` menutup sebuah endpoint, lalu menduplikasi endpoint lain sampai salinannya
menempati slot yang sama: angkanya harus berbeda, dan `handle_info`, `send` dan `handle_close`
dengan angka lama harus `BadHandle`. Di acceptance: `0x401's slot was reused as 0x501, a different
handle` (slot 1, generasi 4 lalu 5), lalu ketiga penggunaan angka lama `BadHandle`; seluruh suite,
matriks compat dan AArch64 tetap lulus dengan nilai handle yang tidak lagi kecil dan berurutan.

## Gigi

| Pelemahan | Akibat |
|---|---|
| Generasi tidak diperiksa (indeks saja) | angkanya tetap berbeda (`0x401` lalu `0x501`), tetapi angka lama sampai ke objek baru: `handle_info` → `Ok(HandleInfo { kind: 1, rights: 15 })`, `send` → `PeerClosed`, `handle_close` → `Ok(())` (salinan baru ikut tertutup); acceptance `FAIL K02: syscall ABI negative tests`, QEMU exit 35 |

## Konsekuensi

- Nilai handle tidak lagi kecil dan berurutan; program tidak boleh memakainya sebagai indeks.
- Handle yang dipegang lebih dari 16 juta siklus close pada slot yang sama bisa hidup kembali;
  batas itu jauh di atas apa pun yang dijalankan uji stres.
