# Space OS — Deskripsi produk dan persyaratan pengembangan

PRD versi 0.1 | 15 September 2026 | Usulan untuk persetujuan produk

> Salinan markdown dari `Space_OS_PRD_v0_1.docx` agar dapat dirujuk dari kode, ADR, dan
> backlog. Dokumen asli tetap menjadi sumber otoritatif.

Dokumen ini menetapkan arah Space OS dan kriteria pengembangan dari prototipe kernel sampai ekosistem AI native. Tujuan terdekat adalah membuktikan boot mandiri, isolasi proses, dan inferensi CPU di atas kernel sendiri sebelum memperluas desktop, integrasi agent, GPU, dan ARM64.

## Deskripsi produk yang diperbaiki

Space OS adalah proyek sistem operasi AI-native dengan kernel baru yang dibangun dari nol. Sistem ini dirancang untuk menjalankan layanan inti, aplikasi, model lokal, dan agent secara native, tanpa menjadikan Linux, Windows, atau macOS sebagai fondasi runtime sistem.

Di dalam Space OS, AI dapat melaksanakan pekerjaan melalui kemampuan sistem yang terstruktur: mencari informasi, mengelola file, menulis dan menjalankan kode, mengoperasikan aplikasi, serta menyelesaikan alur kerja lintas layanan. Pengguna menentukan ruang kerja, izin, dan tingkat otonomi setiap agent, dengan akses untuk meninjau, menghentikan, dan memulihkan tindakan yang mendukung pemulihan.

SpaceLink direncanakan sebagai layanan konteks bersama. Layanan ini menghubungkan file, proyek, aplikasi, riwayat tugas, dan hasil kerja melalui indeks serta hubungan antardata yang diperbarui secara inkremental. AI mengambil konteks yang relevan tanpa memindai ulang seluruh perangkat pada setiap permintaan. Setiap hasil tetap mengikuti izin akses dan memuat rujukan ke sumbernya.

Space OS menargetkan integrasi Codex, Claude Code, Hermes, local LLM, dan plugin melalui antarmuka yang sesuai. Model lokal melakukan inferensi di perangkat; model tertutup berbasis cloud tetap berjalan di server penyedia dan menggunakan kemampuan OS melalui konektor yang diizinkan.

Target perangkat meliputi x86-64 Intel/AMD dan ARM64 secara bertahap. Desktop mengutamakan konsistensi, kenyamanan visual, dan interaksi langsung, dengan inspirasi pengalaman macOS serta identitas desain Space OS sendiri. Optimasi berbantuan AI diarahkan pada efisiensi komputasi dan memori yang terukur, bukan penambahan kemampuan fisik perangkat.

## 1 Tujuan dan batas produk

### Pengguna dan kebutuhan utama

- Pengembang membutuhkan lingkungan kerja tempat agent memahami proyek, mengubah kode dalam ruang kerja yang disetujui, dan menunjukkan hasil pengujian.
- Pengguna AI lokal membutuhkan inferensi offline, kendali model dan memori, serta pilihan untuk tidak mengirim data ke penyedia cloud.
- Pengembang plugin membutuhkan SDK, manifest izin, kontrak alat, dan pengujian kompatibilitas yang terdokumentasi.

### Definisi native yang menjadi syarat produk

Kernel, pengelolaan proses, syscall, IPC, driver inti, dan layanan dasar berjalan di Space OS sendiri. Local inference, SpaceLink, dan Space Shell harus berjalan sebagai program user-space yang menargetkan ABI Space OS. Build lintas platform dan pengujian menggunakan QEMU diperbolehkan; sistem tamu tidak boleh bergantung pada kernel Linux atau inferensi di host untuk melewati uji native.

Pemakaian spesifikasi terbuka, compiler, firmware UEFI, dan pustaka yang di-port dengan lisensi sesuai diperbolehkan. Kernel baru tidak berarti seluruh algoritma, format file, atau pustaka harus ditemukan ulang. Program melalui compatibility layer harus diberi label berbeda dari program native.

### Cakupan rilis

