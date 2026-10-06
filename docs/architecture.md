# Arsitektur yang diimplementasikan (tahap 1–5, jaringan, TLS dan layar tahap 3, dan layanan tahap 5A)

Peta ke lapisan PRD §2: repo ini mengisi baris **Kernel (Space Kernel dan HAL)**, **Layanan OS** (storage, jaringan, TLS, compute, sesi, broker, indeks, paket), **Runtime AI** untuk model referensi, adapter cloud pertama (diuji terhadap penyedia tiruan), dan desktop pertama: server tampilan, pengelola jendela dari keyboard, terminal, file manager dan Agent Center (ADR-0020). SDK dan aplikasi pihak ketiga belum ada.

```
UEFI (OVMF) ──► spaceboot (boot/)  ──► spacekernel (kernel/) ──► bin/init (user/init) ──► program uji (user/tests/*)
                 BootInfo (spaceabi::boot)     ABI v0 (spaceabi::syscall)   libspace (user/libspace)
```

## Dua arsitektur (ADR-0028)

Kode di luar `kernel/src/arch/` tidak menyebut satu pun instruksi atau register: walk page table ada sekali di `mm::pt` (x86-64 dan AArch64 sama-sama empat tingkat 512 entri), dan `arch::{x86_64,aarch64}` hanya menyandikan entri, mengurus root dan TLB, interrupt, context switch, entry syscall, konsol, PCI (port vs ECAM), waktu dan daya. Di AArch64: EL1/EL0, `TTBR1` kernel dan `TTBR0` proses, GICv3, timer generik, PL011, PSCI; boot lewat AAVMF (`BOOTAA64.EFI`). Uraian di bawah ini adalah jalur x86-64.

## Rantai boot

1. OVMF memuat `\EFI\BOOT\BOOTX64.EFI` (= `spaceboot`) dari ESP.
2. `spaceboot` membaca `spacekernel.elf`, `initrd.tar`, `spaceos.cfg`; memuat segmen kernel; menyalin initrd/cmdline ke memori bertipe KERNEL; membaca GOP dan RSDP; membangun page table (kernel higher-half, linear map RAM, identity sementara); keluar dari boot services; menormalkan memory map; mengaktifkan NXE/WP; melompat ke `_start` dengan `rdi = &BootInfo`.
3. `spacekernel::kmain`: serial (dengan probe keberadaan UART) → GDT/TSS (IST untuk double fault, NMI, `#DB`, machine check) → IDT (256 stub asm; hanya `int3` berDPL 3) → memori (bitmap frame, PML4 kernel baru, heap 16 MiB, slot kernel stack berguard) → pindah dari stack bootloader ke slot kernel stack berguard → framebuffer → cmdline → initrd → PIC/PIT → MSR syscall → jam (timer ACPI PM, ADR-0029) → scheduler → selftest kernel → spawn `bin/init` → idle loop.
4. `init` (user, ring 3) memegang handle Root dan menjalankan/menguji program lain.

## Layout memori virtual

| Rentang | Isi |
|---|---|
| `0x0000_0000_0040_0000` | kode/data program user (ELF) |
| `0x0000_0010_0000_0000…` | region `mem_map` dan pemetaan memory object: first-fit, rentang yang dilepas dipakai lagi beserta page table-nya, halaman penjaga di antara pemetaan (ADR-0019) |
| `0x0000_7FFF_EFFF_0000 – 0x7FFF_F000_0000` | stack user 64 KiB (NX) |
| `0xFFFF_8000_0000_0000` | linear map memori fisik (`PHYS_OFFSET`), NX |
| `0xFFFF_9000_0000_0000` | heap kernel |
| `0xFFFF_A000_0000_0000` | slot kernel stack 64 KiB (32 KiB terpeta + guard) |
| `0xFFFF_B000_0000_0000` | jendela MMIO perangkat (uncached, NX) |
| `0xFFFF_FFFF_8000_0000` | image kernel |

