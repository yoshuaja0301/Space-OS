# Build, image, dan menjalankan

## Prasyarat

- Rust via `rustup` (toolchain `1.94.1` dan target `x86_64-unknown-none`, `x86_64-unknown-uefi` dipasang otomatis dari `rust-toolchain.toml`).
- QEMU ≥ 8.2 (`qemu-system-x86`) dan firmware OVMF. Ubuntu/Debian: `sudo apt install qemu-system-x86 ovmf`. Lokasi OVMF dicari otomatis; timpa dengan `SPACEOS_OVMF_CODE` / `SPACEOS_OVMF_VARS`.
- Tidak perlu `mtools`, `mkfs.fat`, atau `nasm`: image FAT dibuat murni oleh `xtask` (crate `fatfs`), stub interrupt dirakit oleh `rustc`.

## Perintah

| Perintah | Hasil |
|---|---|
| `cargo xtask build` | bootloader (`target/boot/…/spaceboot.efi`), kernel (`target/kernel/…/spacekernel`), program user (`target/user/…`), `build/esp.img` |
| `cargo xtask run [--gui] [--cmdline "selftest=panic"]` | boot image di QEMU, serial di stdio (`Ctrl-A X` untuk keluar) |
| `cargo xtask run --cmdline "init=bin/spaceterm"` | boot ke sesi interaktif: ketik `help`, `status`, `ls /spaceos`, `run ok`, `stop`, `quit` langsung di konsol |
| `cargo xtask test` | sembilan skenario boot + pemeriksaan log/exit code, log di `build/logs/` |
| `cargo xtask compat` | sembilan konfigurasi mesin QEMU dengan image yang sama (ADR-0010), log di `build/logs/compat/` |
| `cargo xtask soak --boots 100` | 100 cold boot berturut-turut skenario acceptance (K01) |
| `cargo xtask clippy` / `fmt` / `fmt-check` / `ci` | lint dan format semua target; `ci` = fmt-check + clippy + `test` + `compat` |

Proses user pertama dipilih `init=` pada cmdline kernel (`bin/init` bila tidak disebut).
Tambahkan `--debug` untuk profil dev. Build pertama ≈ 1 menit; build inkremental beberapa detik.

## Isi image (`build/esp*.img`)

MBR dengan satu partisi EFI System (FAT32, 63 MiB) berisi:

```
EFI/BOOT/BOOTX64.EFI          spaceboot
EFI/SPACEOS/spacekernel.elf   kernel (stripped)
EFI/SPACEOS/initrd.tar        bin/init dan program uji (bin/hello, bin/fault, bin/abi_negative,
                              bin/ipc_echo, bin/quota, bin/spin, bin/worker, bin/blocker,
                              bin/uiworker) serta layanan (bin/spacecompute, bin/spaceai,
                              bin/spaceshell, bin/spaceterm, bin/spacebroker, bin/spaceagent,
                              bin/spacelink, bin/spacepkg)
EFI/SPACEOS/spaceos.cfg       cmdline=...
```

Image dapat ditulis ke USB stick untuk mencoba di PC UEFI fisik (**belum diuji**; lihat `docs/limitations.md`).

## Menerjemahkan alamat panic

Kernel di `target/kernel/x86_64-unknown-none/release/spacekernel` menyimpan debug info:

```
llvm-addr2line -e target/kernel/x86_64-unknown-none/release/spacekernel -f -i 0xffffffff8010b159
```
