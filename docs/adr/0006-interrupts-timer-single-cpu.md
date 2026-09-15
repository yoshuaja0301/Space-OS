# ADR-0006: PIC/PIT dan satu CPU untuk MVP kernel

Status: Diterima (sementara) — 2026-09-15

## Konteks

Profil lab memakai 4 vCPU, tetapi K01–K03 tidak mensyaratkan SMP. LAPIC/IOAPIC/x2APIC memerlukan parsing ACPI MADT dan kalibrasi timer; keduanya menambah risiko pada tahap "buktikan boot dan isolasi".

## Keputusan

- Interrupt controller: 8259 PIC dipetakan ke vektor 32–47; hanya IRQ0 (timer) dan IRQ1 (keyboard, hanya dikuras) yang dibuka.
- Timer: PIT channel 0 pada 1000 Hz (kuantum scheduler 10 ms); `SYS_TICKS`/`SYS_SLEEP` beresolusi 1 ms.
- Satu CPU: AP dibiarkan parkir oleh firmware. Penyederhanaan yang bergantung pada asumsi ini dan harus diganti saat SMP: stack kernel untuk `syscall` disimpan di global (`SYSCALL_KERNEL_RSP`, bukan `swapgs`/per-CPU), spinlock = matikan interrupt + deteksi re-entrancy, `schedule()` tanpa lock lintas CPU.
- RSDP sudah diteruskan di `BootInfo` agar migrasi ke ACPI/APIC tidak mengubah kontrak boot.

## Konsekuensi

- Throughput terbatas satu core; cukup untuk K01–K03 dan inferensi CPU awal (A01 mengukur correctness, bukan kecepatan).
- Tiket lanjutan: MADT → LAPIC timer + IOAPIC, per-CPU data, SMP boot AP, lalu ubah ADR ini.