Setiap proses memiliki PML4 sendiri: half bawah privat, slot 256–511 disalin dari PML4 kernel (sub-tabel heap dan kernel stack dipra-alokasi agar pemetaan baru terlihat semua proses).

## Recovery (tahap 7, ADR-0027)

`spaceboot` memilih command line sebelum kernel berjalan: `cmdline=` untuk boot normal, atau `recovery=` (bawaan `init=bin/spacerecovery`) bila operator menekan R selama `recovery_wait_ms`, atau bila `boot_count=on` dan `\SPACEOS\VAR\BOOTS.TXT` di volume data — yang ditambah satu oleh bootloader setiap boot, lewat driver FAT firmware, dan dikembalikan ke 0 oleh desktop atau sesi terminal setelah naik — sudah mencapai tiga. Alasannya ikut di command line (`recovery=operator` atau `recovery=failed-boots`). `bin/spacerecovery` hanya butuh konsol dan volume data.

## Penyimpanan (tahap 3)

`pci` (port 0xCF8/0xCFC, atau ECAM di AArch64) menemukan perangkat; `virtio_blk` membawa setiap perangkat virtio-blk 1.0 modern (sampai delapan) ke keadaan siap (reset → ACKNOWLEDGE/DRIVER → negosiasi `VIRTIO_F_VERSION_1` → antrean 0 → DRIVER_OK) dan melayani pembacaan **dan penulisan** dengan polling berbatas (`VIRTIO_BLK_F_FLUSH` dinegosiasikan bila ditawarkan, `VIRTIO_BLK_F_RO` dicatat sehingga tulisan ditolak di depan); register perangkat dipetakan uncached di jendela MMIO. `fs::fat32` membaca dan menulis volume FAT32 dari perangkat itu, `SYS_FS_OPEN/READ/STAT` memberi user space akses baca berbasis capability, dan `SYS_FS_CREATE/WRITE` akses tulis di balik hak root `FS_WRITE` yang terpisah (ADR-0015). Hanya disk data yang terjangkau (ADR-0025): selain virtio-blk, `ahci` (setiap pengendali SATA; setiap port dihentikan lalu port dengan disk diberi command list, FIS dan command table sendiri; READ/WRITE DMA EXT, FLUSH CACHE EXT) dan `nvme` (reset, admin queue, satu pasang I/O queue, PRP) menemukan disk, dan `block` memilih **satu** volume data dari semuanya: FAT32 berlabel `SPACEDATA`, seluruh disk atau satu partisi GPT/MBR. `fs` menghitung sektor dari awal volume; `block` menolak permintaan yang melewati akhirnya sebelum driver melihatnya, jadi ESP — di disk lain atau di partisi lain disk yang sama — tidak terjangkau. Semua driver blok mem-poll; batas waktunya diukur dengan TSC karena driver berjalan sebelum timer dan di bawah spinlock.

## Jaringan (tahap 3, ADR-0016)

```
program ──sesi (allowlist)──► bin/spacenet ──SYS_NET_SEND/RECV──► nic: virtio_net | e1000 (kernel) ──► kartu
   ▲                            DHCP, ARP, IPv4, TCP (smoltcp), DNS lewat TCP
   └──── satu channel per koneksi TCP ────┘
```

Kernel hanya memindahkan frame Ethernet. `virtio_net` memakai transport virtio yang
sama dengan `virtio_blk` (`dev/virtio.rs`), polling, dengan 32 buffer terima dan 32
buffer kirim; tanpa virtio-net, `e1000` (Intel 82540EM/82545EM/82574L, ADR-0026)
memakai dua ring 32 deskriptor legacy dengan buffer 2 KiB. INTx dimatikan dan tick timer
yang membangunkan penunggu saat ring terima berisi. Kartu — apa pun drivernya — diwakili
objek kernel **lease** di lapisan `nic` yang hanya bisa dipegang satu proses
(`SYS_NET_OPEN` di balik hak root `NET`). `SYS_WAIT_ANY` menunggu channel,
proses, dan lease sekaligus, dengan batas waktu.

