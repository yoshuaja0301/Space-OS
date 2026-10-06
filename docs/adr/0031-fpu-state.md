# ADR-0031 — Register FP/SIMD milik thread yang menulisnya

- Status: diterima; menggantikan keputusan 2 ADR-0017 ("unit FPU dan vektor dimatikan")
- Tanggal: 2026-10-06
- Konteks: PRD v0.2 §8.2 ("State floating-point/SIMD disimpan serta dipulihkan dengan benar.
  Deteksi fitur CPU dan ukuran state menjadi bagian uji"), K03 ("FPU/SIMD context tidak bocor atau
  rusak antarproses"), §6 ("AVX tidak diasumsikan pada baseline; fitur CPU dipilih melalui deteksi
  dan fallback yang diuji"), §12 (optimasi SIMD sesudah pembuktian numerik); ADR-0017, ADR-0024
  (SMP), ADR-0028 (AArch64)

## Konteks

ADR-0017 mematikan unit FPU dan vektor untuk user space: kernel tidak menyimpan state-nya per
thread, jadi satu-satunya cara agar proses tidak membaca register yang ditinggalkan proses lain
adalah membuat setiap instruksi FP/SIMD membunuh prosesnya. PRD v0.2 meminta yang sebaliknya:
state FP/SIMD disimpan dan dipulihkan dengan benar, dengan deteksi fitur CPU dan ukuran state
sebagai bagian uji, dan tidak boleh bocor atau rusak antarproses. Itu juga prasyarat optimasi
SIMD untuk inferensi nanti (§12).

## Keputusan

**1. Setiap thread user punya area state FP/SIMD** (`kernel/src/fpu.rs`). Penjadwal menyimpan
register thread yang ditinggalkan ke areanya dan memuat register thread berikutnya pada **setiap**
switch, tepat sebelum stack berganti (eager). Tidak ada trik "pemakaian pertama" (#NM): register
yang tidak ditulis sebuah thread tidak pernah berisi apa yang ditinggalkan thread lain, dan tidak
ada jendela seperti LazyFP. Area thread baru dimulai dari state awal arsitektur. Thread idle hanya
menjalankan kode kernel dan tidak punya area.

**2. x86-64: x87 dan SSE, ditambah AVX bila ada, lewat XSAVE bila ada.** Setiap CPU menyalakan
unit untuk user space (CR0.EM dan TS clear, MP dan NE set; CR4.OSFXSR dan OSXMMEXCPT set). Bila
CPU punya XSAVE, CPU boot menyalakan CR4.OSXSAVE dan `XCR0` = x87 | SSE | AVX (AVX hanya bila
CPUID melaporkannya), dan ukuran area diambil dari CPUID leaf 0xD untuk tepat komponen itu; CPU
lain memakai `XCR0` yang sama. Tanpa XSAVE, area adalah image FXSAVE 512 byte. State awal: FCW
0x37F, MXCSR 0x1F80 (semua exception dimask, pembulatan ke terdekat), register nol; di format XSAVE
header XSTATE_BV nol, jadi XRSTOR memasang state awal setiap komponen. AVX-512 dan komponen XSAVE
lain tidak dinyalakan: instruksinya #UD.

**3. AArch64: V0–V31, FPCR, FPSR.** `CPACR_EL1.FPEN = 0b11`; area 528 byte (32 × 16 + 16) yang
disimpan dengan `stp q`/`ldp q` dan `mrs`/`msr` FPCR/FPSR. State awal semua nol.

**4. Exception FP yang tidak dimask membunuh prosesnya saja.** #MF (x87) → `X87_FP_ERROR`, #XM
(SSE/AVX) → `SIMD_FP_ERROR`; di kernel keduanya panic (kernel tidak memakai unit ini).

**5. Kernel dan program biasa tetap soft-float, dan gerbang build menegakkannya per biner.**
Kernel hanya boleh berisi instruksi yang menyimpan dan memuat state (FXSAVE64, FXRSTOR64,
XSAVE64, XRSTOR64, XSETBV; di AArch64 load/store register FP/SIMD dan akses FPCR/FPSR, tanpa
aritmetika). Program biasa tetap tanpa satu pun instruksi FP/SIMD: baseline inferensi A01 sama
bit demi bit di kedua arsitektur dalam FP perangkat lunak, dan optimasi SIMD menunggu pembuktian
numerik (§12). Pengecualiannya `bin/fpu` (uji K03, berapa pun) dan `bin/fault` (tepat delapan
instruksi yang memicu exception FP dengan sengaja, hanya di x86-64).

**6. Uji K03.** `bin/fpu pattern <seed>` mengisi setiap register vektor, MXCSR dan control word
x87 (FPCR dan FPSR di AArch64) dan puncak stack x87 dengan nilai dari seed, lalu selama 400 ms
bergantian berputar (agar timer merebut CPU) dan tidur sebentar, memeriksa setelah setiap putaran
bahwa semuanya masih tepat. Enam berjalan bersamaan dengan seed berbeda di empat CPU. `bin/fpu
fresh` memeriksa, sebelum menyentuh apa pun, bahwa proses baru mulai dengan state awal.

## Bukti

| Klaim | Bukti |
|---|---|
| x86-64 tanpa XSAVE (`qemu64`) | `fpu: x87 and SSE per thread through FXSAVE, 512 bytes`; enam pola `XMM0-15, MXCSR, the x87 control word and the x87 stack held through 580–635 checks in 400 ms`; `fresh: x87, MXCSR and XMM0-15 in the initial state` |
| x86-64 dengan XSAVE dan AVX (`-cpu max`) | `fpu: x87 and SSE and AVX per thread through XSAVE, 832 bytes`; enam pola `YMM0-15 (AVX), …` 611–633 pemeriksaan; `fresh: x87, MXCSR and YMM0-15 (AVX) in the initial state` |
| AArch64 | `fpu: V0-V31, FPCR and FPSR per thread, 528 bytes`; enam pola `V0-V31, FPCR and FPSR held through` 608–625 pemeriksaan; `fresh` bersih |
| Exception FP | `#MF`: `bin/fault` dibunuh `x87 floating-point exception`, kernel lanjut. `#XM` tidak bisa diuji di QEMU TCG — TCG menyetel flag MXCSR tanpa pernah menjebak — jadi uji itu dilewati dengan alasan di TCG dan berjalan di KVM atau perangkat keras |
| Gerbang build | `FP/SIMD instructions: the kernel 6 (saving and loading a thread's state), fault 8, fpu 91, none in the other 25 programs`; AArch64: kernel 36, `fpu` 56, nol di 26 program lain |

## Gigi

| Pelemahan | Akibat |
|---|---|
| Register thread berikutnya tidak dimuat pada switch | `pattern 2: XMM 0 changed after 3 checks: byte 0 is 0x65, not 0xcb` — 0x65 adalah byte pertama seed 1: register satu proses terbaca oleh proses lain |
| Register thread yang ditinggalkan tidak disimpan | `pattern 1: XMM 0 changed after 3 checks: byte 0 is 0x00, not 0x65` di keenam proses |
| State awal dengan MXCSR 0x3F80 | `fresh: control/status not initial: FCW 0x037f FSW 0x0000 FTW 0x00 MXCSR 0x00003f80` |

## Konsekuensi

- Setiap switch antar thread user menyimpan dan memuat 512–832 byte (x86-64) atau 528 byte
  (AArch64), juga untuk thread yang tidak pernah memakai unit itu. XSAVEOPT/XSAVEC dan pemuatan
  lazy yang aman belum dipakai.
- AVX-512, AMX dan komponen XSAVE lain mati; program yang memakainya mendapat #UD.
- Program biasa tetap soft-float. Menyalakan SSE untuk seluruh user space (atau SIMD untuk
  `spaceai`) adalah keputusan tersendiri yang harus membuktikan baseline numerik dulu (§12).
- #XM baru teruji di luar TCG; di TCG flag-nya dinyalakan tanpa jebakan.
- Heap kernel menanggung area setiap thread (dicek terhadap headroom saat spawn).
