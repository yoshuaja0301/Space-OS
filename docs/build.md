# Build, image, dan menjalankan

## Prasyarat

- Rust via `rustup` (toolchain `1.94.1` dan target `x86_64-unknown-none`, `x86_64-unknown-uefi` dipasang otomatis dari `rust-toolchain.toml`).
- QEMU ≥ 8.2 (`qemu-system-x86`) dan firmware OVMF. Ubuntu/Debian: `sudo apt install qemu-system-x86 ovmf`. Lokasi OVMF dicari otomatis; timpa dengan `SPACEOS_OVMF_CODE` / `SPACEOS_OVMF_VARS`.
- Tidak perlu `mtools`, `mkfs.fat`, atau `nasm`: image FAT dibuat murni oleh `xtask` (crate `fatfs`), stub interrupt dirakit oleh `rustc`.

## Perintah

| Perintah | Hasil |
|---|---|
| `cargo xtask build` | bootloader (`target/boot/…/spaceboot.efi`), kernel (`target/kernel/…/spacekernel`), program user (`target/user/…`), `build/esp.img` |
| `cargo xtask run [--gui] [--serial-input] [--cmdline "selftest=panic"]` | boot image di QEMU, log serial di stdio (`Ctrl-A X` untuk keluar). Tanpa `--gui` atau `--serial-input` tamu **tidak punya jalur masukan** sama sekali |
| `cargo xtask run --gui --cmdline "init=bin/spaceterm"` | boot ke sesi interaktif lewat keyboard jendela QEMU: ketik `help`, `status`, `ls /spaceos`, `run ok`, `stop`, `quit` |
| `cargo xtask run --serial-input --cmdline "init=bin/spaceterm"` | sesi yang sama tanpa jendela: QEMU mencetak path pty COM2, ketik ke sana (`screen <pty>`) |
| `cargo xtask run --gui --cmdline "init=bin/spacedesk"` | boot ke **desktop** (ADR-0020): Super+Enter (atau Alt+F1) terminal, Super+E file manager, Super+A Agent Center (tombol `5` menjalankan model, `S` Stop), Super+Space Command Center, Alt+Tab pindah jendela, Ctrl+Alt+Delete mematikan mesin. Di jendela QEMU, Super sering ditangkap sistem host; pakai Alt+F1/F2/F3/F5 |
| `cargo xtask test [--only <nama>]` | sebelas skenario boot + pemeriksaan log/exit code dan rekaman jaringan, log di `build/logs/` |
| `cargo xtask compat` | sepuluh konfigurasi mesin QEMU dengan image yang sama (ADR-0010), log di `build/logs/compat/` |
| `cargo xtask stress --minutes 480` | uji stabilitas (ADR-0019): satu boot, seluruh suite berulang selama 8 jam dengan pembunuhan acak dan pemeriksaan memori; hasil di `build/stress/` |
| `cargo xtask unit` | uji unit host (`spaceabi`, `xtask`) |
| `cargo xtask soak --boots 100` | 100 cold boot berturut-turut skenario acceptance (K01) |
| `cargo xtask clippy` / `fmt` / `fmt-check` / `ci` | lint dan format semua target; `ci` = fmt-check + clippy + `unit` + `test` + `compat` |

Proses user pertama dipilih `init=` pada cmdline kernel (`bin/init` bila tidak disebut).
Tambahkan `--debug` untuk profil dev. Build pertama ≈ 1 menit; build inkremental beberapa detik.

## Isi image (`build/esp*.img`)

MBR dengan satu partisi EFI System (FAT32, 63 MiB) berisi:

```
EFI/BOOT/BOOTX64.EFI          spaceboot
EFI/SPACEOS/spacekernel.elf   kernel (stripped)
EFI/SPACEOS/initrd.tar        bin/init dan program uji (bin/hello, bin/fault, bin/abi_negative,
                              bin/ipc_echo, bin/quota, bin/spin, bin/worker, bin/blocker,
                              bin/uiworker, bin/tlsprobe, bin/churn) serta layanan
                              (bin/spacecompute, bin/spaceai, bin/spaceshell, bin/spaceterm,
                              bin/spacebroker, bin/spaceagent, bin/spacelink, bin/spacepkg,
                              bin/spacenet, bin/spacecloud, bin/spacedesk, bin/deskapps)
EFI/SPACEOS/spaceos.cfg       cmdline=...
```

Image dapat ditulis ke USB stick untuk mencoba di PC UEFI fisik (**belum diuji**; lihat `docs/limitations.md`).

## Menerjemahkan alamat panic

Kernel di `target/kernel/x86_64-unknown-none/release/spacekernel` menyimpan debug info:

```
llvm-addr2line -e target/kernel/x86_64-unknown-none/release/spacekernel -f -i 0xffffffff8010b159
```