`bin/spacenet` adalah satu-satunya pemegang lease dan satu-satunya yang berbicara
TCP/IP. Operatornya (siapa pun yang menjalankannya — di uji, `init`) menyerahkan
lease, menunjuk server DNS, lalu membuat sesi dengan allowlist `host:port`. Program
yang memegang sesi bisa `HELLO`, `RESOLVE`, `CONNECT`, dan `STATS`; tujuan di luar
allowlist ditolak sebelum pencarian DNS atau paket apa pun. Setiap koneksi adalah
channel sendiri yang dibawa `CONNECT`; `libspace::net` membungkusnya sebagai
`Session` dan `TcpStream`.

## Entropi, unit FPU, dan TLS (ADR-0017)

`dev::entropy` mengisi `SYS_RANDOM` dari virtio-rng (transport virtio yang sama, polling)
atau, tanpa itu, dari RDRAND; tanpa keduanya panggilan itu menjawab `NotFound` — tidak ada
cadangan dari jam atau penghitung. `arch::cpu::init` mematikan unit FPU dan vektor untuk semua
ring (CR0.EM; CR4.OSFXSR/OSXMMEXCPT/OSXSAVE), karena kernel tidak menyimpan state-nya per
thread: instruksi x87 berakhir dengan kill `NO_FPU`, instruksi SSE/AVX dengan `INVALID_OPCODE`.
`cargo xtask build` mendekode setiap instruksi kernel dan program untuk membuktikan tidak ada
yang membutuhkan unit itu.

```
program ──► spacetls (pustaka) ──► rustls 0.23 (no_std, unbuffered) + penyedia RustCrypto
               │  keacakan: getrandom → SYS_RANDOM     waktu: TimeProvider → SYS_CLOCK_REALTIME
               └─ transport: TcpStream dari sesi spacenet (allowlist diperiksa lebih dulu)
```

`spacetls` bukan layanan melainkan pustaka yang ditautkan ke program yang membutuhkannya
(di uji: `bin/tlsprobe`). TLS 1.3 saja; tidak ada root CA bawaan — pemanggil menyerahkan
otoritas yang dipercayanya. Suite: ChaCha20-Poly1305, lalu AES-128/256-GCM dengan batas 2^24
record per kunci.

## Komputasi (tahap 4)

Kernel menambahkan satu objek: *memory object* (kumpulan frame yang dapat dipetakan beberapa proses) dengan `SYS_VMO_CREATE/MAP/SIZE`. Di atasnya, `bin/spacecompute` mengimplementasikan Space Compute ABI v0 di user space: kontrol lewat pesan channel berukuran tetap, tensor di memory object yang dipetakan kedua sisi, eksekusi bertahap dengan tenggat sehingga `WAIT` dapat timeout dan `CANCEL` dapat menghentikan pekerjaan.

## Inferensi (tahap 5)

`bin/spaceai` memuat model SpaceLM v0 dari disk guest, memverifikasi checksum-nya, lalu menjalankan dekode greedy: runtime memegang tata letak (cache KV, transposisi V, potongan buffer) sementara seluruh aritmetika berjalan lewat Compute ABI ke `bin/spacecompute`. Satu langkah dekode adalah 54 operasi Compute. Token yang dihasilkan dibandingkan dengan baseline yang dipatok di `/spaceos/baseline.txt`.

## Layanan Developer Preview (tahap 5A)

Semua berjalan di user space dan hanya memegang kapabilitas yang diserahkan
kepadanya — tidak ada yang bisa mematikan mesin atau membaca statistik kernel.

