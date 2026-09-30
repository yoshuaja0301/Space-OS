# Space OS

Sistem operasi AI-native dengan kernel baru yang dibangun dari nol (Rust, x86-64, UEFI).
Repo ini mengimplementasikan **tahap 1–5** dan sebagian **5A** dari roadmap
[PRD v0.1](docs/prd/Space_OS_PRD_v0_1.md): bootloader UEFI sendiri, microkernel berorientasi
capability, user-space dengan syscall/IPC/kuota, VirtIO block + FAT32 baca-tulis + ABI file, jaringan
(virtio-net + TCP/IP di user space dengan allowlist tujuan per sesi), TLS 1.3 dengan kunci dari
sumber entropi kernel, objek memori
bersama + Space Compute ABI v0, inferensi model native yang cocok dengan baseline yang dipatok,
serta layanan Developer Preview (desktop grafis dengan terminal, file manager, Agent Center yang
menjalankan model dengan Stop kooperatif, dan Command Center untuk pencarian SpaceLink,
sesi terminal, tool broker, SpaceLink, paket bertanda tangan, adapter cloud) — dengan bukti uji
otomatis untuk persyaratan **K01, K02, K03, D01, C01, A01, U01, G01, L01–L03, P01** di QEMU, **I01
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
| `spaceboot` | `boot/spaceboot` | `x86_64-unknown-uefi` | Bootloader UEFI: muat kernel + initrd, page table higher-half, memory map, GOP, lompat ke kernel |
| `spacekernel` | `kernel` | `x86_64-unknown-none` | Microkernel: GDT/IDT/TSS, frame allocator, paging per proses, heap, kernel stack berguard, scheduler preemptif, ring 3, `syscall/sysret`, channel IPC, `wait_any` bertimeout, tabel capability, kuota, crash log, PCI + virtio-blk + FAT32 baca-tulis, virtio-net (lease frame), RTC, sumber entropi (virtio-rng/RDRAND, tanpa cadangan lemah), unit FPU/vektor dimatikan, konsol framebuffer + masukan keyboard/serial |
| `libspace` | `user/libspace` | `x86_64-unknown-none` | Runtime user: `_start`, wrapper syscall, heap, `println!`, klien jaringan (`Session`, `TcpStream`) |
| `spacenet` | `user/services/spacenet` | `x86_64-unknown-none` | Layanan jaringan: DHCP, ARP, IPv4, TCP (smoltcp), DNS lewat TCP; program lain hanya lewat sesi dengan allowlist `host:port` (ADR-0016) |
| `spacetls` | `user/spacetls` | `x86_64-unknown-none` | Pustaka klien TLS 1.3: rustls (no_std) + RustCrypto di jalur perangkat lunak, kunci dari `SYS_RANDOM`, waktu dari RTC, tanpa root CA bawaan (ADR-0017) |
| `spacecloud` | `user/services/spacecloud` | `x86_64-unknown-none` | Adapter cloud (I01): memegang kredensial dan sesi ke satu alamat, streaming SSE di atas TLS, alat model hanya lewat Tool Broker, budget dan retry terbatas, local-only ditolak (ADR-0018) |
| `spacecompute` | `user/services/spacecompute` | `x86_64-unknown-none` | Layanan Space Compute ABI v0 di user space, backend CPU |
| `spaceai` | `user/services/spaceai` | `x86_64-unknown-none` | Runtime AI: memuat SpaceLM v0 dari disk, verifikasi checksum, generate token lewat Compute ABI |
| `spacedesk` + `deskapps` | `user/services/spacedesk`, `user/services/deskapps` | `x86_64-unknown-none` | Desktop (U01, ADR-0020): server tampilan yang memegang layar lewat lease kernel, menyusun jendela dari memori klien (dibaca saja), dock, workspace dan semua manajemen jendela dari keyboard, API otomasi; aplikasinya terminal, file manager, Agent Center (model sungguhan, progres, Stop kooperatif; ADR-0021) dan Command Center (pencarian SpaceLink dengan asal setiap hasil; ADR-0022) |
| `spaceshell` + `spaceterm` | `user/services/spaceshell`, `user/services/spaceterm` | `x86_64-unknown-none` | Sesi yang bertahan melewati worker yang crash/macet, daftar berkas, `Stop`; `spaceterm` mem-boot langsung ke sesi yang bisa diketik orang (U01) |
| `spacebroker` + `spaceagent` | `user/services/*` | `x86_64-unknown-none` | Tool Broker dengan scope workspace dan audit log; agent yang lahir tanpa kapabilitas file (G01) |
| `spacelink` | `user/services/spacelink` | `x86_64-unknown-none` | Indeks korpus, revokasi yang bertahan indeks ulang, context bundle dengan provenance (L01–L03) |
| `spacepkg` | `user/services/spacepkg` | `x86_64-unknown-none` | Paket terautentikasi (HMAC-SHA256), penolakan yang menyebut alasan, rollback (P01) |
| `init` + uji | `user/init`, `user/tests/*` | `x86_64-unknown-none` | Proses pertama sekaligus penggerak 115 uji penerimaan K01–K03, D01, C01, A01, U01, G01, L01–L03, P01, I01, jaringan (`NET`) dan `TLS`; dengan `stress=` di command line kernel, suite itu diulang dalam satu boot dengan putaran pembunuhan acak (ADR-0019) |
| `xtask` | `xtask` | host | `cargo xtask build/run/test/compat/soak/stress/unit/ci`: image FAT (MBR+ESP), QEMU + OVMF, ketikan dan kombinasi tombol ke guest serta screenshot, layanan jaringan dan TLS lab dengan otoritas sertifikatnya, penyedia cloud tiruan, rekaman pcap yang diperiksa, pemeriksaan bahwa tidak ada instruksi FPU/vektor di image, verifikasi log dan exit code |