| Tahap | Termasuk | Belum menjadi syarat |
|---|---|---|
| MVP kernel AI | QEMU, kernel, isolasi, ABI CPU, model kecil offline | Desktop lengkap, cloud, GPU, NPU |
| Developer Preview | SpaceLink, SDK alat, agent, desktop dasar, adapter terbatas | Semua CLI atau plugin pihak ketiga |
| Hardware Preview | Satu PC referensi dan GPU terpilih | Seluruh PC Intel/AMD atau ARM |
| Rilis berikutnya | ARM64, NPU terpilih, perluasan aplikasi | Kompatibilitas universal Apple Silicon |

### Batas yang tidak boleh disamarkan

"AI dapat melakukan apa saja" berarti cakupan tindakan dapat diperluas melalui API dan plugin, bukan jaminan semua tugas berhasil atau akses tanpa batas. Tidak ada target untuk membuka bobot model tertutup, menghapus aturan penyedia, menjalankan semua aplikasi Windows/macOS, atau menyediakan context window tak terbatas.

## 2 Arsitektur native

Usulan awal adalah microkernel berorientasi capability: kernel menjaga batas akses dan sumber daya; layanan kompleks ditempatkan pada user-space. Rust menjadi bahasa utama yang diusulkan, dengan Assembly terbatas untuk boot, interrupt, dan perpindahan konteks. Keputusan final dicatat melalui Architecture Decision Record sebelum implementasi.

| Lapisan | Komponen | Tanggung jawab |
|---|---|---|
| Antarmuka | Space Shell dan aplikasi | Jendela, pencarian, terminal, file manager, kontrol agent |
| Orkestrasi | Agent Runtime dan Tool Broker | Rencana tugas, izin, eksekusi alat, audit |
| Konteks | SpaceLink | Indeks, relasi, pembaruan, retrieval dan provenance |
| Inferensi | AI Runtime dan Model Router | Tokenizer, model, KV cache, routing dan batas biaya |
| Komputasi | Space Compute service | Buffer, antrean, dispatch, backend CPU/GPU/NPU |
| Layanan OS | VFS, jaringan, driver, package service | Penyimpanan, protokol, perangkat, lifecycle paket |
| Kernel | Space Kernel dan HAL | Address space, thread, IPC, capability, timer, interrupt |

### Batas kernel dan layanan

Kernel tidak memuat tokenizer, prompt, graph semantik, atau logika pemilihan model. Memory object, pemetaan halaman, antrean IPC, dan kuota merupakan primitive kernel; tensor dan inferensi merupakan abstraksi layanan komputasi. Identitas Agent adalah peran yang dikelola runtime, bukan keharusan membuat jenis proses kernel baru.

Driver user-space menerima akses MMIO, interrupt, dan DMA secara terbatas. Isolasi DMA membutuhkan dukungan IOMMU dan implementasi yang diuji; tanpa itu, driver DMA tetap diperlakukan sebagai komponen tepercaya. Kebijakan tidak boleh menyatakan driver aman hanya karena berada di user-space.

### Rantai boot dan pemulihan

UEFI memuat bootloader, menyerahkan memory map dan framebuffer, lalu kernel menginisialisasi memori, interrupt, scheduler, dan proses init. Init menjalankan layanan minimum. Jalur recovery harus tetap tersedia ketika model, indeks, atau desktop gagal; fungsi dasar OS tidak bergantung pada keluaran LLM.

## 3 SpaceLink sebagai layanan konteks

SpaceLink dalam PRD ini merupakan kontrak integrasi yang diusulkan. Repository rujukan ditetapkan oleh pemilik produk; kompatibilitas kode, skema, lisensi, dan dependensinya menjadi syarat peninjauan sebelum port native dimulai.

### Cara konteks tetap terhubung

Pengguna memilih folder dan sumber yang boleh diindeks. Sistem melakukan indeks awal pada cakupan tersebut, kemudian memproses event create, modify, rename, delete, dan perubahan izin. Journal dengan nomor urut serta checkpoint memungkinkan kelanjutan setelah restart. Jika ada celah event, sistem melakukan rekonsiliasi terbatas; full rescan hanya dilakukan saat diperlukan atau diminta.

