# ADR-0027 — Recovery dan disk terpasang

- Status: diterima
- Tanggal: 2026-10-05
- Konteks: PRD "rantai boot dan pemulihan" (jalur recovery harus tetap ada ketika model, indeks
  atau desktop gagal; fungsi dasar OS tidak bergantung pada keluaran LLM), PRD §6 ("file manager,
  terminal, pengaturan model, serta recovery dapat digunakan tanpa AI atau jaringan"), tahap 7
  (installer, recovery), ADR-0025 (volume data dari labelnya)

## Konteks

Sistem hanya punya satu jalan boot: `spaceos.cfg` menyebut satu command line, dan `init` yang
disebutnya berjalan. Bila program itu gagal di setiap boot — desktop yang crash saat mulai,
store paket yang rusak, model yang tidak bisa dimuat — tidak ada jalan lain selain menyunting
ESP dari mesin lain. Dan sistem tidak bisa dipasang: image uji terdiri dari dua disk (ESP dan
disk data), bukan bentuk yang bisa ditulis ke satu disk atau stik USB.

## Keputusan

**1. Pilihan dibuat oleh bootloader, sebelum kernel.** Recovery harus bisa dicapai justru ketika
yang lain rusak, jadi keputusannya tidak boleh bergantung pada apa pun yang dijalankan kernel.
`spaceos.cfg` kini punya `recovery=` (command line recovery, bawaan `init=bin/spacerecovery`),
`recovery_wait_ms=` dan `boot_count=on`.

**2. Dua jalan ke recovery.**
- *Operator memintanya:* selama `recovery_wait_ms` bootloader berkata `press R … for recovery`
  dan menunggu tombol atau timer, mana yang lebih dulu.
- *Mesin terus gagal:* dengan `boot_count=on`, setiap boot menambah satu pada
  `\SPACEOS\VAR\BOOTS.TXT` di volume data, lewat driver FAT milik firmware, dan sistem
  mengembalikannya ke 0 setelah berhasil naik. Tiga boot berturut-turut yang tidak pernah naik
  → recovery, dengan alasannya dicetak.
Alasannya diteruskan di command line (`recovery=operator` atau `recovery=failed-boots`).
Tidak ada jalan yang bisa menggagalkan boot: volume data yang tidak ditemukan atau konsol tanpa
masukan hanya berarti jalan itu tidak ditawarkan kali ini, dan log mengatakannya.

**3. `spacerecovery`: konsol yang tidak butuh apa pun yang bisa rusak sendiri** — tanpa model,
indeks, desktop, jaringan, atau AI; hanya konsol dan volume data. Perintah: `status`, `check`
(model terhadap manifest, store paket, daftar revokasi, hitungan boot — masing-masing dengan
putusan), `repair packages|revocations` (mengosongkan berkas yang tidak terbaca kembali agar
layanannya mulai bersih), `boot normal`, `files`, `show`, `shell` (sesi `spaceshell` penuh, lalu
kembali), `poweroff`.

**4. Disk terpasang.** `cargo xtask disk-image` membuat satu disk GPT: partisi 1 ESP (bootloader,
kernel, initrd, `spaceos.cfg` dengan desktop, recovery dan hitungan boot), partisi 2 volume
data. Ditulis ke disk atau stik USB dengan `dd`. Kernel menemukan volume datanya dari label
(ADR-0025), jadi ESP di partisi 1 disk yang sama tidak terjangkau.

**5. Mematikan mesin: ACPI S5.** Perangkat debug-exit QEMU tidak ada di PC, jadi `poweroff`
(dan setiap `shutdown`) di sana dulu hanya menghentikan CPU. Kini kernel membaca FADT saat boot —
register PM1a/PM1b control, dan perintah SMI untuk menyerahkan ACPI bila firmware belum — dan
mencari satu paket `\_S5` di DSDT untuk nilai SLP_TYP. Tidak ada interpreter AML: hanya
pencarian paket itu, dengan integer AML sederhana (Zero, One, Byte, Word). Mematikan = SLP_TYP |
SLP_EN ke PM1a (dan PM1b). Tabel yang tidak menyebutnya dilaporkan di log boot (`power: no ACPI
S5 (…)`), dan shutdown di mesin itu tetap berhenti di CPU — dengan pesan, bukan diam.

## Bukti

| Klaim | Bukti |
|---|---|
| Operator bisa meminta recovery di menu boot | skenario `recovery-key` (R lewat keyboard PS/2 firmware): `spaceboot: press R within 5 s for recovery`, `spaceboot: recovery (operator)`, `[recovery] started because the operator asked for it at the boot menu`; `check` lewat keyboard: `check model: ok: /spaceos/model.slm: 460096 bytes, sha256 matches the manifest`, `check done: 0 problem(s)`; `init` tidak pernah berjalan |
| Tiga boot yang tidak naik → recovery dengan sendirinya | skenario `recovery-auto` (disk data dengan `tries=3` dan store paket rusak): `spaceboot: 3 boots in a row did not come up; starting recovery`, `boots since the system last came up: 4`, `check packages: PROBLEM: /spaceos/var/pkgstore.dat: 20 bytes that are not a package store`, `repair packages` → `emptied`, `boot normal` → `boot count cleared`, `check` sesudahnya `0 problem(s)` |
| Sistem yang naik tidak pernah dikirim ke recovery | skenario `boot-count` (dua boot dari `tries=2`): boot 1 `spaceboot: 2 boot(s) since the system last came up`, lalu `[term] the system is up; the boot count is back to 0`; boot 2 `spaceboot: 0 boot(s) …`; tidak ada `spaceboot: recovery` |
| Satu disk terpasang boot sendiri | compat `installed-disk`: hanya disk buatan `disk-image` (tanpa disk lain): `press R within 1 s for recovery`, `0 boot(s) since the system last came up`, `AHCI 00:1f.2 port 0 (130 MiB) holds the data volume (GPT partition 2)`, `ALL TESTS PASSED (117/117, 0 skipped)` |
| Shutdown mematikan mesin tanpa debug-exit | skenario `acpi-poweroff` (`shutdown=acpi`): `[kernel] power: ACPI S5 through PM1a_CNT 0x604 (SLP_TYP 0)`, `[kernel] switching off through ACPI S5`, QEMU keluar dengan 0 (dimatikan), bukan 33 |

## Gigi

Setiap pelemahan dijalankan sendiri terhadap skenarionya, lalu dikembalikan:

| Pelemahan | Akibat |
|---|---|
| Bootloader tidak pernah memulai recovery karena hitungan boot | `recovery-auto`: `the guest exited without printing "[recovery] ready"`, `missing marker "spaceboot: 3 boots in a row did not come up; starting recovery"` |
| Sesi terminal tidak mengembalikan hitungan ke 0 setelah naik | `boot-count`: `missing marker "[term] the system is up; the boot count is back to 0"` di boot pertama |
| `repair packages` membuka berkas alih-alih mengosongkannya | `recovery-auto`: `missing marker "[recovery] check packages: ok: the package store is empty"`, `missing marker "[recovery] check done: 0 problem(s)"` |
| SLP_TYP ditulis tanpa SLP_EN | `acpi-poweroff`: `QEMU exit code None, expected 0`, `unexpected marker "could not be switched off"` |

Temuan saat menjalankannya: harness membuka pty COM2 hanya selama mengetik. QEMU mencari pembaca
di pty kira-kira sekali per detik, jadi satu baris pendek (`quit` saja) bisa datang dan pergi di
antara dua pemeriksaan dan tidak pernah sampai ke guest — `boot-count` macet di 3 dari 4 boot.
Pty kini dibuka saat guest boot dan ditahan sampai guest selesai.

## Konsekuensi

- Firmware menulis ke volume data sebelum kernel berjalan: satu berkas kecil, lewat driver FAT
  milik firmware. Firmware yang tidak mengenali disk data (tanpa driver untuk pengendalinya)
  berarti boot tidak dihitung — dan log mengatakannya — bukan boot yang gagal.
- Recovery tidak bisa memperbaiki bootloader atau kernel yang rusak: keduanya ada di ESP, yang
  sengaja tidak terjangkau dari dalam sistem (ADR-0025). ESP yang rusak berarti menulis ulang
  image disk dari mesin lain.
- Belum ada installer yang berjalan di dalam Space OS dan menulis ke disk lain: itu butuh akses
  tulis ke disk di luar volume data, yang justru ditolak lapisan `block`. Memasang = menulis
  image disk dengan `dd` (atau alat sejenis) dari sistem lain.
- Mematikan daya bergantung pada `\_S5` yang ditulis langsung sebagai paket di DSDT (begitu
  hampir semua firmware menulisnya); `\_S5` yang dihitung oleh metode AML tidak terbaca, dan
  mesin itu berhenti di CPU. `_PTS` tidak dipanggil. Belum ada `reboot`.
- Uji recovery berjalan di QEMU/OVMF: tombol R lewat keyboard PS/2 firmware; papan fisik dengan
  keyboard USB bergantung pada driver USB firmware-nya.
