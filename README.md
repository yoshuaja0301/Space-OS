# Space OS

Sistem operasi AI-native dengan kernel baru yang dibangun dari nol (Rust, x86-64 dan AArch64, UEFI).
Repo ini mengimplementasikan **tahap 1–5** dan sebagian **5A** dari roadmap
[PRD v0.1](docs/prd/Space_OS_PRD_v0_1.md): bootloader UEFI sendiri, microkernel berorientasi
capability yang menjalankan thread di **semua CPU** (SMP), user-space dengan syscall/IPC/kuota, VirtIO block + FAT32 baca-tulis + ABI file, jaringan
(virtio-net + TCP/IP di user space dengan allowlist tujuan per sesi), TLS 1.3 dengan kunci dari
sumber entropi kernel, objek memori
bersama + Space Compute ABI v0, inferensi model native yang cocok dengan baseline yang dipatok,
serta layanan Developer Preview (desktop grafis dengan terminal, file manager, Agent Center yang
menjalankan model dengan Stop kooperatif, dan Command Center untuk pencarian SpaceLink,
sesi terminal, tool broker, SpaceLink, paket bertanda tangan, adapter cloud) — dengan bukti uji
otomatis untuk persyaratan **K01, K02, K03, D01, C01, A01, U01, G01, L01–L03, P01** di QEMU — suite yang sama juga
lulus di **ARM64** (QEMU `virt` + AAVMF, H02) —, **I01
terhadap penyedia cloud tiruan** di jaringan lab (belum pernah terhadap layanan sungguhan), dan uji
stabilitas PRD §9: seluruh suite berulang dalam satu boot selama berjam-jam dengan pembunuhan acak dan
memori yang harus kembali tepat. Tahap 6 dijawab sebatas [studi kelayakannya](docs/gpu-feasibility.md);
drivernya belum ditulis, dan alasannya ada di sana.

> Status: MVP kernel, bukan produk. Lihat [docs/limitations.md](docs/limitations.md) sebelum
> menyimpulkan apa pun dari angka di sini.

## Apa yang ada