Retrieval menggabungkan metadata, pencarian teks, dan hubungan antardata. Embedding semantik bersifat opsional agar sistem tetap berfungsi tanpa model embedding. Pemilihan konteks memperhitungkan relevansi, versi sumber, dan anggaran token masing-masing model.

| Objek | Data minimum |
|---|---|
| Resource | ID stabil, URI, tipe, pemilik, scope izin, versi, hash isi |
| Chunk | Resource ID, rentang teks, versi, hash, embedding version opsional |
| Relation | Sumber, tujuan, jenis relasi, asal bukti, waktu pembaruan |
| Task memory | Tujuan, keputusan, artefak, status, scope dan masa simpan |
| Context bundle | Daftar sumber dan versi, token budget, expiry, redaksi, penerima |

### Kontrak pencarian dan privasi

API usulan mencakup query, resolve, subscribe, invalidate, forget, dan build_context. Query membawa identitas pemanggil, workspace, anggaran token, serta tujuan local/cloud. Izin diperiksa saat retrieval dan diperiksa kembali sebelum data atau tindakan dikirimkan; hasil cache harus terikat identitas dan versi kebijakan.

Penghapusan sumber membuat tombstone dan membatalkan cache aktif. Revokasi izin harus memblokir retrieval baru meskipun penghapusan fisik indeks belum selesai. Secret dikecualikan secara default. Riwayat lintas pengguna tidak digabung. Ringkasan bukan sumber otoritatif dan selalu ditautkan ke bukti aslinya.

Satu indeks dapat melayani beberapa model, tetapi bobot, tokenizer, KV cache, dan context window model tidak otomatis dapat dibagikan. Data yang sudah dikirim ke cloud tidak dapat ditarik kembali hanya dengan menghapus indeks lokal.

## 4 Runtime model dan ekosistem agent

### Space Compute ABI versi awal

ABI versi 0 menetapkan representasi tipe dan ukuran yang eksplisit, opaque handle, aturan ownership, error code, timeout, cancellation, serta negosiasi versi. Hindari pointer mentah lintas proses. GPU dan NPU ditambahkan sebagai backend; aplikasi wajib menerima error unsupported jika operasi tidak tersedia.

- Primitive usulan: device_query, buffer_create, buffer_map, queue_create, submit, wait, cancel, release.
- Pemuatan model, tokenisasi, dan generate merupakan API AI Runtime di atas layanan Compute, bukan syscall kernel.
- Backend CPU pertama menggunakan operasi yang dibutuhkan satu arsitektur model terpilih. GGUF atau format lain hanya didukung untuk subset yang dinyatakan, bukan seluruh model dalam format tersebut.

### Eksekusi model lokal

Runtime memverifikasi checksum model, metadata, ukuran tensor, dan batas alokasi sebelum memuat. Worker inferensi memiliki kuota RAM, batas token dan waktu, serta penghentian kooperatif di antara langkah komputasi. Model malformed ditolak tanpa crash kernel. Scheduler CPU deterministik tetap menjadi fallback saat kebijakan optimasi AI gagal.

### Integrasi yang perlu dibuktikan satu per satu

| Target | Rute usulan | Kriteria dukungan |
|---|---|---|
| Model lokal | Port engine CPU ke ABI Space OS | Inferensi offline di guest; model dan lisensi dipatok |
| Codex dan Claude Code | Port/compatibility jika memungkinkan; adapter layanan terpisah | Uji runtime, autentikasi, streaming dan tool execution; bukan sekadar chat API |
| Hermes | Identifikasi proyek dan versi dahulu | Audit dependensi, lisensi, transport dan uji tugas |
| Plugin | Manifest dan protokol Tool Broker | Schema tervalidasi, izin terbatasi dan contract test |

