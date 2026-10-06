# ADR-0032 — Serah terima yang diperiksa (BootInfo v3) dan status boot yang dapat dibedakan

- Status: diterima
- Tanggal: 2026-10-06
- Konteks: PRD v0.2 §7.1 (urutan startup), §7.2 (kontrak BootInfo), §7.3 (boot image), §7.4
  (status keberhasilan), §17 ("Parser image dan struktur boot diuji dengan ukuran, offset, dan
  rentang yang rusak"), B02, B03, B04; ADR-0002 (bootloader UEFI), ADR-0027 (recovery dan hitungan
  boot), ADR-0028 (AArch64)

## Konteks

BootInfo v2 hanya membawa magic dan versi sebagai identitas, dan kernel hanya memeriksa keduanya.
Sisanya dipercaya begitu saja: alamat memory map, jumlah entrinya, rentang initrd dan command line,
akar page table. Satu field yang rusak -- bug bootloader, tulisan nyasar -- berakhir sebagai page
fault di kernel, atau lebih buruk: frame allocator yang membagikan halaman milik kernel image. PRD
v0.2 meminta kontrak yang lengkap (ukuran total, flags, ukuran dan versi format descriptor memory
map, informasi validasi image, reservasi beserta pemiliknya, entropi, boot slot), kernel yang
menolak dengan pesan diagnostik, dan uji dengan struktur yang dirusak.

B04 meminta empat keadaan yang bisa dibedakan: kernel alive (kernel menulis log setelah serah
terima), user-space alive (proses terisolasi pertama berjalan), OS usable (shell bisa membaca berkas
serta menjalankan dan menghentikan program) dan AI ready (runtime model siap); keberhasilan boot untuk
rollback didasarkan pada kesehatan layanan inti, bukan pada inferensi.

## Keputusan

**1. BootInfo versi 3** (`abi/spaceabi/src/boot.rs`, 720 byte, satu halaman sendiri):

| Bagian PRD | Field |
|---|---|
| Identitas | `magic`, `version` = 3, `size` = 720, `flags` (bagian opsional yang ada) |
| Memory map | `memory_map` (alamat, panjang), `memory_map_entries`, `memory_map_entry_size` = 24, `memory_map_format` = 1, `mem_kind` per entri |
| Kernel dan boot image | `kernel_image`, `initrd`, `initrd_sha256` (diukur bootloader saat memuat) |
| Framebuffer | `framebuffer` (alamat, ukuran, dimensi, stride, format piksel) |
| Platform | `rsdp`; `uart`/`uart_kind` (AArch64, dari SPCR) |
| Parameter | `cmdline` ≤ 4096 byte, UTF-8; opsi recovery (`recovery=`) dan debugging (`selftest=`, `exit=`) ada di dalamnya |
| Reservasi | `reservations[16]` + `reservation_count`: rentang dan pemiliknya (kernel image, boot image, command line, boot stack, boot info, memory map, page table) |
| Entropi | `entropy`: sumber (`UEFI_RNG` atau tidak ada), panjang ≤ 64, byte |
| Boot slot | `boot_slot`: slot (normal/recovery), alasan (operator, failed-boots), percobaan sebelum boot ini (atau `NOT_COUNTED`), tempat state pemulihan (`/spaceos/var/boots.txt` di volume `SPACEDATA`) |

Setiap alamat fisik, dan dokumentasi field menyebut alignment, umur dan pemiliknya. BootInfo tidak
memuat secret: entropi bukan kunci, dan kernel menghapusnya dari struktur begitu diambil.

**2. Kernel memeriksa sebelum percaya** (`kernel/src/bootinfo.rs`), dengan validator yang sama yang
dipakai bootloader dan uji host:
- `validate_header` hanya membaca struktur itu sendiri: identitas, `phys_offset`, `phys_map_end`
  (kelipatan 2 MiB, paling jauh 16 TiB), letak dan ukuran memory map (jumlah × 24 dihitung dengan
  `checked_mul`), setiap rentang di dalam linear map dan mulai di awal halaman, framebuffer, UART,
  entropi, boot slot, dan setiap flag cocok dengan field-nya. Entri reservasi dan byte entropi
  di luar jumlahnya harus nol.
- `validate` lalu memeriksa memory map yang ditunjuk header (urut, tidak tumpang tindih, jenis
  dikenal, RAM di bawah `phys_map_end`, ada RAM bebas) dan reservasi terhadapnya (pemilik dikenal
  dan unik, halaman utuh, di dalam region `KERNEL` -- memori yang tidak pernah dibagikan frame
  allocator -- dan tidak saling tumpang tindih); setiap rentang yang diserahkan harus ada di dalam
  reservasi pemiliknya, termasuk struktur BootInfo itu sendiri.
- Setiap rentang yang akan dibaca dicari dulu di page table yang hidup (`mm::range_is_mapped`),
  karena linear map berlubang di antara RAM: alamat bohong berakhir sebagai penolakan, bukan fault.
  Vektor exception dipasang sebelum pemeriksaan.
