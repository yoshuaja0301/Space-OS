# Arsitektur yang diimplementasikan (tahap 1–2)

Peta ke lapisan PRD §2: repo ini mengisi baris **Kernel (Space Kernel dan HAL)** dan fondasi untuk **Layanan OS**; lapisan lain belum ada.

```
UEFI (OVMF) ──► spaceboot (boot/)  ──► spacekernel (kernel/) ──► bin/init (user/init) ──► program uji (user/tests/*)
                 BootInfo (spaceabi::boot)     ABI v0 (spaceabi::syscall)   libspace (user/libspace)
```

## Rantai boot

1. OVMF memuat `\EFI\BOOT\BOOTX64.EFI` (= `spaceboot`) dari ESP.
2. `spaceboot` membaca `spacekernel.elf`, `initrd.tar`, `spaceos.cfg`; memuat segmen kernel; menyalin initrd/cmdline ke memori bertipe KERNEL; membaca GOP dan RSDP; membangun page table (kernel higher-half, linear map RAM, identity sementara); keluar dari boot services; menormalkan memory map; mengaktifkan NXE/WP; melompat ke `_start` dengan `rdi = &BootInfo`.
3. `spacekernel::kmain`: serial → GDT/TSS (IST untuk double fault) → IDT (256 stub asm) → memori (bitmap frame, PML4 kernel baru, heap 16 MiB, slot kernel stack berguard) → framebuffer → cmdline → initrd → PIC/PIT → MSR syscall → scheduler → selftest kernel → spawn `bin/init` → idle loop.
4. `init` (user, ring 3) memegang handle Root dan menjalankan/menguji program lain.

## Layout memori virtual

| Rentang | Isi |
|---|---|
| `0x0000_0000_0040_0000` | kode/data program user (ELF) |
| `0x0000_0010_0000_0000…` | region `mem_map` (bump, dengan celah guard) |
| `0x0000_7FFF_EFFF_0000 – 0x7FFF_F000_0000` | stack user 64 KiB (NX) |
| `0xFFFF_8000_0000_0000` | linear map memori fisik (`PHYS_OFFSET`), NX |
| `0xFFFF_9000_0000_0000` | heap kernel |
| `0xFFFF_A000_0000_0000` | slot kernel stack 64 KiB (32 KiB terpeta + guard) |
| `0xFFFF_FFFF_8000_0000` | image kernel |

Setiap proses memiliki PML4 sendiri: half bawah privat, slot 256–511 disalin dari PML4 kernel (sub-tabel heap dan kernel stack dipra-alokasi agar pemetaan baru terlihat semua proses).

## Objek kernel

- **Process**: address space + tabel handle + kuota + status keluar + antrean penunggu `wait`.
- **Thread**: satu per proses (MVP); kernel stack sendiri; context switch menyimpan register callee-saved (`switch_to`); masuk ring 3 lewat `iretq`; syscall lewat `syscall/sysret`.
- **Channel/Endpoint**: dua sisi, antrean pesan terbatas, wait queue penerima, penutupan sisi membangunkan peer.
- **Root**: capability istimewa `init` (spawn dari initrd, statistik, shutdown, fault injection).

## Scheduler

Round-robin preemptif, tick 1 ms, kuantum 10 ms, satu CPU. Thread yang mati direklamasi oleh thread yang berjalan berikutnya (`finish_switch`), sehingga stack kernel tidak dibebaskan oleh pemiliknya sendiri. Semua blocking (`recv`, `wait`, `sleep`) mendaftar ke wait queue/sleepers dengan interrupt mati sebelum `schedule()` agar wake-up tidak hilang.

## Terminasi dan diagnosis

- Exception ring 3 → `[kernel] pid N '…' killed: <exception> at rip=… (error=…, addr=…)` → `exit_current` → orang tua menerima `ExitStatus`.
- Exception ring 0 / panic → `!!! CPU EXCEPTION IN KERNEL MODE …` dump register + `!!! KERNEL PANIC !!!` + backtrace frame pointer + `isa-debug-exit(0x3f)`.
- Overflow stack kernel → guard page → double fault pada stack IST → dump + panic (diuji oleh `selftest=stack`).
