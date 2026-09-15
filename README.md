# Space OS

Sistem operasi AI-native dengan kernel baru yang dibangun dari nol (Rust, x86-64, UEFI).
Repo ini mengimplementasikan **tahap 1 (Kernel boot) dan tahap 2 (User space CPU)** dari roadmap
[PRD v0.1](docs/prd/Space_OS_PRD_v0_1.md): bootloader UEFI sendiri, microkernel berorientasi
capability, user-space dengan syscall/IPC/kuota, dan bukti uji otomatis untuk persyaratan
**K01, K02, K03** di QEMU.

> Status: MVP kernel, bukan produk. Lihat [docs/limitations.md](docs/limitations.md) sebelum
> menyimpulkan apa pun dari angka di sini.

## Apa yang ada

| Komponen | Direktori | Target | Peran |
|---|---|---|---|
| `spaceabi` | `abi/spaceabi` | no_std | Kontrak bersama: protokol boot, nomor syscall, error, hak handle, parser ELF64/ustar |
| `spaceboot` | `boot/spaceboot` | `x86_64-unknown-uefi` | Bootloader UEFI: muat kernel + initrd, page table higher-half, memory map, GOP, lompat ke kernel |
| `spacekernel` | `kernel` | `x86_64-unknown-none` | Microkernel: GDT/IDT/TSS, frame allocator, paging per proses, heap, kernel stack berguard, scheduler preemptif, ring 3, `syscall/sysret`, channel IPC, tabel capability, kuota, crash log |
| `libspace` | `user/libspace` | `x86_64-unknown-none` | Runtime user: `_start`, wrapper syscall, heap, `println!` |
| `init` + uji | `user/init`, `user/tests/*` | `x86_64-unknown-none` | Proses pertama sekaligus penggerak uji penerimaan K01–K03 |
| `xtask` | `xtask` | host | `cargo xtask build/run/test/soak/ci`: image FAT (MBR+ESP), QEMU + OVMF, verifikasi log dan exit code |

Semua yang berjalan di guest adalah kode Space OS; tidak ada Linux, libc, atau inferensi host di jalur uji (PRD §1 "definisi native").

## Mulai cepat

```bash
sudo apt install qemu-system-x86 ovmf     # Ubuntu 24.04; rustup memasang toolchain+target otomatis
cargo xtask test                           # build semua target, buat image, 4 skenario boot di QEMU
cargo xtask run                            # boot interaktif, serial di terminal (Ctrl-A X keluar)
cargo xtask soak --boots 100               # K01: 100 cold boot berturut-turut
```

Keluaran acceptance (dipotong):

```
spaceboot 0.1.0: Space OS UEFI bootloader
spacekernel 0.1.0: Space OS kernel booting
[kernel] selftest: heap ok, frames ok, paging ok, address-space ok
[kernel] spawn pid 1 'bin/init': entry=0x4058dc, 36 pages mapped, quota 2048 pages
[init] Space OS init running: pid 1, ABI v0, quota 2048 pages (36 used)
[kernel] pid 3 'bin/fault' killed: page fault at rip=0x40027d (error=0x7, addr=0xffff800000000000)
[init] PASS K02: write to kernel memory kills the process (page fault)
...
[init] frames free before=2089056 after=2089056 ; heap used before=1208 after=1208
[init] PASS K03: 50 spawn/exit cycles leak no frames and no kernel heap
[init] ALL TESTS PASSED (18/18)
[kernel] shutdown requested by pid 1 'bin/init' with code 0 (uptime 491 ms, 147 context switches)
```

## Dokumentasi

- [docs/architecture.md](docs/architecture.md) — rantai boot, layout memori, objek kernel, scheduler, diagnosis.
- [docs/adr/](docs/adr/README.md) — keputusan arsitektur (microkernel/Rust stable, bootloader UEFI, profil QEMU, ABI v0, ELF/initrd, PIC/PIT).
- [docs/requirements.md](docs/requirements.md) — traceability K01…H02 dengan status planned/experimental/verified.
- [docs/testing.md](docs/testing.md) dan [docs/evidence/](docs/evidence/) — skenario uji, marker, kode keluar, log bukti.
- [docs/build.md](docs/build.md) — prasyarat dan perintah.
- [docs/limitations.md](docs/limitations.md) — batas yang diketahui.
- [docs/roadmap.md](docs/roadmap.md) — tahap PRD vs kondisi repo, backlog berikutnya.

## Lisensi

MIT (lihat `LICENSE`). Dependensi: `x86_64`, `linked_list_allocator`, `noto-sans-mono-bitmap`, `fatfs`, `tar` (MIT/Apache-2.0), `uefi` (MPL-2.0).