Adapter layanan cloud membutuhkan DNS, TCP/IP, TLS, trust store, waktu sistem, dan penyimpanan kredensial. Port CLI dapat membutuhkan libc, proses anak, terminal, atau runtime bahasa yang belum tersedia. Status integrasi harus ditampilkan sebagai planned, experimental, atau verified beserta versi yang diuji.

Router tidak boleh mengalihkan tugas local-only ke cloud. Retry memiliki batas biaya dan jumlah percobaan; retry tindakan eksternal harus memperhatikan idempotency. Perpindahan model membangun context bundle baru, bukan menyalin state internal model secara sembarang.

## 5 Izin dan kendali tindakan AI

Space Guard menerapkan default-deny. Agent menerima capability sesuai workspace, alat, tujuan jaringan, masa berlaku, dan kuota. Model menghasilkan usulan tindakan; Tool Broker memvalidasi schema dan izin sebelum mengeksekusi. Teks file, hasil pencarian, dan keluaran plugin diperlakukan sebagai data tidak tepercaya, bukan instruksi otoritatif.

| Mode | Tindakan | Persetujuan |
|---|---|---|
| Observe | Membaca sumber yang diizinkan dan memberi saran | Scope baca disetujui di awal |
| Assist | Menyiapkan patch, preview, atau rencana | Pengguna menyetujui perubahan |
| Autonomous scoped | Menjalankan rangkaian tugas dalam workspace dan anggaran | Persetujuan awal berbatas; eskalasi bila scope berubah |

### Operasi berisiko

Penghapusan massal, format disk, perubahan boot, pemasangan driver, pengambilan secret, pengiriman data keluar, dan publikasi membutuhkan kebijakan khusus serta persetujuan yang jelas. Autonomi workspace tidak memberikan hak administrator. Layanan pemeriksa izin harus gagal tertutup bila tidak tersedia.

Tindakan file menggunakan staging dan preview diff bila memungkinkan. Undo berlaku untuk operasi yang memang reversibel dan memiliki snapshot; pesan terkirim, unggahan cloud, dan perubahan perangkat eksternal tidak boleh dijanjikan dapat dibatalkan. Berkas symlink dan pergantian versi harus diperiksa untuk mencegah akses keluar scope dan race condition.

### Manifest agent dan plugin

Manifest minimum memuat package ID, versi, publisher, hash, entry point, arsitektur, ABI range, tool schema, file scopes, network allowlist, quota, timeout, dan kebijakan update. Secret diberikan melalui broker, tidak dimasukkan ke prompt. Tanda tangan paket memverifikasi asal dan integritas, bukan menjamin perilaku aman.

### Audit dan penghentian

Audit menyimpan task ID, identitas model/adapter, capability, sumber konteks, tool, persetujuan, waktu, exit status, dan artefak hasil. Isi sensitif direduksi. Pengguna dapat menghentikan antrean, mencabut capability, dan mematikan worker; tindakan eksternal yang sudah selesai tetap tercatat sebagai selesai, bukan dibatalkan.

Uji ancaman mencakup prompt injection dalam dokumen, plugin palsu, traversal path, kebocoran konteks antaragent, resource exhaustion, model rusak, dan crash driver. Cacat isolasi atau secret disclosure yang diketahui menghalangi rilis publik.

## 6 Desktop dan perangkat

### Space Shell yang native

Desktop menggunakan compositor dan toolkit yang menargetkan layanan grafis Space OS. MVP grafis memakai software rendering dan framebuffer/VirtIO display. Antarmuka web di host tidak dihitung sebagai desktop native. Aplikasi web atau compatibility runtime boleh ditambahkan kemudian dengan label yang jelas.

- Dock dan pengelola jendela mendukung fokus, resize, minimize, perpindahan workspace, serta navigasi keyboard.
- Command Center menampilkan pencarian SpaceLink, sumber hasil, status indeks, dan pilihan tindakan.
- Agent Center menampilkan rencana, progres, izin, konsumsi sumber daya, biaya cloud, preview perubahan, dan tombol Stop.
- File manager, terminal, pengaturan model, serta recovery dapat digunakan tanpa AI atau jaringan.
- Aksesibilitas mencakup scaling, kontras, reduce motion, fokus yang terlihat, dan semantik kontrol untuk API otomasi.