Semua yang berjalan di guest adalah kode Space OS; tidak ada Linux, libc, atau inferensi host di jalur uji (PRD §1 "definisi native").

## Mulai cepat

```bash
sudo apt install qemu-system-x86 ovmf     # Ubuntu 24.04; rustup memasang toolchain+target otomatis
cargo xtask test                           # build semua target, buat image, 11 skenario boot di QEMU
cargo xtask compat                         # image yang sama di 10 konfigurasi mesin (ADR-0010)
cargo xtask unit                           # uji unit host (codec DNS, tata letak pesan, server DNS lab)
cargo xtask run                            # boot acceptance, serial di terminal (Ctrl-A X keluar)
cargo xtask run --gui --cmdline "init=bin/spaceterm"           # sesi yang bisa diketik, lewat keyboard jendela QEMU
cargo xtask run --serial-input --cmdline "init=bin/spaceterm"  # sama, tanpa jendela: ketik ke pty COM2 yang dicetak QEMU
cargo xtask run --gui --cmdline "init=bin/spacedesk"          # desktop grafis (Super+Enter terminal, Super+A Agent Center, Super+Space cari, Alt+Tab, Ctrl+Alt+Delete mati)
cargo xtask soak --boots 100               # K01: 100 cold boot berturut-turut
cargo xtask stress --minutes 480           # PRD §9: suite berulang 8 jam dalam satu boot, hasil di build/stress/
```

Keluaran acceptance (dipotong):

```
spaceboot 0.1.0: Space OS UEFI bootloader
spacekernel 0.1.0: Space OS kernel booting
[kernel] selftest: heap ok, frames ok, paging ok, address-space ok, input decoding ok
[kernel] spawn pid 1 'bin/init': entry=0x453344, 133 pages mapped, quota 2048 pages
[init] Space OS init running: pid 1, ABI v0, quota 2048 pages (133 used)
[kernel] pid 3 'bin/fault' killed: page fault at rip=0x40041c (error=0x7, addr=0xffff800000000000)
[init] PASS K02: write to kernel memory kills the process (page fault)
...
[init] frames free before=2090017 after=2090017 ; heap used before=3432 after=3432 ; switches=165
[init] PASS K03: 50 spawn/exit cycles leak no frames and no kernel heap
[ai] model verified: sha256 a1955def6c7b4e8e...
[ai] generated 128 tokens offline, all matching the pinned baseline
[init] desktop: agent: worker 'hang' stopped; Stop has nothing to stop; 11 commands served (43 ms after Stop was pressed)
[init] PASS U01: the desktop, terminal, file manager and Stop keep working while inference workers crash
[kernel] console input: 8 byte(s) dropped, the buffer was full
[shell] input was lost; the line was discarded
[init] PASS U01: input lost to a full buffer is reported before the bytes that survived
[init] desktop: agent: worker 'infer' stopped between two steps after 9 of 128 tokens (Stop took 3 ms); 9/128 tokens, 9 matching; Stop has nothing to stop; 26 commands served (11 ms after Stop was pressed)
[init] PASS U01: the model writes text in the Agent Center, Stop ends it between two steps, and the terminal answers
[init] desktop: command: 3 results for 'channel'; selected 1 /spaceos/docs/IPC.TXT bytes 0+186 sha 0d37dfc6; index 4 docs 7 chunks 0 revoked
[init] PASS L01: the Command Center searches the index, shows where each result came from, bundles it and opens it in Files
[init] ALL TESTS PASSED (115/115, 0 skipped)
[kernel] shutdown requested by pid 1 'bin/init' with code 0 (uptime 34168 ms, 91513 context switches)
```

## Dokumentasi

- [docs/architecture.md](docs/architecture.md) — rantai boot, layout memori, objek kernel, scheduler, diagnosis.
- [docs/adr/](docs/adr/README.md) — keputusan arsitektur (microkernel/Rust stable, bootloader UEFI, profil QEMU, ABI v0, ELF/initrd, PIC/PIT, storage, compute, layanan 5A, jaringan, TLS, adapter cloud, uji stabilitas, desktop, Stop kooperatif, Command Center).
- [docs/requirements.md](docs/requirements.md) — traceability K01…H02 dengan status planned/experimental/verified.
- [docs/testing.md](docs/testing.md) dan [docs/evidence/](docs/evidence/) — skenario uji, marker, kode keluar, log bukti.
- [docs/build.md](docs/build.md) — prasyarat dan perintah.
- [docs/limitations.md](docs/limitations.md) — batas yang diketahui.
- [docs/roadmap.md](docs/roadmap.md) — tahap PRD vs kondisi repo, backlog berikutnya.
- [docs/gpu-feasibility.md](docs/gpu-feasibility.md) — studi kelayakan akselerator (tahap 6): apa yang sudah siap, apa yang menghalangi, dan kenapa drivernya belum ditulis.

## Lisensi

MIT (lihat `LICENSE`). Dependensi: `x86_64`, `linked_list_allocator`, `noto-sans-mono-bitmap`, `fatfs`, `tar` (MIT/Apache-2.0), `uefi` (MPL-2.0), `smoltcp` (0BSD) beserta `managed` (0BSD), `heapless`, `byteorder`, `bitflags`, `cfg-if`, `hash32`, `stable_deref_trait` (MIT/Apache-2.0).