| Komponen | Direktori | Target | Peran |
|---|---|---|---|
| `spaceabi` | `abi/spaceabi` | no_std | Kontrak bersama: protokol boot, nomor syscall, error, hak handle, parser ELF64/ustar |
| `spaceboot` | `boot/spaceboot` | `x86_64-unknown-uefi`, `aarch64-unknown-uefi` | Bootloader UEFI: muat kernel + initrd, pilih boot normal atau recovery (tombol R, hitungan boot di volume data; ADR-0027), page table higher-half, memory map, GOP, lompat ke kernel |
| `spacekernel` | `kernel` | `x86_64-unknown-none`, `aarch64-unknown-none-softfloat` | Microkernel (x86-64 dan AArch64, ADR-0028): GDT/IDT/TSS per CPU, frame allocator, paging per proses, heap, kernel stack berguard, scheduler preemptif untuk semua CPU (AP dari MADT ACPI, timer local APIC, IPI; ADR-0024), ring 3, `syscall/sysret`, channel IPC, `wait_any` bertimeout, tabel capability, kuota, crash log, PCI + virtio-blk, AHCI (SATA) dan NVMe dengan volume data dari labelnya (ADR-0025) + FAT32 baca-tulis, virtio-net dan Intel e1000 (satu lease frame, ADR-0026), RTC, sumber entropi (virtio-rng/RDRAND, tanpa cadangan lemah), unit FPU/vektor dimatikan, konsol framebuffer + masukan keyboard/serial |
| `libspace` | `user/libspace` | `x86_64-unknown-none` | Runtime user: `_start`, wrapper syscall, heap, `println!`, klien jaringan (`Session`, `TcpStream`) |
| `spacenet` | `user/services/spacenet` | `x86_64-unknown-none` | Layanan jaringan: DHCP, ARP, IPv4, TCP (smoltcp), DNS lewat TCP; program lain hanya lewat sesi dengan allowlist `host:port` (ADR-0016) |
| `spacetls` | `user/spacetls` | `x86_64-unknown-none` | Pustaka klien TLS 1.3: rustls (no_std) + RustCrypto di jalur perangkat lunak, kunci dari `SYS_RANDOM`, waktu dari RTC, tanpa root CA bawaan (ADR-0017) |
| `spacecloud` | `user/services/spacecloud` | `x86_64-unknown-none` | Adapter cloud (I01): memegang kredensial dan sesi ke satu alamat, streaming SSE di atas TLS, alat model hanya lewat Tool Broker, budget dan retry terbatas, local-only ditolak (ADR-0018) |
| `spacecompute` | `user/services/spacecompute` | `x86_64-unknown-none` | Layanan Space Compute ABI v0 di user space, backend CPU |
| `spaceai` | `user/services/spaceai` | `x86_64-unknown-none` | Runtime AI: memuat SpaceLM v0 dari disk, verifikasi checksum, generate token lewat Compute ABI |
| `spacedesk` + `deskapps` | `user/services/spacedesk`, `user/services/deskapps` | `x86_64-unknown-none` | Desktop (U01, ADR-0020): server tampilan yang memegang layar lewat lease kernel, menyusun jendela dari memori klien (dibaca saja), dock, workspace dan semua manajemen jendela dari keyboard, API otomasi; aplikasinya terminal, file manager, Agent Center (model sungguhan, progres, Stop kooperatif; ADR-0021) dan Command Center (pencarian SpaceLink dengan asal setiap hasil; ADR-0022) |
| `spacerecovery` | `user/services/spacerecovery` | `x86_64-unknown-none` | Konsol recovery (ADR-0027): dipilih bootloader saat operator menekan R atau setelah tiga boot yang tidak naik; memeriksa model terhadap manifest, store paket, revokasi dan hitungan boot, mengosongkan berkas yang rusak, membuka sesi — tanpa model, jaringan atau AI |
| `spaceshell` + `spaceterm` | `user/services/spaceshell`, `user/services/spaceterm` | `x86_64-unknown-none` | Sesi yang bertahan melewati worker yang crash/macet, daftar berkas, `Stop`; `spaceterm` mem-boot langsung ke sesi yang bisa diketik orang (U01) |
| `spacebroker` + `spaceagent` | `user/services/*` | `x86_64-unknown-none` | Tool Broker dengan scope workspace dan audit log; agent yang lahir tanpa kapabilitas file (G01) |
| `spacelink` | `user/services/spacelink` | `x86_64-unknown-none` | Indeks korpus, revokasi yang bertahan indeks ulang, context bundle dengan provenance (L01–L03), satu berkas yang berubah diindeks ulang tanpa full rescan (ADR-0023) |
| `spacepkg` | `user/services/spacepkg` | `x86_64-unknown-none` | Paket terautentikasi (HMAC-SHA256), penolakan yang menyebut alasan, rollback (P01) |
| `init` + uji | `user/init`, `user/tests/*` | `x86_64-unknown-none` | Proses pertama sekaligus penggerak 122 uji penerimaan B04, K01–K04, D01, C01, A01, A02, U01, G01, L01–L03, P01, I01, jaringan (`NET`) dan `TLS`; dengan `stress=` di command line kernel, suite itu diulang dalam satu boot dengan putaran pembunuhan acak (ADR-0019) |
| `xtask` | `xtask` | host | `cargo xtask build/run/test/compat/soak/stress/unit/ci`: image FAT (MBR+ESP), QEMU + OVMF, ketikan dan kombinasi tombol ke guest serta screenshot, layanan jaringan dan TLS lab dengan otoritas sertifikatnya, penyedia cloud tiruan, rekaman pcap yang diperiksa, pemeriksaan bahwa tidak ada instruksi FPU/vektor di image, verifikasi log dan exit code |

