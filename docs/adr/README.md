# Architecture Decision Records

PRD §2 mensyaratkan keputusan arsitektur dicatat sebelum implementasi. Format: konteks → keputusan → konsekuensi → status.

| ADR | Judul | Status |
|---|---|---|
| [0001](0001-microkernel-rust-toolchain.md) | Microkernel berorientasi capability, Rust stable, target bare-metal | Diterima |
| [0002](0002-uefi-bootloader.md) | Bootloader UEFI sendiri (`spaceboot`) dan protokol boot | Diterima |
| [0003](0003-qemu-lab-profile.md) | Profil laboratorium QEMU yang dipatok | Diterima |
| [0004](0004-syscall-capability-abi-v0.md) | ABI syscall dan model capability versi 0 | Diterima |
| [0005](0005-executable-format-and-initrd.md) | Format executable (ELF64 statis) dan initrd (ustar) | Diterima |
| [0006](0006-interrupts-timer-single-cpu.md) | PIC/PIT dan satu CPU untuk MVP kernel | Diterima (sementara) |
| [0007](0007-storage-stack-and-file-abi.md) | Tempat driver, VirtIO block, FAT32 read-only, ABI file | Diterima |
| [0008](0008-compute-abi-v0.md) | Objek memori bersama dan Space Compute ABI v0 | Diterima |