### Profil pengujian awal yang diusulkan

QEMU x86-64 dengan UEFI, 4 vCPU, 8 GiB RAM, disk virtual 32 GiB, VirtIO block/network, framebuffer lalu VirtIO display. Ini profil laboratorium, bukan spesifikasi minimum produk. Host, versi QEMU/firmware, mode emulasi/akselerasi, dan flag CPU harus dipatok agar hasil dapat dibandingkan.

Mulai dengan model berlisensi sesuai yang kecil dan dapat direproduksi, sekitar 100–500 juta parameter bila cocok dengan engine. Pilihan model, quantization, tokenizer, hash, dan expected output dipatok sebelum pengujian. Tidak ada jaminan kecepatan inferensi sebelum benchmark pertama.

### GPU dan ARM64

Pilih satu GPU setelah studi dokumentasi, lisensi firmware, command submission, memory management, compiler, dan reset/recovery. VirtIO display tidak membuktikan kemampuan compute GPU. Pembuktian driver GPU fisik memerlukan uji perangkat fisik terbatas sebelum Hardware Preview dianggap stabil.

ARM64 merupakan port kernel dan user-space tersendiri dengan dukungan interrupt controller, timer, page table, firmware, dan perangkat yang dipilih. CPU Intel/AMD tetap x86-64; NPU tidak mengubahnya menjadi ARM. Apple Silicon bukan target otomatis dari port ARM64.

### Optimasi yang dapat dinonaktifkan

AI optimizer hanya memberi kebijakan dalam batas aman untuk penjadwalan, prefetch, cache dan penempatan model. RAM tidak bertambah; penghematan berasal dari penggunaan yang lebih efisien. Aktifkan optimizer secara default hanya jika A/B benchmark memperlihatkan manfaat bersih, termasuk overhead inferensi dan energi.

## 7 Persyaratan dan kriteria penerimaan

P0 wajib untuk MVP kernel AI. P1 wajib untuk Developer Preview. P2 adalah perluasan perangkat. Setiap requirement harus memiliki pemilik teknis dan bukti uji dalam backlog sebelum dikerjakan.

| ID | Prioritas | Persyaratan dan bukti penerimaan |
|---|---|---|
| K01 | P0 | Boot UEFI ke init di QEMU; 100 cold boot berturut-turut tanpa panic. |
| K02 | P0 | User-space, syscall, IPC, timer dan isolasi; akses memori terlarang mematikan proses uji, bukan kernel. |
| K03 | P0 | Kuota dan reclamation; worker dihentikan dan buffer dilepas pada siklus berulang tanpa kebocoran yang terus tumbuh. |
| D01 | P0 | VirtIO block dan VFS; baca model dari disk guest serta verifikasi checksum setelah reboot. |
| C01 | P0 | Compute ABI CPU; uji versi, invalid handle, batas buffer, unsupported op, timeout dan cancel. |
| A01 | P0 | Model kecil menghasilkan 128 token offline; hasil atau toleransi numerik cocok dengan baseline yang dipatok. |
| L01 | P1 | SpaceLink indeks awal dan inkremental; modify, rename, delete dan restart menghasilkan versi yang benar. |
| L02 | P1 | Revokasi izin memblokir hasil baru sebelum akses diberikan; cache lama tidak membocorkan isi. |
| L03 | P1 | Context bundle memuat sumber/versi dan tidak melampaui anggaran tokenizer model. |
| G01 | P1 | Agent membaca, membuat patch dan menjalankan uji dalam workspace; keluar scope ditolak dan dicatat. |
| I01 | P1 | Satu adapter cloud lulus auth, streaming, tool use, timeout, cost cap dan local-only negative test. |
| U01 | P1 | Desktop, terminal, file manager dan Stop dapat dipakai ketika worker inferensi crash. |
| P01 | P1 | Paket bertanda tangan dan rollback update diuji; signature salah ditolak. |
| H01 | P2 | Satu PC/GPU referensi lulus inference correctness, stress, recovery dan benchmark terhadap CPU. |
| H02 | P2 | ARM64 lulus boot, isolasi dan inferensi CPU pada target virtual sebelum perluasan hardware. |

