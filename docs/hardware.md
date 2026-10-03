# Hardware, boot, dan pemulihan

Space OS saat ini adalah OS eksperimental x86-64 UEFI. Dukungan keluarga CPU
tidak berarti seluruh motherboard, GPU, dan controller pada PC itu sudah
didukung. Belum ada sertifikasi perangkat fisik atau jaminan bebas crash.

| Perangkat | Dukungan saat ini | Yang masih dibutuhkan |
|---|---|---|
| AMD Ryzen / Intel x86-64 | Boot memeriksa NX dan SYSCALL; firmware 5-level paging ditolak | Uji model CPU/motherboard fisik; ACPI/APIC dan SMP |
| NVIDIA / AMD / Intel integrated | Konsol software pada framebuffer RGBX/BGRX yang disediakan UEFI GOP | Driver native, akselerasi, multi-monitor, power management |
| SSD NVMe | Firmware dapat mengenali media untuk memuat image UEFI | Driver kernel NVMe; belum bisa menjadi volume data Space OS |
| HDD / SSD SATA | Firmware dapat mengenali media untuk memuat image UEFI | Driver AHCI; belum bisa menjadi volume data Space OS |
| VirtIO block modern | Driver kernel dan FAT32 baca/tulis | Dukungan controller di luar matriks QEMU |
| Keyboard PS/2 / COM2 | Input kernel tersedia | Keyboard USB/xHCI sesudah ExitBootServices |
| Keyboard USB di menu firmware | Melalui UEFI Simple Text Input bila firmware menyediakannya | Ini tidak memberi driver USB kepada kernel |

## Menu startup

`cargo xtask build` menghasilkan `build/esp.img` yang membuka menu Space OS.
Untuk mencobanya dengan keyboard pada QEMU:

```bash
cargo xtask run --gui --boot-menu
```

Windows memakai launcher pada [panduan build](build.md):

```powershell
pwsh -File .\scripts\dev-wsl.ps1 run --gui --boot-menu
```

Menu memakai canvas grafis GOP dengan panel terang, fokus teal, dan font bitmap.
Jika GOP tidak tersedia, menu memakai konsol teks UEFI. Navigasi memakai Up/Down,
Enter, tombol angka 1–7, dan Esc.
Ini adalah menu startup milik Space OS, bukan pengganti BIOS/UEFI motherboard.

- **Start Space OS** membuka sesi terminal dengan driver penyimpanan normal.
- **Recovery terminal** membuka terminal dengan `storage=off`: enumerasi PCI
  untuk penyimpanan dan inisialisasi driver disk dilewati, sehingga volume tidak
  dipasang. Ini tidak memperbaiki kerusakan filesystem secara otomatis.
- **Hardware information** menampilkan vendor CPU, RAM tersedia, jumlah region,
  keberadaan GOP/ACPI, jumlah handle blok firmware (termasuk partisi), dan adanya
  Windows Boot Manager pada volume boot yang sama. Tidak memindai seluruh OS di
  semua disk, dan tidak memuat ulang Windows.
- **Restart** memakai layanan reset firmware; **Return to firmware** keluar dari
  loader. Firmware menentukan layar/menu berikutnya.

- **Network check** membaca adaptor Simple Network Protocol dan keberadaan
  protokol WiFi2 yang benar-benar disediakan firmware. Tombol `D` menguji DHCP
  pada antarmuka pertama dengan pertukaran OFFER/REQUEST/ACK yang dibatasi jumlah polling. Link atau
  lease DHCP tidak membuktikan akses internet atau jaringan di dalam kernel.
  `R` menyegarkan status; Esc kembali ke menu. Maksimum delapan referensi antarmuka
  diperiksa; hanya yang berhasil dibaca dan dipulihkan masuk hitungan.
  Kegagalan membaca referensi firmware dilaporkan terpisah.