- Boot image di-hash ulang dan harus sama dengan SHA-256 dari bootloader; command line harus UTF-8.

Penolakan adalah panic dengan field, nilainya dan aturan yang dilanggar (`BootInfo rejected:
version = 0x63: a BootInfo version this kernel does not read (expected 0x3)`): kernel tidak bisa
berjalan di atas serah terima yang tidak dipercayanya, dan jalur panic sudah melaporkan lalu
menghentikan mesin.

**3. Bootloader menyerahkan hanya yang bisa dijelaskan kontrak.** Semua page table diambil dari
satu pool yang dihitung untuk kasus terburuk, sehingga satu reservasi menutupinya; sisa pool
dikembalikan ke firmware. Framebuffer yang tidak memenuhi kontrak (tidak sejajar halaman, di luar
yang bisa dipetakan linear map) tidak diserahkan -- konsol serial saja. RAM di atas 16 TiB ditolak
dengan pesan. Command line diperiksa (UTF-8, ≤ 4096 byte) sebelum diserahkan.

**4. Entropi firmware dicampur, tidak dihitung.** Bootloader meminta 32 byte dari
`EFI_RNG_PROTOCOL` bila firmware punya. Kernel menyimpan hash-nya saja dan meng-XOR aliran
SHA-256(kunci ‖ counter) ke setiap byte yang diberikan `SYS_RANDOM`; keluaran perangkat keras yang
baik tetap baik, dan keluaran perangkat keras yang bisa ditebak tidak lagi bisa ditebak tanpa byte
firmware. Kualitasnya tidak diasumsikan: tanpa virtio-rng atau RDRAND, `SYS_RANDOM` tetap menolak.

**5. Kerusakan disengaja untuk uji.** `bootinfo_fault=<nama>` di `spaceos.cfg` merusak satu field
setelah semuanya dibangun, tepat sebelum lompat ke kernel (`boot/spaceboot/src/fault.rs`), dan
bootloader mengumumkannya di konsol sebelum boot services berakhir. Lima belas kerusakan: magic,
versi, ukuran, flags, `phys_offset`, jumlah entri memory map, entri yang tumpang tindih, kernel
image yang bergeser, boot image sepanjang linear map, digest boot image, command line bukan UTF-8,
reservasi yang tumpang tindih, entropi kepanjangan, boot slot 7, stride framebuffer.

**6. Empat status boot, masing-masing dengan kalimatnya sendiri** (B04):

| Status | Siapa yang mengatakannya, dan kapan |
|---|---|
| `[status] kernel alive` | kernel, baris pertama setelah banner -- log setelah serah terima |
| `[status] user-space alive` | kernel, pada system call pertama dari proses mana pun: proses terisolasi pertama menjalankan kodenya sendiri di address space-nya sendiri |
| `[status] OS usable` / `OS not usable` | `spaceshell`, pada `cmd::CHECK`: membaca berkas pertama yang berisi di `/spaceos`, menjalankan `bin/uiworker` sampai selesai dengan exit 0, dan menghentikan `bin/uiworker` lain yang berputar tanpa syscall (alasan `SIGNAL`) |
| `[status] AI ready` | `spaceai`, saat token pertama: model terverifikasi, dimuat di backend compute, dan menjawab |

`spaceterm` dan `spacedesk` meminta `CHECK` sekali setelah sesi siap, dan **hitungan boot
(ADR-0027) hanya kembali ke 0 bila OS usable** -- keberhasilan boot adalah kesehatan inti, bukan
inferensi atau SpaceLink. `init` menguji hal yang sama (`B04`): dengan volume data harus usable,
tanpa volume harus mengatakan tidak usable. Harness melaporkan status tertinggi setiap boot
(`reached OS usable`), dan setiap skenario punya status akhir yang harus dicapai dan tidak dilewati.

## Bukti

