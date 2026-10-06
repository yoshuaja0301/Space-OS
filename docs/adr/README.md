# Architecture Decision Records

PRD §2 mensyaratkan keputusan arsitektur dicatat sebelum implementasi. Format: konteks → keputusan → konsekuensi → status.

| ADR | Judul | Status |
|---|---|---|
| [0001](0001-microkernel-rust-toolchain.md) | Microkernel berorientasi capability, Rust stable, target bare-metal | Diterima |
| [0002](0002-uefi-bootloader.md) | Bootloader UEFI sendiri (`spaceboot`) dan protokol boot | Diterima |
| [0003](0003-qemu-lab-profile.md) | Profil laboratorium QEMU yang dipatok | Diterima |
| [0004](0004-syscall-capability-abi-v0.md) | ABI syscall dan model capability versi 0 | Diterima |
| [0005](0005-executable-format-and-initrd.md) | Format executable (ELF64 statis) dan initrd (ustar) | Diterima |
| [0006](0006-interrupts-timer-single-cpu.md) | PIC/PIT dan satu CPU untuk MVP kernel | Diterima; bagian satu CPU digantikan 0024 |
| [0007](0007-storage-stack-and-file-abi.md) | Tempat driver, VirtIO block, FAT32 read-only, ABI file | Diterima |
| [0008](0008-compute-abi-v0.md) | Objek memori bersama dan Space Compute ABI v0 | Diterima |
| [0009](0009-spacelm-model-and-inference.md) | Format model SpaceLM v0, runtime inferensi, baseline dipatok | Diterima |
| [0010](0010-compatibility-matrix.md) | Matriks kompatibilitas mesin dan degradasi anggun | Diterima |
| [0011](0011-session-service-and-supervision.md) | Layanan sesi `spaceshell` dan aturan supervisi | Diterima |
| [0012](0012-tool-broker-and-agent-scope.md) | Tool Broker, scope workspace, dan audit agent | Diterima |
| [0013](0013-spacelink-index-revocation-bundle.md) | SpaceLink: indeks, revokasi, dan context bundle | Diterima |
| [0014](0014-package-format-and-rollback.md) | Format paket, autentikasi HMAC, dan rollback | Diterima |
| [0015](0015-writable-storage.md) | Penyimpanan yang bisa ditulis: hak terpisah, FAT32 tulis, durabilitas | Diterima; alasan "ESP bukan virtio" digantikan 0025 |
| [0016](0016-network-stack.md) | Jaringan: kernel memindahkan frame, TCP/IP di user space, tujuan sebagai kapabilitas | Diterima |
| [0017](0017-tls.md) | TLS: pustaka yang di-port, kunci dari sumber entropi kernel, kepercayaan hanya dari yang diberikan | Diterima; keputusan 2 (unit FPU dimatikan) digantikan 0031 |
| [0018](0018-cloud-adapter.md) | Adapter cloud: satu program memegang kredensial, jaringan ke satu alamat, dan tool hanya lewat broker | Diterima |
| [0019](0019-stability-run.md) | Uji stabilitas: seluruh suite berulang dalam satu boot, pembunuhan acak, dan memori yang harus kembali tepat | Diterima |
| [0020](0020-desktop.md) | Desktop: layar sebagai lease, jendela sebagai memori klien, dan manajemen jendela dari keyboard | Diterima |
| [0021](0021-inference-job-and-cooperative-stop.md) | Worker inferensi di dalam sesi: laporan per token dan Stop kooperatif | Diterima |
| [0022](0022-command-center.md) | Command Center: pencarian SpaceLink di desktop, dengan asal setiap hasil | Diterima |
| [0023](0023-spacelink-update.md) | SpaceLink: satu berkas yang berubah diindeks ulang sendirian, tanpa full rescan | Diterima |
| [0024](0024-smp.md) | SMP: setiap CPU menjalankan thread — satu run queue, `on_cpu`, tanpa TLB shootdown | Diterima |
| [0025](0025-block-devices.md) | Disk SATA (AHCI) dan NVMe, dan volume data yang dipilih dari labelnya | Diterima |
| [0026](0026-e1000.md) | Kartu jaringan Intel e1000 di balik lease yang sama | Diterima |
| [0027](0027-recovery-install.md) | Recovery yang dipilih bootloader, hitungan boot, dan satu disk untuk dipasang | Diterima |
| [0028](0028-aarch64.md) | AArch64: kernel yang sama di mesin ARM | Diterima |
| [0029](0029-clock.md) | Waktu dari counter (timer ACPI PM, generic timer), bukan dari hitungan tick | Diterima |
| [0030](0030-usb-keyboard.md) | USB: pengendali xHCI dan keyboard boot protocol, di x86-64 dan AArch64 | Diterima |
| [0031](0031-fpu-state.md) | Register FP/SIMD milik thread yang menulisnya: disimpan dan dimuat pada setiap switch (PRD v0.2 K03) | Diterima |
