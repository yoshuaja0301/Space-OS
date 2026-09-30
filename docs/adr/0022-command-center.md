# ADR-0022 — Command Center: pencarian SpaceLink di desktop, dengan asal setiap hasil

- Status: diterima
- Tanggal: 2026-09-30
- Konteks: PRD §6 ("Command Center menampilkan pencarian SpaceLink, sumber hasil, status indeks, dan
  pilihan tindakan"), PRD §7 L01–L03, ADR-0013 (SpaceLink: indeks, revokasi, context bundle),
  ADR-0020 (desktop)

## Konteks

SpaceLink sudah bisa mengindeks, memberi peringkat, dan menyusun context bundle, dan setiap chunk
membawa path, rentang byte dan SHA-256 agar pemanggil bisa memeriksanya sendiri (ADR-0013). Tetapi
yang bisa memanggilnya hanya `init` dan uji. Orang di depan desktop tidak punya cara mencari, apalagi
melihat dari mana hasil itu berasal.

## Keputusan

**1. Aplikasi sendiri, SpaceLink sendiri, baca saja.** Command Center (`bin/deskapps` mode `command`,
Super+Space atau Alt+F5) diberi `SPAWN | FS | TRANSFER | DUP` oleh desktop, memunculkan
`bin/spacelink` miliknya, dan menyerahkan kepadanya hanya `FS | TRANSFER` — baca saja. Setelah itu
Command Center menutup kapabilitasnya sendiri: yang tersisa hanya channel ke layanan dan ke desktop.
Karena layanannya tidak bisa menulis, revokasi tidak ditawarkan di sini; yang sudah dicabut (daftar di
disk, ADR-0015) tetap dihormati karena layanan membacanya saat mulai.

**2. Yang ditampilkan** (PRD §6): kueri yang diketik; **status indeks** (dokumen, chunk, yang dicabut,
folder yang diindeks); **hasil** dengan **sumbernya** — path, rentang byte, skor, dan awal SHA-256
chunk — plus cuplikan teksnya; dan **tindakan**: Enter mencari, ↑/↓ memilih, **Ctrl+B** menyusun
context bundle untuk kueri (entri, byte dari anggaran 512, digest), **Ctrl+O** menampilkan hasil yang
dipilih di file manager.

**3. "Tampilkan di Files" adalah permintaan kepada desktop, bukan kapabilitas.** Pesan baru `OPEN`
(klien → server) membawa sebuah path; desktop membuka file manager **baru** di sana — folder dibuka,
berkas dipilih di foldernya. Aplikasi yang meminta tidak mendapat jawaban maupun handle apa pun, dan
file manager itu mendapat persis apa yang didapat file manager lain (`FS`). `HELLO` kini boleh membawa
path awal untuk file manager.

**4. Uji memeriksa asal hasil seperti pemanggil SpaceLink mana pun.** Uji desktop mengetik kueri,
membaca path, rentang dan digest yang **ditampilkan** Command Center dari deskripsinya, lalu membaca
byte itu sendiri dari disk dan menghitung SHA-256-nya: yang ditampilkan harus yang ada di disk.

## Bukti

| Klaim | Bukti |
|---|---|
| Cari, lihat asal, periksa asal, bundle, buka di Files | L01 "the Command Center searches the index, shows where each result came from, bundles it and opens it in Files": `channel` → `/spaceos/docs/IPC.TXT` teratas, digest yang ditampilkan = SHA-256 byte di disk, bundle dengan entri, file manager baru dengan `IPC.TXT` terpilih |
| Jalur keyboard sungguhan | skenario `desktop`: Super+Space, ketik `channel`, Enter, Ctrl+B, Ctrl+O; screenshot `desktop-6-command-center.png` |

## Gigi

Setiap pelemahan dijalankan sendiri terhadap skenario `acceptance`, lalu dikembalikan:

| Pelemahan | Akibat |
|---|---|
| Command Center menampilkan digest yang bukan milik chunk-nya (satu bit dibalik) | `the Command Center shows sha 0c37dfc6 for /spaceos/docs/IPC.TXT 0+186; the disk says 0d37dfc6` |
| Desktop mengabaikan `OPEN` | `Ctrl+O opened no file manager` |
| File manager mengabaikan path awalnya | `window 2 never said "selected IPC.TXT"; last: "files: /spaceos, 12 entries, selected MODEL.SLM (460096 bytes)"` |

## Konsekuensi

- Pencarian di desktop memakai kontrak yang sama dengan pemanggil lain: tidak ada jalur khusus yang
  bisa melewati revokasi atau menyembunyikan asal.
- Batas yang jujur: hanya `/spaceos/docs` yang diindeks, saat jendela dibuka — perubahan berkas
  sesudahnya tidak terlihat sampai jendela dibuka lagi (belum ada pembaruan per berkas); peringkat
  leksikal (ADR-0013); paling banyak lima hasil ditampilkan; cuplikan adalah awal chunk, bukan bagian
  yang cocok; tidak ada revokasi dari desktop.