- **Install boot files** menyalin bootloader, kernel, initrd, dan konfigurasi ke
  partisi EFI GPT lain yang sudah tersedia dan dapat ditulis. Pilih tujuan,
  Enter untuk meninjau, lalu `I` untuk mengonfirmasi. Partisi boot sumber tidak
  ditawarkan. Tujuan yang memiliki `EFI\SPACEOS` atau `EFI\BOOT\BOOTX64.EFI`
  ditolak; direktori OS lain dipertahankan. Berkas bootloader ditulis terakhir.
  Kegagalan dapat menyisakan berkas baru yang belum lengkap dan dilaporkan
  sebagai instalasi tidak selesai. Tidak ada pembuatan boot entry firmware.

## Batas Wi-Fi dan jaringan

Belum ada driver Wi-Fi kernel, pemindaian SSID, autentikasi WPA, TCP/IP kernel,
atau TLS. Intel BE201 yang terdeteksi pada host Windows belum didukung di
Space OS. Protokol jaringan firmware dapat tersedia atau tidak tersedia,
terlepas dari keberadaan perangkat Wi-Fi. Menu tidak menampilkan SSID palsu
atau menganggap semua adaptor jaringan sebagai Wi-Fi. Uji DHCP saat boot tidak
meneruskan koneksi ke kernel setelah ExitBootServices.

## Pemeriksaan dan kegagalan boot

Pemeriksaan fitur CPU dan RAM berjalan sebelum kernel dimuat. Kurang dari 128 MiB
RAM yang tersedia ditolak; ini batas penolakan awal, bukan spesifikasi minimum
untuk workload AI. Initrd harus tersedia dan tidak kosong. Framebuffer diperiksa
format, alignment, stride, dimensi, dan batas memorinya. Jika format piksel GOP
saat ini tidak didukung, loader mencoba mode RGBX/BGRX yang ditawarkan firmware.
Metadata framebuffer yang tetap tidak valid membuat boot memakai konsol serial.

Kegagalan loader menampilkan alasan serta pilihan restart/kembali ke firmware.
Framebuffer kernel diaktifkan sebelum inisialisasi memori agar diagnosis awal
dapat terlihat. Reset controller VirtIO memiliki batas polling, sehingga tidak
menunggu tanpa akhir. Hal ini mengurangi beberapa penyebab layar kosong atau
hang; tidak menjamin semua kegagalan hardware dapat dipulihkan.

## Instalasi dan data yang sudah ada

**Installer saat ini hanya menyalin berkas boot ke partisi EFI yang sudah ada.**
Belum ada pemartisi disk, instalasi volume data penuh, dual-boot otomatis,
Secure Boot yang ditandatangani, atau sertifikasi PC fisik. Image adalah media
pengembangan. Menu/preflight tidak memformat disk; penulisan installer hanya
berjalan setelah tujuan dan konfirmasi dipilih. Pengujian memakai disk QEMU
sintetis yang dibuat xtask, bukan disk fisik host.

Jangan menganggap boot dari USB berhasil berarti NVMe/SATA, keyboard USB, atau
GPU native sudah didukung. Target berikutnya adalah driver dan pengujian pada
model perangkat tertentu, baru kemudian installer dengan pemilihan disk eksplisit.

`cargo xtask setup-test` menguji DHCP firmware, adaptor tidak tersedia/link putus,
pembatalan instalasi, salinan pada EFI sintetis, penolakan penimpaan, dan boot
langsung dari partisi tujuan tanpa sumber. Hasil
aktual tersedia pada log setiap eksekusi; daftar skenario bukan bukti kelulusan.

## Verifikasi

`cargo xtask boot-test` menguji menu dengan ketikan QEMU, mode pemulihan, sesi
normal, serta penolakan CPU tanpa NX/SYSCALL dan RAM rendah. Termasuk tampilan
640×480, tanpa GOP, dan navigasi halaman laporan. Log dan tangkapan layar
disimpan di `build/logs/boot/`. `cargo xtask test` dan `cargo xtask compat` tetap
menjalankan pengujian kernel dan matriks mesin yang ada. Status aktual setiap
eksekusi harus diambil dari log, bukan dari daftar skenario ini.