- **`bin/spaceshell`** (U01): memiliki sesi konsol, daftar berkas, dan masa hidup
  worker. Loopnya tidak pernah memblokir pada worker (`recv` non-blocking +
  `SYS_WAIT` bendera `NONBLOCK`), jadi worker yang crash, tidur, atau macet tanpa
  syscall tidak bisa menahan sesi. Punya terminal yang bisa diketik (`help`,
  `status`, `ls`, `run`, `stop`, `quit`). Diberi root yang dipersempit ke
  `SPAWN|FS|CONSOLE|DUP` (ADR-0011). Job `infer` (ADR-0021) menjalankan `bin/spaceai`
  di atas `bin/spacecompute`-nya sendiri; worker hanya diberi channel laporan, satu
  koneksi compute, dan akses baca berkas yang dipersempit sesi dari miliknya sendiri.
  Worker melapor setiap token, dan Stop memintanya berhenti di antara dua operasi
  compute sebelum sesi, setelah 1 detik, membunuhnya.
- **`bin/spacebroker` + `bin/spaceagent`** (G01): agent lahir hanya dengan satu
  channel — `fs_open` miliknya ditolak kernel. Broker memegang satu-satunya
  kapabilitas file (`FS`), memeriksa setiap path terhadap workspace
  komponen-per-komponen, dan mencatat setiap panggilan di audit log operator
  (ADR-0012).
- **`bin/spacelink`** (L01–L03): mengindeks korpus, memberi peringkat chunk,
  mencabut dokumen, dan menyusun context bundle di bawah anggaran byte. Setiap
  chunk membawa path, rentang byte, dan SHA-256 sehingga pemanggil bisa
  memverifikasi provenance-nya sendiri (ADR-0013). Satu berkas yang berubah
  diindeks ulang sendirian lewat `UPDATE`, di tempat chunk lamanya (ADR-0023).
- **`bin/spacepkg`** (P01): memasang paket yang terautentikasi HMAC-SHA256,
  menolak paket rusak/palsu/terpotong dengan alasannya tanpa mengubah apa pun, dan
  `ROLLBACK` mengembalikan payload versi sebelumnya (ADR-0014).
- **`bin/spacecloud`** (I01): adapter model cloud. Memegang trust anchor dan kredensial
  (diserahkan operator; ia sendiri **tanpa kapabilitas file**), sesi `spacenet` yang hanya
  boleh ke satu `host:port`, dan channel ke broker. Klien hanya bertanya (`spaceabi::cloud`):
  local-only ditolak dan biaya terburuk harus muat di budget **sebelum** apa pun dikirim;
  jawaban (Messages API: HTTP/1.1 + SSE di atas `spacetls`) diteruskan saat tiba dan diputus
  bila penyedia melewati reservasi; alat model (`read_file`) hanya lewat broker (ADR-0018).

```
klien ──Ask/Event──► bin/spacecloud ──sesi (api.cloud.test:443)──► bin/spacenet ──► penyedia
                          │  kredensial + trust anchor dari operator, tanpa hak FS
                          └──READ──► bin/spacebroker (scope /spaceos/ws, audit)
```

- **`bin/spacedesk` + `bin/deskapps`** (U01, ADR-0020): server tampilan. Memegang layar lewat
  lease kernel, menyusun jendela dari memori milik klien yang dipetakannya **read-only**, dan
  mengelola fokus, posisi, ukuran, minimize, empat workspace dan penutupan dari keyboard. Setiap
  aplikasi adalah proses `bin/deskapps` sendiri dengan satu kapabilitas: terminal dan Agent Center
  masing-masing sesi `spaceshell` sendiri (`SPAWN|FS`), file manager hanya `FS`. Server yang
  dijalankan proses lain menerima operatornya lewat channel bootstrap (`OP_*`): jalankan aplikasi,
  tekan tombol, baca keadaan jendela, dan minta jendela menjelaskan isinya dengan kata-kata — API
  otomasi yang dipakai uji U01. Command Center (ADR-0022) memunculkan `bin/spacelink`
  sendiri dengan akses baca saja, dan meminta desktop membuka file manager di sebuah path
  lewat pesan `OPEN` — permintaan, bukan kapabilitas.