Setiap komponen guest juga dibangun untuk `aarch64-unknown-none-softfloat` (bootloader: `aarch64-unknown-uefi`); `cargo xtask arm64` mem-boot hasilnya (ADR-0028).

Semua yang berjalan di guest adalah kode Space OS; tidak ada Linux, libc, atau inferensi host di jalur uji (PRD §1 "definisi native").

## Mulai cepat

```bash
sudo apt install qemu-system-x86 ovmf     # Ubuntu 24.04; rustup memasang toolchain+target otomatis
cargo xtask test                           # build semua target, buat image, 32 skenario boot di QEMU
cargo xtask compat                         # image yang sama di 16 konfigurasi mesin (ADR-0010)
cargo xtask unit                           # uji unit host (codec DNS, tata letak pesan, server DNS lab)
cargo xtask run                            # boot acceptance, serial di terminal (Ctrl-A X keluar)
cargo xtask run --gui --cmdline "init=bin/spaceterm"           # sesi yang bisa diketik, lewat keyboard jendela QEMU
cargo xtask run --serial-input --cmdline "init=bin/spaceterm"  # sama, tanpa jendela: ketik ke pty COM2 yang dicetak QEMU
cargo xtask run --gui --cmdline "init=bin/spacedesk"          # desktop grafis (Super+Enter terminal, Super+A Agent Center, Super+Space cari, Alt+Tab, Ctrl+Alt+Delete mati)
cargo xtask soak --boots 100               # K01: 100 cold boot berturut-turut
cargo xtask stress --minutes 480           # PRD §9: suite berulang 8 jam dalam satu boot, hasil di build/stress/
cargo xtask disk-image                     # satu disk GPT untuk dipasang (ESP + volume data): dd ke disk/stik USB (ADR-0027)
cargo xtask arm64                          # semuanya untuk AArch64, di-boot di QEMU virt + AAVMF (ADR-0028)
```

Keluaran acceptance (dipotong):

