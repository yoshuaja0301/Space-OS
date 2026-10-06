# ADR-0009: Format model SpaceLM v0, runtime inferensi, dan baseline yang dipatok

Status: Diterima — 2026-09-16

## Konteks

Tahap 5 PRD menuntut inferensi native: model kecil menghasilkan 128 token offline, hasilnya cocok dengan baseline yang dipatok (A01), tanpa proses inferensi di host. PRD §6 menyarankan "model berlisensi sesuai yang kecil dan dapat direproduksi, sekitar 100–500 juta parameter bila cocok dengan engine", dan menuntut pilihan model, quantization, tokenizer, hash, serta expected output dipatok **sebelum** pengujian.

## Keputusan

1. **Format sendiri, SpaceLM v0** (`spaceabi::model`): header 64 byte `repr(C)` (magic `SLM0`, versi, arsitektur, `n_layers`, `d_model`, `n_heads`, `head_dim`, `d_ff`, `vocab`, `max_seq`, `rope_theta`) diikuti bobot `f32` little-endian dalam urutan tensor yang tetap. Arsitektur: decoder-only, RMSNorm, rotary embedding, feed-forward SwiGLU. Header dan tata letak tensor dipakai bersama oleh alat host dan runtime guest, sehingga keduanya tidak bisa menyimpang. GGUF tidak dipakai: PRD hanya menuntut subset yang dinyatakan, dan format sendiri membuat validasi ukuran tensor menjadi satu pemeriksaan.
2. **Validasi sebelum memuat** (PRD §4): magic, versi, arsitektur, setiap dimensi terhadap batas (`MAX_LAYERS/MAX_D_MODEL/MAX_VOCAB/MAX_SEQ/MAX_D_FF`), konsistensi `n_heads * head_dim == d_model`, dan **ukuran berkas harus sama persis** dengan ukuran tensor yang dideklarasikan. Model rusak ditolak dengan alasan, bukan crash — diuji dengan tiga fixture (`badmagic`, `baddims`, `trunc`) yang dibuat host.
3. **Checksum sebelum pakai**: runtime menghitung SHA-256 sambil menstreamkan berkas langsung ke buffer compute, lalu membandingkannya dengan manifest. Bobot tidak pernah disalin dua kali dan tidak pernah ditahan di heap.
4. **Tokenizer byte-level** (vocab 256). Dipilih karena dapat direproduksi persis dan tidak memerlukan berkas tokenizer terpisah; tokenizer sub-word menyusul bersama model terlatih.
5. **Semua aritmetika lewat Compute ABI.** Runtime memiliki tata letak (cache KV, transposisi V, potongan buffer) dan penjadwalan; layanan compute memiliki aritmetika. Satu langkah dekode = 54 operasi Compute untuk konfigurasi ini.
6. **Baseline dipatok oleh implementasi referensi host** (`xtask/src/reference.rs`) yang memakai **modul math dan tata letak yang sama** (`spaceabi::math`, `spaceabi::model`) serta urutan operasi yang sama. Karena f32 IEEE-754 deterministik dan tidak ada kontraksi FMA pada target keduanya, hasilnya harus cocok **token demi token**, bukan sekadar dalam toleransi. Baseline (prompt + 128 token) ditulis ke disk data sebagai `/spaceos/baseline.txt` bersama model dan manifest, jadi harapan dipatok sebelum guest dijalankan.
7. **Seed model dipilih dengan kriteria yang tercatat**: `cargo xtask seedsearch` menilai kandidat seed berdasarkan jumlah token berbeda dan panjang pengulangan terpanjang; seed yang dipatok menghasilkan 57 token berbeda dari 128, sehingga baseline benar-benar membedakan implementasi yang salah (model acak dapat terjebak mengulang satu token, yang akan membuat uji ini lemah).

## Konsekuensi dan batas yang tidak disamarkan

- Model referensi berukuran **115.008 parameter (449 KiB), bukan 100–500 juta**, dan **tidak dilatih**: bobotnya berasal dari PRNG ber-seed. Keluarannya karena itu **tidak bermakna sebagai teks**. Yang dibuktikan A01 adalah pipeline native-nya benar dan deterministik: disk → verifikasi checksum → validasi model → Compute ABI → 128 token yang identik dengan referensi. Porting model terlatih berlisensi (dan tokenizer-nya) tetap menjadi pekerjaan terbuka.
- Angka kecepatan berasal dari QEMU TCG dan bukan performa perangkat fisik (PRD §9).
- Tidak ada quantization, batching, atau sampling; dekode greedy dengan KV cache penuh f32.