```
keyboard ─IRQ1 / xHCI─► kernel (decoder) ─SYS_INPUT_READ─► bin/spacedesk ──KEY──► aplikasi yang fokus
                                                       │  ▲ CREATE/PRESENT (handle memori jendela, dibaca saja)
framebuffer ◄─lease (SYS_DISPLAY_OPEN, hak DISPLAY)────┘  └── bin/deskapps: terminal | files | agent
                                                                    └─ sesi spaceshell ─► worker
```

## Masukan konsol

Keyboard PS/2 (IRQ 1, scan code set 1), keyboard USB (ADR-0030) dan UART kedua COM2
(IRQ 3) mengisi satu ring buffer 256 byte di kernel. Keyboard USB ada di pengendali
xHCI yang dilihat dari tick CPU boot (tanpa interrupt): laporan boot protocol delapan
byte dibandingkan dengan yang sebelumnya, dan setiap tombol yang ditekan atau dilepas
menjadi scan code set 1 yang akan dikirim keyboard PS/2, lalu melewati decoder yang
sama — modifier, event desktop dan byte terminal tidak bisa berbeda antara keduanya. `SYS_CONSOLE_READ` menyerahkannya ke user space di
balik hak root `CONSOLE` dan **tidak pernah memblokir**. Kernel tidak melakukan
echo dan tidak mengenal baris; sesi yang menentukan semantik terminal. COM1 tetap
khusus keluaran log, termasuk dari handler panic.

Ring yang penuh membuang byte paling tua dan menghitungnya. Pembacaan berikutnya
mendapat `DataLoss` **sebelum** byte yang selamat diserahkan (tidak ada yang ikut
terbuang), lalu hitungannya dinolkan — jadi setiap episode kehilangan terlihat,
bukan hanya yang pertama, dan sesi bisa membuang baris yang setengah jadi alih-alih
menjalankan perintah yang tidak pernah diketik.

Dekoder yang sama juga menghasilkan **event** tombol untuk desktop (`SYS_INPUT_READ`, hak
`CONSOLE` yang sama, ring 128 event): tombol mana, ditekan atau dilepas, dengan Shift/Ctrl/Alt/Super
yang sedang ditahan, termasuk tombol extended. Tekanan dengan Ctrl, Alt atau Super tidak mengetik
byte apa pun — pintasan tidak pernah bocor menjadi teks. Event yang hilang karena ring penuh
dilaporkan (`kind::LOST`) sebelum event yang selamat.

`init=` pada command line kernel memilih proses user pertama: `bin/init` untuk
acceptance run, `init=bin/spaceterm` untuk sesi interaktif, `init=bin/spacedesk` untuk desktop —
semuanya dari image yang sama. Command line itu sendiri bisa dibaca lewat `SYS_CMDLINE` (hak root
`STATS`); `init` memakainya untuk `stress=` (ADR-0019).

## Layar (ADR-0020)

Kernel menggambar konsol teks di framebuffer GOP sampai ada yang meminta layar.
`SYS_DISPLAY_OPEN` (hak root `DISPLAY`) menyerahkan halaman-halaman framebuffer sebagai objek memori
(`MemoryKind::Display`: memori perangkat, tidak pernah masuk alokator frame) kepada **satu**
pemegang; lease kedua → `Busy`. `SYS_DISPLAY_INFO` memberi ukuran, stride, format (BGRX/RGBX) dan
offset piksel (0,0). Selama objek itu atau pemetaan mana pun darinya hidup, konsol kernel hanya
menulis ke serial; saat yang terakhir hilang (server keluar, crash, atau dibunuh) kernel
membersihkan layar dan mengambilnya kembali. Panic kernel selalu mengambil layar lebih dulu, jadi
yang terlihat adalah pesan panic, bukan desktop yang membeku.

## Objek kernel