## 8 Roadmap dan hasil tiap tahap

Roadmap menggunakan syarat kelulusan, bukan tanggal yang belum didukung kapasitas tim. Urutan tujuh tahap tetap dipertahankan; SpaceLink dan desktop ditambahkan setelah fondasi inferensi CPU terbukti. Arsitektur harus modular sejak awal untuk menghindari penulisan ulang saat port ARM64.

| Tahap | Hasil kerja | Syarat beralih |
|---|---|---|
| 1 Kernel boot | Bootloader UEFI, kernel, memori awal, serial dan crash log | Boot berulang serta diagnosis panic tersedia |
| 2 User space CPU | Address space, syscall, ELF loader, IPC, allocator, scalar compute | Isolasi proses dan correctness operasi CPU |
| 3 Perangkat virtual | VirtIO block, VFS, network, input, display secara bertahap | Model dapat dibaca dari disk guest; uji driver lulus |
| 4 Compute ABI | Kontrak versi 0, CPU backend, handle dan quota | Contract test dan cancellation lulus |
| 5 Inferensi native | Model kecil, tokenizer, generation, benchmark offline | A01 lulus tanpa proses inferensi host |
| 5A Developer Preview | SpaceLink, Tool Broker, agent, desktop dan adapter pertama | L01–L03, G01, I01, U01 dan P01 lulus |
| 6 GPU terpilih | Studi kelayakan, driver, compute dan uji lab hardware | Correctness serta reset/recovery terbukti |
| 7 Hardware Preview | Installer, recovery, matriks hardware, port ARM64 bertahap | Uji perangkat referensi dan pemulihan lulus |

### Dependensi yang perlu diperhatikan

Tahap 2 menyiapkan kemampuan CPU; model lengkap dibuktikan pada tahap 5 setelah storage dan ABI tersedia. Jaringan tidak diperlukan untuk inferensi offline, tetapi menjadi prasyarat integrasi cloud. Pengembangan GPU pada tahap 6 memerlukan perangkat lab; tahap 7 berarti perluasan penggunaan fisik yang stabil, bukan pertama kali perangkat fisik disentuh.

### Urutan backlog pertama

Setujui ADR kernel/ABI dan target QEMU; buat build lintas target yang reproduktif; buktikan boot dan panic log; implementasikan memori dan user mode; tambahkan syscall/IPC serta negative tests; baru masuk storage dan komputasi. Setiap milestone menyertakan source, build instructions, image uji, log, dan daftar batasan.

## 9 Ukuran keberhasilan dan pengujian

Angka berikut adalah target penerimaan usulan, bukan hasil benchmark. Bekukan fixture, perangkat, versi software, dan konfigurasi sebelum menilai hasil. Pengukuran di QEMU tidak boleh dipasarkan sebagai performa perangkat fisik.

| Area | Target usulan | Metode |
|---|---|---|
| Stabilitas MVP | 100 boot; stress 8 jam tanpa panic | Log otomatis, fault injection dan monitoring memory |
| Freshness SpaceLink | p95 <= 2 detik untuk satu edit kecil | Fixture 10.000 file teks <= 100 MiB, indeks awal selesai |
| Retrieval SpaceLink | p95 <= 300 ms untuk pencarian teks/metadata hangat | 100 query tetap; tidak termasuk generation LLM |
| Kualitas retrieval | Recall@10 >= 90 persen | 100 query berlabel; hasil tidak berizin harus nol |
| Efisiensi konteks | Token masukan turun >= 30 persen | Dibanding full-context yang muat; keberhasilan tugas turun <= 2 poin persentase |
| Inferensi CPU | 128 token selesai dan hasil benar | Laporkan TTFT, token/detik, peak RSS; ambang kecepatan setelah baseline |
| Stop agent | Dispatch alat baru berhenti <= 1 detik | Ukur broker; worker stop <= 2 detik pada backend CPU uji |
| AI optimizer | Manfaat bersih >= 10 persen pada metrik utama terpilih | A/B >= 30 run, overhead dihitung, p95 UI tidak memburuk > 5 persen |