| Klaim | Bukti |
|---|---|
| Serah terima normal diterima | `[kernel] boot info v3 accepted: 720 bytes, 41 memory regions, 7 reservations, …` (acceptance; AArch64 26 region), tujuh baris `reserved …: kernel image / boot image / command line / boot stack / boot info / memory map / page tables` (page table 96 KiB untuk 8 GiB), `boot image: sha256 … as the bootloader measured it`, `boot slot: normal; boots are not counted`, `boot entropy: 32 bytes from the firmware's RNG protocol, mixed in and never counted as a source` |
| Setiap kerusakan ditolak dengan field dan aturannya | 15 skenario `bootinfo-*` (x86-64) dan `arm64-bootinfo-reservation`: masing-masing berakhir di `kernel alive` dengan pesannya sendiri, misalnya `kernel_image = 0x7d56d000: not inside the kernel image's reservation`, `initrd: the boot image hashes to 9f93…, not to the 6093… the bootloader measured`, `memory map start (entry 1) = 0x0: out of order, or overlapping the entry before it` |
| Entropi firmware tidak pernah dihitung | skenario `entropy-boot-only` (kernel mengabaikan virtio-rng dan RDRAND): `spaceboot: entropy: 32 bytes from the firmware's RNG protocol`, `[kernel] entropy: none; boot entropy from the firmware mixed in, not counted`, `[init] entropy: none; everything that needs keys will be skipped`, TLS dilewati dengan alasan |
| Validator di host | 11 uji `boot::tests`: setiap aturan punya kerusakan yang memicunya tepat; 200 000 kerusakan acak (struktur dan memory map) tidak pernah membuat validator panic (overflow check menyala), dan setiap struktur yang lolos diperiksa ulang secara independen terhadap janji yang dipegang kernel |
| Empat status dapat dibedakan | `init-missing-diagnosis`, `panic-diagnosis` dan `bootinfo-*` berakhir di `kernel alive`; `init-exit-diagnosis` (program yang hanya menyapa) dan recovery di `user-space alive`; `terminal`, `boot-count` di `OS usable` (`read 512 bytes of /spaceos/MODEL.SLM, ran bin/uiworker to its end and stopped another while it ran, in 33 ms`); `acceptance`, `desktop`, `stress`, AArch64 di `AI ready`; mesin compat `no-disk` di `user-space alive` dengan `OS not usable: cannot read files: …` |

## Gigi

| Pelemahan | Akibat |
|---|---|
| Aturan "RAM di luar linear map" dibuang dari validator | `the_memory_map_entries_are_checked` dan uji kerusakan acak gagal: struktur yang lolos menjanjikan RAM yang tidak dipetakan |
| Aturan "reservasi di dalam memori kernel" dibuang | `reservations_are_checked` dan uji kerusakan acak gagal: reservasi di RAM yang dibagikan allocator |
| `checked_mul` jumlah entri diganti perkalian yang membungkus | `the_memory_map_header_is_checked` gagal: 768 614 336 404 564 651 entri × 24 byte membungkus menjadi 8 byte dan lolos |
| Kernel hanya memeriksa header (tanpa memory map dan reservasi) | `bootinfo-reservation` gagal: BootInfo dengan dua reservasi bertumpuk diterima dan boot berjalan sampai `AI ready` (`exit code Some(33), expected 127`, `unexpected marker "[status] user-space alive"`) |
| Kernel tidak membandingkan digest boot image | `bootinfo-initrd_hash` gagal dengan cara yang sama: digest yang dibalik satu byte tidak menghentikan apa pun |
| Kernel mengaku `user-space alive` sebelum proses mana pun berjalan | `init-missing-diagnosis` gagal: `the boot ended at "user-space alive", expected "kernel alive" (B04)` |
| Sesi mengaku `OS usable` tanpa membaca berkas | compat `no-disk` gagal: `[init] FAIL B04` (sesi mengaku usable tanpa sistem berkas) dan `the boot ended at "OS usable", expected "user-space alive"` |
| `SYS_RANDOM` menjawab dari entropi firmware saat hanya itu yang ada | `entropy-boot-only` gagal: `unexpected marker "[init] entropy: available"`, `unexpected marker "[init] PASS TLS"` -- kunci TLS dibuat dari 32 byte yang kualitasnya tidak diketahui |

## Konsekuensi

- Setiap boot meng-hash boot image dua kali (bootloader dan kernel): sekitar 0,3–0,5 s tambahan di
  QEMU TCG untuk 4 MB, beberapa milidetik di perangkat keras.
- Di AArch64 konsol diambil dari BootInfo, jadi magic atau versi yang rusak membuat kernel diam:
  tidak ada yang bisa dipercaya untuk menemukan UART. Kernel AArch64 yang menolak juga tidak bisa
  mengakhiri QEMU sendiri (izin semihosting ada di command line yang tidak dipercaya); harness
  menghentikannya setelah log mengatakan berhenti.
- Reservasi belum dipakai untuk mereklamasi apa pun: boot stack, memory map dan identity map tetap
  milik kernel selamanya.
- Entropi firmware hanya dicampur; kernel belum punya CSPRNG sendiri. OVMF di QEMU hanya punya
  protokol RNG bila ada perangkat virtio-rng, dan kernel lalu memakai perangkat itu sendiri
  (`i440fx` dan `cpu-max`: `the firmware has no RNG protocol`); karena itu "tidak pernah dihitung"
  dibuktikan dengan sakelar uji `entropy=boot-only`, yang membuat kernel mengabaikan perangkat dan
  RDRAND.
- B03 tetap dibuktikan oleh mesin `no-disk`: root task dan semua program berasal dari boot image di
  RAM, tanpa driver disk yang menemukan perangkat; status boot di mesin itu berhenti di
  `user-space alive` karena tidak ada berkas untuk dibaca.
