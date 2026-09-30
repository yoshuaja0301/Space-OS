# ADR-0018 — Adapter cloud: satu program memegang kredensial, jaringan ke satu alamat, dan tool hanya lewat broker

- Status: diterima
- Tanggal: 2026-09-29
- Konteks: PRD §7 I01 ("Satu adapter cloud lulus auth, streaming, tool use, timeout, cost cap dan
  local-only negative test"); PRD §4, "Integrasi yang perlu dibuktikan satu per satu" (adapter layanan terpisah; DNS, TCP/IP, TLS, trust store,
  waktu sistem dan **penyimpanan kredensial**; router tidak boleh mengalihkan tugas local-only ke
  cloud; retry dengan batas biaya dan jumlah percobaan; status integrasi planned/experimental/
  verified beserta versi yang diuji); ADR-0012 (Tool Broker), ADR-0016 (jaringan), ADR-0017 (TLS)

## Konteks

Semua prasyarat I01 sudah ada: DNS, TCP/IP, TLS 1.3, entropi, jam. Yang belum diputuskan adalah
**siapa memegang apa**. Model cloud berjalan di server penyedia; di mesin ini hanya ada adapter,
dan adapter itulah yang memegang hal-hal paling berbahaya dalam gambar ini: kunci API, jalan ke
internet, dan kemampuan menjalankan alat atas permintaan model yang isinya tidak dikendalikan
siapa pun di mesin ini. Tidak ada penyedia sungguhan yang bisa dicapai dari lingkungan ini
(jaringan lab tertutup, tanpa kunci), jadi adapter diuji terhadap penyedia tiruan — dan harus
berkata begitu.

## Keputusan

**1. Satu program, otoritas minimum: `spacecloud`.** Ia memegang empat hal dan tidak memberikan
satu pun:

- **trust anchor** penyedia dan **kredensial API** — keduanya diserahkan operator lewat pesan.
  `spacecloud` sendiri **tidak memegang kapabilitas file sama sekali**; kunci di disk
  (`/spaceos/cred/cloud.key`) ada di luar workspace;
- **sesi `spacenet`** yang allowlist-nya hanya `api.cloud.test:443`;
- **channel ke Tool Broker** untuk panggilan alat model.

Klien (agent, sesi, atau `init` di uji) hanya bisa *bertanya*: `Ask` masuk lewat channel klien,
`Event` keluar — `DELTA` (teks), `TOOL` (panggilan alat dan putusan broker), lalu `DONE` atau
`ERROR` dengan token, biaya dan jumlah percobaan. Kontraknya di `spaceabi::cloud`.

**2. Sebelum satu byte keluar, urutan pemeriksaannya tetap:** (i) **local-only → ditolak**, tanpa
koneksi dan tanpa satu frame; (ii) siap? (trust anchor, kredensial, endpoint, sesi); (iii)
**reservasi**: seluruh request sebagai input (bytes/3 + 16 token — sengaja berlebih) dan
`max_tokens` sebagai output, dengan harga yang dikonfigurasi (µ$ per juta token, integer, dibulatkan
ke atas), harus muat di sisa budget. Reservasi diulang sebelum setiap putaran alat.

**3. Streaming dengan penjaga.** Jawaban adalah server-sent events di atas HTTP/1.1 chunked di atas
TLS 1.3, dan setiap `text_delta` diteruskan ke klien saat tiba. Usage yang dilaporkan penyedia
(`message_start`, `message_delta`) dibandingkan terus dengan reservasi: output melewati
`max_tokens`, atau biaya berjalan melewati sisa budget → koneksi **diputus di tempat**
(`over-budget`), dan yang dilaporkan terpakai tetap ditagihkan — adapter tidak pernah
menghitung kurang dari yang dibelanjakannya.

Memutus harus benar-benar **menghentikan** penyedia. Run pertama membuktikan tidak: adapter
berhenti membaca, `spacenet` menutup dengan FIN, dan penyedia tiruan terus mengalirkan ke-30
putarannya (`streamed all 30 rounds; nobody stopped it`) — di dunia nyata, token yang terus
ditagih. Perbaikannya ada di `spacenet`: data yang tiba setelah klien menutup koneksinya dijawab
**RST** (RFC 1122 §4.2.2.13), baik saat koneksi masih menunggu maupun di graveyard. Kini penyedia
berhenti setelah 2 putaran. Uji NET baru memeriksa aturan yang sama tanpa TLS: layanan
character generator (RFC 864) di `chargen.lab.test:19` harus melihat guest pergi.