```
spaceboot 0.1.0: Space OS UEFI bootloader
spaceboot: boot image 4050432 bytes, sha256 9f93593a...
spaceboot: entropy: 32 bytes from the firmware's RNG protocol
spacekernel 0.1.0: Space OS kernel booting
[status] kernel alive
[kernel] boot info v3 accepted: 720 bytes, 41 memory regions, 7 reservations, initrd 4050432 bytes, cmdline 13 bytes, rsdp 0x7f77e014
[kernel] boot image: sha256 9f93593a... as the bootloader measured it
[kernel] selftest: heap ok, frames ok, paging ok, address-space ok, input decoding ok
[kernel] spawn pid 1 'bin/init': entry=0x455ae0, 138 pages mapped, quota 2048 pages
[status] user-space alive: pid 1 (bin/init) made the first system call
[init] Space OS init running: pid 1, ABI v0, quota 2048 pages (138 used)
[kernel] pid 3 'bin/fault' killed: page fault at rip=0x40041c (error=0x7, addr=0xffff800000000000)
[init] PASS K02: write to kernel memory kills the process (page fault)
...
[kernel] smp: 4 CPUs online (boot CPU local APIC 0; started 1 2 3), xAPIC APIC mode, AP timer 100 Hz (620450 counts)
[init] smp: 4 CPUs; 200 turns through shared memory, no system call, in 1 ms
[init] PASS K02: two processes run at the same time on different CPUs
[init] frames free before=2089875 after=2089875 ; heap used before=9808 after=9808 ; switches=409
[init] PASS K03: 50 spawn/exit cycles leak no frames and no kernel heap
[status] OS usable: read 512 bytes of /spaceos/MODEL.SLM, ran bin/uiworker to its end and stopped another while it ran, in 28 ms
[init] PASS B04: a session says the OS is usable exactly when it can read files and run and stop programs
[ai] model verified: sha256 a1955def6c7b4e8e...
[status] AI ready: the model is verified and loaded on the CPU backend and answered after 366 ms
[ai] generated 128 tokens offline, all matching the pinned baseline
[init] desktop: agent: worker 'hang' stopped; Stop has nothing to stop; 11 commands served (30 ms after Stop was pressed)
[init] PASS U01: the desktop, terminal, file manager and Stop keep working while inference workers crash
[init] A02: /spaceos/bitflip.slm refused before a single step: a20bcbf9bf011b47868c does not match the manifest
[worker] job 'hog' took 4111912 KiB, then the kernel said: out of memory
[init] A02: 3 worker(s) took 8162 MiB in 22917 ms and left 0 frames free; the session answered, a new program was refused, and all 2089722 frames came back
[init] PASS A02: workers that take every free frame are refused at the end of it, the session answers, a new program is refused, and Stop gives every frame back
[kernel] console input: 8 byte(s) dropped, the buffer was full
[shell] input was lost; the line was discarded
[init] PASS U01: input lost to a full buffer is reported before the bytes that survived
[init] desktop: agent: worker 'infer' stopped between two steps after 9 of 128 tokens (Stop took 4 ms); 9/128 tokens, 9 matching; Stop has nothing to stop; 26 commands served (14 ms after Stop was pressed)
[init] PASS U01: the model writes text in the Agent Center, Stop ends it between two steps, and the terminal answers
[init] desktop: command: 3 results for 'channel'; selected 1 /spaceos/docs/IPC.TXT bytes 0+186 sha 0d37dfc6; index 4 docs 7 chunks 0 revoked
[init] PASS L01: the Command Center searches the index, shows where each result came from, bundles it and opens it in Files
[init] link: /spaceos/ws/FRESH.TXT changed; re-indexed on its own in 2 ms (109 bytes read); /spaceos/ws/LATER.TXT, changed too, kept its old text until it was named
[init] PASS L01: a changed file is re-indexed on its own, and the next search answers from the new text
[init] ALL TESTS PASSED (122/122, 1 skipped)
[kernel] shutdown requested by pid 1 'bin/init' with code 0 (uptime 35115 ms, 127325 context switches)
```

## Dokumentasi

- [docs/architecture.md](docs/architecture.md) — rantai boot, layout memori, objek kernel, scheduler, diagnosis.
- [docs/adr/](docs/adr/README.md) — keputusan arsitektur (microkernel/Rust stable, bootloader UEFI, profil QEMU, ABI v0, ELF/initrd, PIC/PIT, storage, compute, layanan 5A, jaringan, TLS, adapter cloud, uji stabilitas, desktop, Stop kooperatif, Command Center, kesegaran SpaceLink).
- [docs/requirements.md](docs/requirements.md) — traceability K01…H02 dengan status planned/experimental/verified.
- [docs/testing.md](docs/testing.md) dan [docs/evidence/](docs/evidence/) — skenario uji, marker, kode keluar, log bukti.
- [docs/build.md](docs/build.md) — prasyarat dan perintah.
- [docs/limitations.md](docs/limitations.md) — batas yang diketahui.
- [docs/roadmap.md](docs/roadmap.md) — tahap PRD vs kondisi repo, backlog berikutnya.
- [docs/gpu-feasibility.md](docs/gpu-feasibility.md) — studi kelayakan akselerator (tahap 6): apa yang sudah siap, apa yang menghalangi, dan kenapa drivernya belum ditulis.

## Lisensi

MIT (lihat `LICENSE`). Dependensi: `x86_64`, `linked_list_allocator`, `noto-sans-mono-bitmap`, `fatfs`, `tar` (MIT/Apache-2.0), `uefi` (MPL-2.0), `smoltcp` (0BSD) beserta `managed` (0BSD), `heapless`, `byteorder`, `bitflags`, `cfg-if`, `hash32`, `stable_deref_trait` (MIT/Apache-2.0).
