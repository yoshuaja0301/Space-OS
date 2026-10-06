# ADR-0030 — USB: pengendali xHCI dan keyboard boot protocol

- Status: diterima
- Tanggal: 2026-10-06
- Konteks: PRD tahap 7 (matriks hardware), PRD §6 (fungsi dasar dapat dipakai tanpa AI atau
  jaringan — terminal butuh keyboard), H01 (PC referensi), H02 (ARM64), ADR-0011 (masukan konsol),
  ADR-0020 (desktop dari keyboard), ADR-0028 (AArch64)

## Konteks

Satu-satunya keyboard yang dikenal kernel adalah PS/2 (8042, IRQ 1). Firmware UEFI meniru keyboard
PS/2 untuk keyboard USB hanya sampai `ExitBootServices`, dan banyak PC baru tidak punya 8042 sama
sekali. Jadi di PC sungguhan dengan keyboard USB — hampir semuanya — Space OS tidak bisa diketik
begitu kernel berjalan: terminal, desktop dan recovery (setelah bootloader) tidak berguna. Di
AArch64 keadaannya sama di VM: `virt` tidak punya PS/2, dan uji keyboard dilewati (ADR-0028).

Setiap PC sejak sekitar 2012 menaruh port USB-nya di pengendali xHCI, begitu juga server Arm dan
`qemu-xhci` di QEMU. Keyboard USB mana pun mendukung *boot protocol* (HID 1.11 lampiran B):
laporan delapan byte yang sama di semua merek, tanpa perlu membaca report descriptor.

## Keputusan

**1. Satu driver xHCI, polled.** `kernel/src/dev/xhci.rs` mengambil setiap pengendali kelas
0C/03/30 (paling banyak empat): meminta pengendali dari firmware lewat kapabilitas *USB Legacy
Support* (dan mematikan SMI-nya; firmware yang tidak melepas dalam satu detik dilangkahi), lalu
menghentikan dan me-reset-nya — apa pun yang ditinggalkan firmware berakhir di sini. Pengendali
diberi device context base array, satu command ring dan satu event ring (satu segmen), tanpa
interrupt: tick CPU boot melihat event ring setiap milidetik (`xhci::poll_tick`), seperti kartu
jaringan dilihat (ADR-0016). Setiap tunggu dibatasi waktu; pengendali atau perangkat yang tidak
menjawab dilaporkan dan ditinggalkan, tidak ditunggu selamanya, dan tidak pernah membuat panic.

**2. Enumerasi saat boot.** Perangkat yang tersambung ke port root saat kernel mulai di-reset
(port USB 2), diberi slot dan alamat (Enable Slot, Address Device), lalu ditanya descriptor
device dan configuration-nya lewat control transfer di endpoint 0. Ukuran paket endpoint 0
perangkat full-speed dikoreksi dengan Evaluate Context bila bukan 8. Interface pertama dengan
kelas HID, subkelas boot, protokol keyboard (alternate setting 0) dan endpoint interrupt IN-nya
dipakai: SET_CONFIGURATION, SET_PROTOCOL (boot), Configure Endpoint, lalu delapan transfer selalu
diantrekan di endpoint itu. Perangkat lain diberi alamat dan dibiarkan (dicatat dengan VID:PID).

**3. Laporan keyboard menjadi scan code PS/2.** `kernel/src/dev/hid.rs` membandingkan setiap
laporan dengan yang sebelumnya (tombol yang muncul ditekan, yang hilang dilepas; laporan
*ErrorRollOver* diabaikan) dan mengirim urutan scan code set 1 yang akan dikirim keyboard PS/2
untuk perubahan itu ke `input::decode` — satu decoder, satu aturan modifier, event dan byte yang
sama ke desktop dan terminal, keyboard mana pun yang dipakai. Tombol yang dilepas diproses lebih
dulu, lalu modifier, lalu tombol yang ditekan. Keyboard yang tercabut atau endpoint yang gagal
melepas semua tombolnya.