- **Process**: address space + tabel handle + kuota + status keluar + antrean penunggu `wait`.
- **Thread**: satu per proses (MVP); kernel stack sendiri; context switch menyimpan register callee-saved (`switch_to`); masuk ring 3 lewat `iretq`; syscall lewat `syscall/sysret`.
- **Channel/Endpoint**: dua sisi, antrean pesan terbatas, wait queue penerima, penutupan sisi membangunkan peer.
- **File**: berkas terbuka pada volume FAT32 (hak `READ`).
- **Memory**: frame bersama yang dapat dipetakan beberapa proses (hak `READ|WRITE|MAP`), atau halaman framebuffer yang di-lease (`MemoryKind::Display`), yang kembali ke konsol, bukan ke alokator frame.
- **Nic**: lease atas kartu jaringan (hak `READ` menerima, `WRITE` mengirim); hanya ada satu, dan lepas saat handle terakhirnya ditutup.
- **Root**: capability istimewa `init` (spawn dari initrd, statistik dan command line, shutdown, fault injection, akses berkas, konsol, kartu jaringan, layar), dengan satu hak per kemampuan. Setiap layanan menerima turunan yang **sudah dipersempit**: `spaceshell` hanya `SPAWN|FS`, `spacebroker`/`spacelink`/`spacepkg` hanya `FS`, `spaceagent` tidak menerima root sama sekali.

## Scheduler

Round-robin preemptif di **setiap CPU** (ADR-0024): satu run queue untuk semua CPU, dan setiap CPU punya thread idle, thread berjalan, dan kuantum 10 ms sendiri. CPU boot memegang waktu (tick PIT 1 kHz: `ticks`, sleeper; jumlah tick mengikuti counter timer ACPI PM, sehingga tick yang hilang tidak menghilangkan waktu — ADR-0029) dan menerima semua IRQ PIC; CPU lain memakai timer local APIC 100 Hz hanya untuk mengakhiri kuantum, dan CPU yang idle dibangunkan dengan IPI "ada pekerjaan". CPU-CPU lain dinyalakan saat boot dari MADT ACPI lewat INIT–SIPI–SIPI dan trampoline di bawah 1 MiB. Sebuah thread hanya ada di run queue selama tidak ada CPU di stack-nya (`on_cpu`): CPU yang meninggalkannya memasukkannya kembali setelah switch selesai (`finish_switch`), dan wake-up yang menemukannya masih di CPU hanya menandainya `Ready`. Thread yang mati direklamasi oleh thread yang berjalan berikutnya di CPU yang sama, sehingga stack kernel tidak dibebaskan oleh pemiliknya sendiri; status keluar prosesnya baru diumumkan setelah itu, jadi `wait` kembali ketika semua milik anak sudah kembali. Semua blocking (`recv`, `wait`, `sleep`, `wait_any`) menandai thread `Blocked` dan mendaftar ke wait queue/sleepers sebelum tidur, lalu melihat sekali lagi — kondisinya (`wait_any`) dan kill yang tertunda (`sched::block`) — agar wake-up atau kill dari CPU lain tidak hilang; `wait_any` mendaftar ke setiap antrean yang ditunggunya sekaligus dan keluar dari semuanya, apa pun yang membangunkannya. CPU selalu memuat CR3 thread tujuan pada setiap switch (CR3 kernel saat idle); dengan satu thread per proses itu cukup untuk tidak pernah membutuhkan TLB shootdown.

## Terminasi dan diagnosis

- Exception ring 3 → `[kernel] pid N '…' killed: <exception> at rip=… (error=…, addr=…)` → `exit_current` → orang tua menerima `ExitStatus`.
- Exception ring 0 / panic → `!!! CPU EXCEPTION IN KERNEL MODE …` dump register + `!!! KERNEL PANIC !!!` + backtrace frame pointer + `isa-debug-exit(0x3f)`.
- Overflow stack kernel → guard page → double fault pada stack IST → dump + panic (diuji oleh `selftest=stack`).
- `#DB` ring 0 dari `syscall` dengan TF → stack IST debug, TF dibersihkan, lanjut; proses mati saat trap terulang di ring 3. NMI → dicatat, diabaikan.
- Loader menolak ELF dengan entry di luar segmen executable, magic salah, terpotong, atau segmen melampaui kuota/ruang user (`NoExec`/`Quota`).