### Skenario ujung ke ujung

- Boot offline, muat model guest, hasilkan teks, hentikan worker, dan pastikan terminal tetap hidup.
- Indeks workspace, ubah satu file, lalu minta ringkasan dengan sumber terbaru tanpa full rescan.
- Ganti model dan bangun ulang bundle sesuai tokenizer serta izin, tanpa membuka folder yang dilarang.
- Cabut izin saat task berjalan; retrieval dan tool berikutnya ditolak, termasuk yang berasal dari cache.
- Crash indexer, isi disk, putuskan jaringan, dan berikan file model rusak; setiap kegagalan mempunyai status yang dapat dipahami serta jalur recovery.

Untuk pembandingan optimizer, satu metrik utama dipilih sebelum eksperimen: latency, throughput, atau energi. Jika hasil tidak memenuhi target, fitur tetap opsional atau dinonaktifkan. Tidak boleh memilih metrik terbaik sesudah melihat hasil untuk mengklaim peningkatan umum.

## 10 Risiko keputusan dan referensi

| Risiko | Dampak | Mitigasi |
|---|---|---|
| Driver GPU/NPU | Inferensi cepat tertunda | Batasi satu perangkat; CPU tetap jalur utama |
| Port CLI tertutup | Integrasi tidak dapat dijalankan native | Pisahkan adapter layanan dari kompatibilitas CLI; jangan beri label verified sebelum uji |
| Indeks usang atau bocor | Jawaban salah dan pelanggaran izin | Versi sumber, journal, pemeriksaan akses, invalidasi cache |
| Otonomi agent | Perubahan salah atau keluar scope | Capability, preview, quota, audit dan penghentian |
| Cakupan terlalu besar | Kernel tidak mencapai MVP | Bekukan P0; tunda store, browser penuh, FS baru dan universal hardware |
| Optimizer lebih mahal | Performa dan daya memburuk | A/B benchmark, budget dan fallback deterministik |

### Keputusan sebelum implementasi

- Pemilik produk menyetujui batas MVP, prioritas native, serta mode otonomi default.
- Lead kernel menetapkan ADR microkernel, bahasa, bootloader, ABI executable dan subset standar perangkat.
- Lead runtime menetapkan model uji, engine, lisensi, tokenizer dan baseline correctness.
- Pemilik SpaceLink menyediakan akses repository atau snapshot; tim mengaudit skema, dependensi, lisensi dan kontrak port.
- Identifikasi repository/produk Hermes yang dimaksud sebelum menetapkan kebutuhan kompatibilitas.
- Tetapkan tim dan kapasitas untuk kernel/driver, runtime, konteks/keamanan, UI/SDK, serta QA/release. Satu orang dapat memegang beberapa peran, tetapi jadwal harus disesuaikan.

### Referensi teknis

- UEFI Forum, Specifications: https://uefi.org/specifications. Digunakan sebagai rujukan boot dan firmware. Versi implementasi harus dipatok dalam ADR, bukan mengikuti perubahan dokumen otomatis.
- OASIS, Virtual I/O Device Version 1.3, Committee Specification Draft 01: https://docs.oasis-open.org/virtio/virtio/v1.3/virtio-v1.3.html. Rujukan keluarga perangkat virtual dan negosiasi fitur; status dokumen yang ditinjau adalah draft, bukan klaim standar final.
- SpaceLink, repository rujukan pemilik produk: https://github.com/yoshuaja0301/SpaceLink. Kesesuaian implementasi menjadi gerbang integrasi; persyaratan SpaceLink di dokumen ini adalah rancangan produk.

Seluruh nama API Space, susunan komponen, target kuantitatif, dan urutan rilis merupakan usulan desain PRD. Persetujuan PRD tidak berarti kernel, port provider, driver, atau produk telah selesai dibuat.