**4. 8042 yang tidak ada dikatakan.** Port status 8042 yang terbaca 0xFF berarti tidak ada
pengendali; kernel menulis `console input: no PS/2 controller` alih-alih mengaku punya keyboard
IRQ 1.

**5. Memori yang terjangkau.** Pengendali tanpa alamat 64 bit (HCCPARAMS1.AC64 = 0) hanya diberi
halaman di bawah 4 GiB; halaman yang tidak terjangkau dikembalikan. Pengendali dengan Port Power
Control menyalakan port-nya dulu setelah reset.

**6. Diuji seperti orang mengetik.** Skenario `terminal-usb` mem-boot mesin lab tanpa 8042
(`-machine q35,i8042=off`) dengan `qemu-xhci` dan `usb-kbd`, lalu mengetik sesi terminal yang sama
dengan skenario `terminal` lewat monitor QEMU — setiap tombol hanya bisa datang lewat USB.
`arm64-terminal-usb` melakukan hal yang sama di AArch64. Mesin compat `usb` menjalankan seluruh
suite penerimaan dengan keyboard dan tablet USB tersambung (tablet bukan keyboard boot: diberi
alamat dan dibiarkan).

## Bukti

| Klaim | Bukti |
|---|---|
| Sesi diketik pada keyboard USB saja (x86-64) | `terminal-usb`: `console input: no PS/2 controller; no COM2 UART`, `xhci: 00:05.0: xHCI 1.0, 8 ports, 32 slots, 32-byte contexts`, `usb: 00:05.0 port 5 (high speed): keyboard 0627:0001 ready -- boot protocol, interface 0, endpoint 0x81, 8-byte reports every 8000 us`; `helpp`, Backspace, `status`, `ls /spaceos`, `run hang`, `stop`, `quit` diketik lewat monitor QEMU dan setiap perintah dijawab (`docs/evidence/terminal-usb.log`) |
| Keyboard di AArch64 | `arm64-terminal-usb`: driver dan decoder yang sama di `virt`, sesi yang sama dijawab (`docs/evidence/arm64-terminal-usb.log`) |
| Perangkat lain dibiarkan, suite utuh | mesin compat `usb`: `device 0627:0001 is not a boot keyboard; left unconfigured` (tablet), `2 device(s) on its ports, 1 keyboard(s)`, `ALL TESTS PASSED (117/117, 0 skipped)` (`docs/evidence/compat-usb.log`) |
| Tidak ada yang lain berubah | `cargo xtask test` 16/16 skenario, `cargo xtask compat` 16/16 mesin, `cargo xtask arm64` 3/3 |

## Gigi

| Pelemahan | Akibat |
|---|---|
| Transfer laporan tidak diantrekan ulang setelah selesai | delapan laporan pertama (empat tombol) sampai, lalu keyboard diam: `terminal-usb` habis waktu setelah 240 s tanpa `[shell] commands: help, status, ls` |
| Backspace tidak dipetakan di decoder HID | `[shell] unknown command "helpp"; try 'help'` — koreksi yang diketik tidak pernah sampai |

## Konsekuensi

- Belum ada hub: perangkat di belakang hub (termasuk hub di dalam monitor atau dock, dan keyboard
  internal laptop yang tersambung lewat hub internal) tidak terlihat.
- Belum ada hot-plug: perangkat yang dicolok setelah boot dilaporkan sekali dan tidak dipakai;
  keyboard yang dicabut lalu dicolok lagi tidak kembali sampai boot berikutnya.
- Belum ada key repeat (keyboard USB tidak mengulang sendiri; host yang harus), LED Caps Lock, atau
  tombol Pause. Tata letak tetap US, sama dengan PS/2.
- Belum ada mouse, tablet, penyimpanan USB, atau kelas lain; belum ada pengendali EHCI/OHCI/UHCI
  (PC sebelum xHCI) dan belum ada perutean port USB 2 dari EHCI ke xHCI di chipset Intel seri 7.
- Event ring dilihat setiap tick 1 ms di CPU boot: latensi tombol paling lama satu tick ditambah
  interval endpoint keyboard (8 ms untuk `usb-kbd`).