**4. Retry terbatas, dan hanya bila aman.** 429/5xx/529, atau `overloaded_error` sebelum ada teks
diteruskan, dicoba lagi hingga `attempts` kali (ditetapkan operator, 1–5) dengan jeda
250 ms × 2ⁿ, di dalam tenggat ask. Setelah teks sampai ke klien tidak ada retry — klien akan
melihat jawaban dua kali. 401/403 tidak pernah dicoba ulang. Header `x-spacecloud-attempt`
memberi tahu penyedia percobaan ke berapa ini.

**5. Alat model hanya lewat Tool Broker.** Satu-satunya alat yang ditawarkan adalah `read_file`,
dan hanya bila ask memintanya. Setiap panggilan menjadi `READ` ke broker (ADR-0012), yang
memeriksa scope workspace dan mencatatnya di audit; hasilnya — isi berkas atau penolakan —
kembali ke model sebagai `tool_result`, dan klien diberi event `TOOL` dengan putusan broker.
Paling banyak 4 request per ask.

**6. Satu tenggat untuk seluruh ask** (`timeout_ms`, termasuk putaran alat dan retry); setiap baca
dan tulis memakai sisanya.

**7. Penyedia diperlakukan sebagai masukan yang tidak dipercaya.** Program ini punya stack 64 KiB,
dan penyedia menentukan apa yang dikirimnya. JSON bersarang lebih dari 32 tingkat ditolak sebelum
parser menyelaminya (batas parser sendiri 128 — cukup untuk menghabiskan stack), satu jawaban
paling banyak 64 blok konten dan 8 panggilan alat yang dijalankan per giliran, teks jawaban tidak
boleh melebihi 32 byte per token yang dipesan (pengaman bila penyedia berbohong tentang usage), dan
setiap baris, event, kepala respons dan body error dibatasi ukurannya.

**8. Format kawat: Messages API 2023-06-01, bagian yang dipakai saja** — `POST /v1/messages`,
`x-api-key`, `anthropic-version`, body JSON (`serde_json` tanpa std), dan event `message_start`,
`content_block_start/delta/stop` (`text_delta`, `input_json_delta`), `message_delta`,
`message_stop`, `error`, `ping`. HTTP/1.1 minimal milik sendiri: satu request per koneksi, body
`content-length`, chunked, atau sampai close_notify. Kredensial hanya diterima sebagai ASCII
tampak (tanpa CR/LF — tidak bisa menyelundupkan header), host harus nama DNS yang sah.

**9. Penyedia tiruan, dan disebut tiruan di mana-mana.** `xtask lab cloud` di 10.0.2.100:443
(`api.cloud.test`, sertifikat dari otoritas lab ADR-0017) memakai rustls + ring dan `serde_json` di
host. Nama model memilih skrip, masing-masing untuk satu hal yang harus benar:

| Model | Skrip |
|---|---|
| `lab-echo` | jawaban dalam 4 potong, berjarak 150 ms |
| `lab-tool` | minta membaca berkas workspace, lalu mengutip isinya |
| `lab-exfil` | minta membaca berkas kredensial, lalu mengatakan apa yang ia terima |
| `lab-stall` | mulai menjawab dan tidak pernah selesai |
| `lab-overloaded` | 529, setiap kali |
| `lab-runaway` | terus mengalir melewati `max_tokens`, melaporkan token yang terus naik |
| `lab-hostile` | satu event bersarang 120 tingkat — JSON sah, cukup dalam untuk menghabiskan stack parser |

Kunci API dibuat baru setiap data disk. Adapter melaporkan statusnya sendiri saat `HELLO`:
`experimental, tested against the lab mock provider only`.

## Alternatif yang ditolak

- **Adapter membaca kunci dari disk sendiri.** Butuh hak `FS`, yang berarti seluruh volume —
  otoritas jauh lebih besar dari satu kunci.
- **Agent memegang sesi jaringan dan kunci.** Model yang disuntik prompt lewat konteks bisa
  menyuruh agent mengirim apa saja ke mana saja yang diizinkan sesinya.
- **Retry setelah teks diteruskan.** Klien melihat duplikat; untuk alat dengan efek samping,
  mengulang bisa berbahaya.
- **Memutus hanya di sisi klien (FIN).** Terbukti tidak menghentikan penyedia.
- **Parser JSON sendiri.** `serde_json` luas dipakai dan jalan tanpa std; gerbang build ADR-0017
  membuktikan ia tidak membawa instruksi vektor.

## Bukti

Sebelas uji `I01` dan satu uji `NET` baru di skenario `acceptance`, dilewati bersama uji TLS pada
mesin tanpa kartu, tanpa entropi, atau tanpa disk:

| Uji | Hasil di sesi ini |
|---|---|
| kredensial: sebelum ada → `not-ready`; salah → `auth` (401 dari penyedia, **1** percobaan); benar → jawaban | lulus |
| streaming: 4 potong tiba satu per satu | potongan pertama 216 ms, akhir 668 ms; biaya = token × harga, dan belanja naik tepat sebesar itu |
| alat: model membaca `/spaceos/ws/input.txt` lewat broker dan mengutipnya; tanpa alat ditawarkan, tidak ada panggilan | 2 request, 1 panggilan `allowed` |
| alat untuk kredensial: `read_file /spaceos/cred/cloud.key` | broker menolak (`denied-scope`), penyedia hanya menerima penolakan, kunci tidak ada di jawaban |
| tenggat 1500 ms pada jawaban yang macet | berakhir 1501 ms, ask berikutnya dilayani |
| penyedia selalu 529 | tepat 3 percobaan, 3 koneksi |
| penyedia melewati `max_tokens` | diputus pada laporan pertama (400 > 100 token), 6078 µ$ ditagihkan |
| ask yang bisa melampaui budget | ditolak, **0 frame**, 0 koneksi |
| ask local-only | ditolak, **0 frame**, 0 koneksi |
| event bersarang 120 tingkat | `protocol`, adapter tetap hidup dan menjawab ask berikutnya |
| penutupan | keluar 0; audit broker memuat baca workspace (`allowed`) dan baca kredensial (`denied-scope`) |

Sisi penyedia juga dituntut harness: log lab harus memuat 401, keempat potongan, `tool_result`
119 byte, penolakan alat, klien yang pergi dari `lab-stall` dan `lab-runaway`, dan percobaan ke-3
yang dijawab 529; dan tidak boleh memuat `nobody stopped it`, `THE CREDENTIAL LEAKED`, percobaan
ke-4, atau model `lab-budget`/`lab-local` — keduanya ditolak di guest dan tidak boleh sampai.

Giginya terbukti — satu pelemahan per run, source dipulihkan sesudahnya (rinciannya di
`docs/evidence/cloud-summary.txt`):

| Pelemahan | Akibat |
|---|---|
| pemeriksaan local-only dihapus | uji local-only merah, dan log penyedia memuat `cloud: 404: model lab-local` — ask itu sampai ke luar |
| reservasi budget dihapus | uji budget merah; `lab-budget` sampai ke penyedia |
| penjaga `max_tokens` dihapus | diputus baru oleh penjaga budget di 3200 token: 48078 µ$ ditagih, belanja 52317 dari budget 50000 |
| teks ditahan sampai `message_stop` | uji streaming merah: `1 pieces arrived; the provider sent 4` |
| broker tanpa pemeriksaan scope | panggilan alat untuk kredensial `allowed`, dan log penyedia: `THE CREDENTIAL LEAKED (55 bytes)` |
| `spacenet` menutup dengan FIN, bukan RST | uji guest tetap hijau, tetapi harness merah: `chargen: sent 514892 bytes over 10 s and nobody stopped it` |
| batas kedalaman JSON dihapus | uji JSON bermusuhan merah (`timeout` alih-alih `protocol`) |
| 401 dianggap bisa dicoba ulang | uji kredensial merah: 3 percobaan untuk kunci yang salah |

Run teeth yang sama juga menemukan bug di kode uji: bila pemeriksaan akhir gagal, `init` menunggu
broker yang tidak pernah disuruh berhenti, dan seluruh run macet sampai batas waktu harness. Kini
broker dan adapter dibunuh bila pemeriksaan gagal, sehingga kegagalan selalu berakhir dengan laporan.

## Konsekuensi

- **I01 terverifikasi terhadap penyedia tiruan lab, bukan terhadap layanan sungguhan.** Mengarah
  ke penyedia nyata butuh endpoint, trust store dengan root CA publik (ADR-0017 belum punya),
  kunci API, dan jaringan di luar `restrict=on`. Tidak ada perubahan kode adapter yang diharapkan,
  tetapi itu belum diuji — label integrasinya tetap *experimental*.
- Token dan biaya berasal dari yang dilaporkan penyedia; estimasi input untuk reservasi sengaja
  berlebih, jadi budget bisa menolak ask yang sebenarnya muat.
- Satu ask pada satu waktu, hingga 4 klien; prompt ≤ 176 byte per ask (satu pesan channel), tanpa
  riwayat percakapan dari klien; satu alat (`read_file`), tanpa tulis lewat model.
- Kunci dipegang di memori adapter dan tidak dihapus saat keluar. Di disk, kunci terbaca oleh
  siapa pun yang memegang `FS` atas volume (`init`, broker); broker hanya melayani workspace.
