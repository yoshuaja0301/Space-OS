# Space OS
## Persyaratan produk dan arsitektur sistem operasi native AI

Versi 0.2 | 6 Oktober 2026 | Status usulan baseline pengembangan

Pemilik produk: Yoshua Januardo Manurung. Pembaca: pemilik produk, pengembang kernel dan driver, pengembang runtime dan aplikasi, serta penguji. Penanggung jawab teknis per area ditetapkan sebelum implementasi.

Space OS adalah sistem operasi mandiri dengan kernel sendiri, layanan sistem sendiri, dan lingkungan kerja AI yang berjalan di atas ABI Space OS. Dokumen ini menetapkan kebutuhan produk dari boot UEFI sampai workspace, model, dan agent dapat digunakan secara terkontrol. Pengalaman workspace terpadu mengambil inspirasi dari konsep Odysseus; implementasi sistem, model keamanan, dan kontrak native tetap milik Space OS.

Keputusan utama versi ini adalah memisahkan tiga pembuktian: OS dasar siap dipakai, inferensi lokal berjalan native, dan workspace AI menyelesaikan pekerjaan. Keberhasilan salah satunya tidak dianggap menggantikan dua pembuktian lainnya. Seluruh angka penerimaan merupakan target usulan, bukan hasil benchmark atau klaim kemampuan yang sudah tersedia.

## 1 Arah produk dan prinsip native

Space OS menyediakan lingkungan kerja tempat pengguna, aplikasi, dan agent mengerjakan proyek dengan konteks yang konsisten. Pengguna dapat bekerja secara manual, meminta saran, meninjau perubahan, atau memberikan otonomi terbatas. File, izin, tugas, sumber konteks, dan hasil kerja menjadi objek sistem yang dapat diperiksa.

Kernel baru tidak mengharuskan penulisan firmware motherboard, compiler, algoritma kriptografi, atau semua pustaka dari awal. UEFI, toolchain lintas platform, spesifikasi terbuka, dan pustaka yang sesuai lisensi dapat digunakan. Linux, Windows, dan macOS boleh menjadi host pengembangan; kernel dan layanan host tidak boleh menjadi runtime tersembunyi yang membuat pengujian native dianggap lulus.

### 1.1 Definisi dukungan

| Kategori | Definisi dan cara pelabelan |
| --- | --- |
| Native Space OS | Executable menargetkan ABI Space OS dan menggunakan layanan guest; kernel Space OS mengelola eksekusinya |
| Model lokal native | Tokenizer dan inference berjalan di guest dari model yang tersimpan atau dimuat ke guest; tidak memakai inference host |
| Adapter cloud | Layanan native menghubungi model eksternal; diberi label remote, bukan model lokal |
| Compatibility | Aplikasi memakai lapisan kompatibilitas; label compatibility wajib ditampilkan |
| Virtual machine | OS tamu lain berjalan dalam VM; aplikasi di dalamnya tidak dihitung sebagai native Space OS |
| Prototype host | Percobaan UX atau kontrak di OS pengembangan; tidak memenuhi gerbang native |

### 1.2 Prinsip yang mengikat

- Boot, recovery, scheduler dasar, keputusan izin, dan integritas penyimpanan tidak bergantung pada jawaban model.
- Model dan agent berjalan di user-space. Kernel tidak memuat prompt, tokenizer, graph konteks, atau kebijakan pemilihan model.
- Pengguna tetap dapat membaca file, mengelola proses, dan menghentikan worker ketika AI gagal.
- Akses agent diberikan per tugas dan workspace dengan batas waktu serta resource; default tidak mempunyai hak administrator.
- Data local-only tidak dialihkan ke cloud ketika backend lokal gagal.
- Setiap status dukungan menyebut target, versi, dan bukti uji. Nama provider atau format model tidak menyiratkan kompatibilitas penuh.
- Perubahan yang tidak dapat dipulihkan harus dinyatakan sebelum eksekusi. Undo tidak dijanjikan untuk pesan terkirim atau tindakan eksternal yang selesai.

### 1.3 Tujuan dan batas

Tujuan produk adalah membuktikan kernel mandiri, isolasi user-space, workspace yang persisten, inference CPU, tindakan aplikasi terstruktur, serta kendali tugas. Target jangka lanjut meliputi desktop yang nyaman, perangkat fisik terpilih, GPU compute, dan ARM64.

Di luar cakupan awal: firmware buatan sendiri, legacy BIOS, semua aplikasi Windows/macOS/Linux, browser engine penuh, dukungan seluruh GPU, telepon seluler, Apple Silicon, NPU universal, filesystem baru dari nol, dan optimizer AI untuk keputusan kritis kernel. Dukungan luas hanya ditambahkan setelah gerbang fondasi lulus.

## 2 Pengguna dan skenario utama

Pengguna awal adalah pengembang dan pengguna teknis yang bersedia bekerja dengan perangkat serta aplikasi terbatas. Pengguna berikutnya adalah penulis, peneliti, dan pengguna model lokal yang membutuhkan kontrol sumber data. Pengembang aplikasi dan plugin memerlukan SDK serta kontrak izin yang stabil.

| Skenario | Hasil yang diharapkan | Batas keberhasilan |
| --- | --- | --- |
| Menulis berbasis sumber | Membaca dokumen proyek, menyusun revisi, menampilkan sumber dan diff | Sumber berizin, perubahan disetujui sesuai mode, hasil tersimpan |
| Pengembangan kode | Agent membaca proyek, menyiapkan patch, menjalankan test terisolasi | Tidak keluar workspace; output dan exit status dapat diperiksa |
| Inferensi offline | Pengguna memilih model lokal dan menghasilkan teks tanpa jaringan | Proses inference native; terminal tetap responsif |
| Pekerjaan panjang | Tugas tetap tercatat ketika jendela ditutup atau sesi terputus | Checkpoint dapat direkonsiliasi; tidak mengulang efek eksternal sembarangan |
| Pemulihan | Pengguna menghentikan worker atau masuk console saat UI bermasalah | Data utama tetap dapat diakses sesuai kondisi storage |

Demonstrasi produk pertama: boot Space OS, buka workspace, baca dokumen, minta revisi dari model lokal, tinjau diff, simpan, reboot, lalu buka hasil yang sama. Model kecil pada gerbang teknis boleh hanya membuktikan inference; kemampuan revisi dokumen memerlukan fixture kualitas terpisah dan tidak diasumsikan muncul dari model 100 sampai 500 juta parameter.

## 3 Adaptasi pengalaman workspace

Odysseus dipakai sebagai referensi pengalaman menggabungkan chat, agent, dokumen, model, dan integrasi. Space OS mengadaptasi hubungan antarfungsi tersebut melalui layanan OS dan aplikasi native. Penyalinan kode tidak diasumsikan; setiap komponen yang hendak digunakan harus melewati audit dependensi, keamanan, dan lisensi.

| Konsep acuan | Komponen Space OS | Prioritas |
| --- | --- | --- |
| Workspace terpadu | Workspace Service dan Space Shell | P1 |
| Chat dan agent | Agent Runtime dan Task Service | P1 |
| Memory dan pengetahuan | SpaceLink | P1 |
| Model lokal dan API | Model Manager dan AI Runtime | P0 lokal, P1 remote |
| Tools dan skills | Tool Broker dan SDK aksi | P1 |
| Editor dokumen | Editor native dengan diff dan riwayat | P1 |
| Riset, email, kalender | Aplikasi dan connector tambahan | P2 |
| Tugas terjadwal | Task Service dengan kebijakan offline dan restart | P1 dasar, P2 integrasi |

Chat merupakan salah satu antarmuka. Status tugas, izin, dan hasil tidak disimpan hanya di transkrip chat. Aplikasi tetap menyediakan jalur manual. Workspace mempunyai identitas stabil, direktori sumber, kebijakan akses, daftar tugas, dan konfigurasi model tanpa menduplikasi seluruh file ke indeks.

## 4 Tahap rilis dan prioritas

P0 adalah fondasi dan MVP inferensi native. P1 adalah workspace AI yang persisten dan Developer Preview. P2 adalah perluasan perangkat dan aplikasi. Prioritas tidak menunjukkan bahwa semua fitur dalam satu kategori harus diimplementasikan sekaligus.

| Rilis | Cakupan wajib | Belum diwajibkan |
| --- | --- | --- |
| Foundation Preview | UEFI, kernel, user-space, IPC, capability, shell, storage baca | AI, jaringan, desktop |
| Native AI MVP | Foundation dan inference CPU offline, kuota, Stop | Workspace grafis lengkap, cloud |
| Workspace Developer Preview | Persistensi, editor, SpaceLink, tugas, broker, desktop dasar, satu adapter | Semua provider, email, kalender |
| Hardware Preview | Satu PC referensi, installer, recovery, matriks driver | Universal hardware, GPU compute universal |
| Compute dan Platform Expansion | GPU terpilih, port ARM64, aplikasi lanjutan | Dukungan otomatis semua perangkat ARM |

## 5 Arsitektur sistem

Baseline yang diusulkan adalah microkernel berorientasi capability. Rust menjadi bahasa utama; Assembly dibatasi pada bagian arsitektur yang membutuhkan kendali langsung. Keputusan final dan pengecualian dicatat dalam ADR. Tidak ada klaim formal verification atau isolasi sempurna hanya karena memakai Rust atau microkernel.

### 5.1 Pembagian komponen

| Lapisan | Komponen | Tanggung jawab utama |
| --- | --- | --- |
| Boot | Bootloader dan boot image | Memuat kernel, root task, konfigurasi, dan BootInfo |
| Kernel | Space Kernel dan HAL | Address space, thread, scheduler, IPC, capability, timer, interrupt |
| Bootstrap | Root task dan service manager | Membentuk layanan, mendelegasikan hak awal, readiness dan recovery |
| Platform | Process manager, device manager, driver | Lifecycle executable dan perangkat |
| Data dan konektivitas | VFS, filesystem, network, time, entropy | I/O, persistensi, koneksi, waktu dan sumber acak |
| Identitas | Identity, policy dan secret service | Sesi, otorisasi, capability tugas, pemberian kredensial |
| Aplikasi | Compositor, toolkit, terminal, editor, file manager | Antarmuka pengguna dan kontrak aksi |
| Konteks | Workspace Service dan SpaceLink | Scope proyek, indeks, sumber, versi, retrieval |
| AI | Model Manager, AI Runtime, Space Compute | Lifecycle model, generation dan resource komputasi |
| Tugas | Agent Runtime, Task Service, Tool Broker | Rencana, state tugas, validasi dan eksekusi alat |

### 5.2 Batas kepercayaan

Kernel menegakkan pemetaan memori dan hak terhadap objek kernel. Root task memperoleh hak awal dan mendelegasikan subset ke layanan. Policy service menerjemahkan kebijakan pengguna menjadi izin tugas; Tool Broker memeriksa operasi aplikasi. Proses agent tidak menerima akses disk atau jaringan luas yang memungkinkan bypass broker.

Driver DMA berada dalam trusted computing base apabila IOMMU belum tersedia dan teruji. Menempatkan driver di user-space tidak otomatis mencegah perangkat menulis ke memori proses lain. MMIO, interrupt, dan DMA diberikan per perangkat dengan lifecycle yang jelas.

### 5.3 Tiga jenis kontrak

Kernel ABI mengatur objek dan syscall minimum. OS Service API mengatur file, proses, network, identity, dan sesi. AI/Compute API mengatur model, buffer, generation, retrieval, dan aksi. ABI wire menggunakan ukuran tipe eksplisit, endianness yang dinyatakan, versi, error, ownership, dan batas panjang; layout Rust internal tidak menjadi ABI publik.

IPC membawa kontrol dan metadata. Buffer besar dapat memakai shared memory dengan hak minimum dan aturan lifetime. Operasi CPU di dalam worker dapat dijalankan langsung; tidak diwajibkan IPC untuk setiap operasi tensor. Aplikasi tidak diberikan pointer mentah milik proses lain.

## 6 Platform awal dan build

Target awal adalah QEMU x86-64 dengan machine q35, UEFI OVMF, disk GPT, console serial, dan perangkat VirtIO modern melalui PCI. Profil laboratorium: 4 vCPU, 8 GiB RAM, disk 32 GiB. Bring-up awal boleh mengaktifkan satu CPU; kelulusan SMP dicatat terpisah. Framebuffer GOP menjadi display awal. Semua ini adalah profil pengujian, bukan minimum produk.

Manifest platform mematok versi QEMU, hash firmware, CPU model dan flags, mode TCG/KVM, perangkat PCI, ukuran disk, image hash, compiler, linker, dan dependency lockfile. AVX tidak diasumsikan pada baseline; fitur CPU dipilih melalui deteksi dan fallback yang diuji. Perbedaan TCG dan akselerasi hardware tidak digabung menjadi satu benchmark.

Build harus menghasilkan kernel, bootloader atau konfigurasi bootloader, boot image, image disk, simbol debug, manifest versi, dan instruksi menjalankan uji. Build bersih tidak mengambil dependency tanpa versi. Reproducibility diukur dengan hash setelah timestamp dan metadata nondeterministik dikendalikan; pengecualian harus dicatat.

Toolchain silang tidak bergantung pada libc host untuk executable guest. Source dan build scripts harus cukup untuk membangun ulang image. CI menguji build, boot serial, negative tests, dan fixture data. Status implementasi tidak dinyatakan selesai hanya karena kompilasi berhasil.

## 7 Firmware dan rantai boot

Firmware UEFI menyiapkan platform lalu menjalankan loader EFI. Legacy BIOS dan penulisan firmware motherboard berada di luar baseline. Jalur removable media x86-64 menggunakan EFI/BOOT/BOOTX64.EFI; entry NVRAM dapat ditambahkan pada installer. Pemilihan bootloader yang sudah ada atau implementasi khusus wajib diputuskan dalam ADR sebelum pekerjaan loader dimulai.

### 7.1 Urutan startup

1. Bootloader membaca konfigurasi dan memilih slot normal atau recovery.
2. Loader memvalidasi format dan batas image, lalu menempatkan kernel serta boot image di RAM.
3. Loader memperoleh informasi framebuffer dan ACPI, menyiapkan BootInfo, serta mempertahankan wilayah yang masih digunakan.
4. Loader mengambil memory map yang berlaku, memanggil ExitBootServices dengan map key yang sesuai, dan menangani pembaruan map sesuai spesifikasi UEFI.
5. Kernel menerima kendali dengan stack dan mapping yang terdokumentasi; log awal dan exception handler disiapkan.
6. Kernel menginisialisasi allocator, proteksi halaman, timer, interrupt, syscall dan scheduler.
7. Kernel memulai root task dari boot image; root task memulai layanan minimum dari RAM.
8. Driver storage dan filesystem menjadi siap; layanan tambahan dimuat dari disk guest.
9. Shell atau sesi grafis tersedia; status OS usable dicatat.
10. Model dan layanan AI dimulai sesuai kebutuhan tanpa menjadi syarat keberhasilan boot OS.

Setelah ExitBootServices berhasil, Boot Services tidak dipanggil lagi. Memori runtime firmware dan wilayah yang masih hidup dipertahankan sesuai kontrak. Penggunaan Runtime Services dibatasi pada subset yang diputuskan; pointer firmware tidak boleh diasumsikan tetap valid setelah perubahan mapping.

### 7.2 Kontrak BootInfo

| Field | Aturan minimum |
| --- | --- |
| Identitas | Magic, versi, ukuran total, flags, validasi batas |
| Memory map | Alamat, jumlah/ukuran descriptor, versi format, jenis memori |
| Kernel dan boot image | Rentang fisik, ukuran, informasi validasi image |
| Framebuffer | Alamat, panjang, dimensi, pitch dan format piksel |
| Platform | ACPI RSDP pada x86; mekanisme target lain ditetapkan saat port |
| Parameter | Panjang terbatas, encoding, opsi recovery dan debugging |
| Reservasi | Rentang yang belum boleh direklamasi dan pemiliknya |
| Entropy | Data opsional, panjang dan sumber; kualitas tidak diasumsikan |
| Boot slot | Slot aktif, percobaan boot dan referensi state pemulihan |

Semua field alamat menyatakan fisik atau virtual, alignment, lifetime, dan kepemilikan. Kernel menolak versi yang tidak didukung dengan pesan diagnostik. BootInfo tidak memuat secret pengguna. Parser image dan struktur boot diuji dengan ukuran, offset, dan rentang yang rusak.

### 7.3 Boot image dan bootstrap

Boot image adalah arsip awal yang dibaca dari RAM. Isinya root task, layanan proses minimum, driver manager, driver storage awal, filesystem service, console recovery, dan manifest capability. Ini memecahkan ketergantungan ketika driver disk harus berjalan sebelum executable dari disk dapat dibaca.

Kernel hanya memuat program pertama melalui loader minimum yang tervalidasi. Program berikutnya dapat dikelola process manager. Boot image tidak boleh bergantung pada akses jaringan. Recovery image mempertahankan alat diagnosis walaupun root filesystem atau konfigurasi utama gagal.

### 7.4 Status keberhasilan

Kernel alive berarti kernel mengeluarkan log setelah handoff. User-space alive berarti proses terisolasi pertama berjalan. OS usable berarti shell dapat membaca berkas dan menjalankan serta menghentikan program. AI ready berarti runtime model siap. Boot-success untuk rollback didasarkan pada kesehatan layanan inti, bukan hasil inference atau SpaceLink.

## 8 Kernel dan manajemen sumber daya

### 8.1 Memori

Kernel menyediakan physical page allocator, virtual mapping, proteksi user/kernel, guard page, heap kernel, dan object accounting. Halaman writable tidak sekaligus executable kecuali pengecualian eksplisit yang ditinjau. Page fault pengguna menghasilkan error atau penghentian proses; fault kernel memicu diagnosis dan recovery sesuai tingkat kerusakan.

Shared memory mempunyai owner, hak baca/tulis, daftar mapping, dan lifecycle. Resource dikembalikan ketika proses mati. Pemetaan file, copy-on-write, swap dan memory compression tidak menjadi syarat P0. Batas alokasi model harus dapat dipenuhi tanpa fasilitas tersebut.

OOM ditangani melalui penolakan alokasi dan penghentian worker sesuai prioritas. Memori cadangan untuk console, broker Stop, serta layanan inti harus ditetapkan melalui pengukuran. Layanan tidak boleh memakai perkiraan model untuk memutuskan integritas allocator.

### 8.2 CPU dan thread

Implementasi x86 mencakup exception, IDT, GDT/TSS yang diperlukan, stack transisi privilege, timer, syscall entry/exit, dan context switch. State floating-point/SIMD disimpan serta dipulihkan dengan benar. Deteksi fitur CPU dan ukuran state menjadi bagian uji agar data numerik tidak rusak antarthread.

Scheduler awal bersifat preemptive dan mempunyai prioritas atau kelas layanan untuk menjaga kendali pengguna. Loop tak berujung di user-space tidak boleh mengunci seluruh sistem. SMP mencakup startup CPU tambahan, sinkronisasi, memory ordering, per-CPU state, dan TLB shootdown sebelum dinyatakan didukung.

### 8.3 Objek capability dan IPC

Objek minimum adalah address space, thread, memory object, endpoint IPC, timer, dan akses perangkat terbatas. Handle bersifat lokal ke proses dan harus tahan reuse yang salah. Transfer hak tidak menambah privilege; aturan delegasi, pengurangan hak, revokasi turunan, dan penghancuran objek ditulis sebelum implementasi.

IPC mempunyai batas pesan dan antrean, timeout, cancellation, serta respons terhadap peer yang mati. Revokasi memblokir operasi baru; operasi yang telah berjalan mengikuti semantik cancel yang dinyatakan. Risiko deadlock dan priority inversion pada layanan inti harus diuji. Kuota meliputi halaman memori, thread, handle, endpoint dan outstanding request.

## 9 Layanan dasar dan runtime aplikasi

Root task memulai dependency layanan dan mendelegasikan capability. Service manager memantau readiness, health, exit status, backoff restart, serta batas percobaan. Crash loop tidak boleh menghabiskan seluruh resource. Layanan yang gagal wajib menghasilkan status normal, degraded, atau recovery required.

| Layanan | Kontrak penting |
| --- | --- |
| Process manager | Spawn, exit, wait, kill, executable validation dan resource cleanup |
| Device manager | Enumerasi PCI, pemilihan driver, pemberian MMIO/IRQ/DMA |
| Filesystem dan VFS | Namespace, handle berkas, hak akses, mount dan error I/O |
| Network | Socket atau API setara, DNS, koneksi, timeout dan isolasi tujuan |
| Time | Monotonic clock untuk timeout, wall clock untuk waktu nyata |
| Entropy | Sumber acak dan CSPRNG; gagal tertutup bila belum siap untuk crypto |
| Identity dan secrets | Identitas sesi, scope aplikasi, kredensial melalui broker |
| Logging | Log terstruktur, bounded buffer, redaksi dan ekspor diagnosis |

Executable awal menggunakan subset ELF64 yang dipatok dan static linking. ABI menetapkan entry point, stack, argument, environment, thread-local storage bila diperlukan, error code, serta calling convention. Dynamic linking dan runtime bahasa besar dapat ditunda.

Library native minimum menyediakan alokasi, I/O, waktu, sinkronisasi, dan proses. Dukungan Rust std, libc/POSIX, Python, Node.js atau runtime lain harus diperlakukan sebagai port tersendiri. Tidak ada janji seluruh CLI dapat dijalankan hanya karena kernel memakai Rust.

Jaringan tidak wajib untuk P0 offline. Adapter remote memerlukan TCP/IP, DNS, TLS, trust store, waktu yang memadai, dan penyimpanan kredensial. Kesalahan sertifikat tidak diabaikan untuk membuat integrasi terlihat berhasil. Initial time dan pembaruannya harus ditangani tanpa bergantung secara melingkar pada koneksi TLS yang belum dapat divalidasi.

## 10 Storage dan persistensi

Disk awal menggunakan GPT dan EFI System Partition yang sesuai UEFI. Sistem memuat program awal dari boot image; VirtIO block digunakan untuk data guest. Transport, subset fitur, split virtqueue, status perangkat, reset, dan penanganan completion dipatok dalam ADR. Dukungan baca disk tidak dianggap membuktikan ketahanan penulisan.

P0 boleh memakai filesystem baca-saja untuk model dan fixture, ditambah penyimpanan sementara RAM. P1 wajib memiliki filesystem tulis dengan pilihan implementasi yang disetujui sebelum integrasi workspace. Format filesystem baru tidak dianjurkan sebagai pekerjaan awal; penggunaan implementasi yang tersedia harus sesuai lisensi dan API target.

### 10.1 Jaminan data

- API membedakan write diterima dari data durable. Semantik sync/flush dinyatakan dan diuji pada perangkat target.
- Penggantian dokumen memakai staging dan commit yang semantiknya jelas; atomic rename hanya diklaim bila implementasi menjaminnya.
- Disk penuh, short write dan I/O error tidak boleh dilaporkan sebagai sukses.
- Indeks dapat direkonstruksi; file pengguna, state tugas dan audit mempunyai kebijakan durability masing-masing.
- Version history mempunyai kuota dan masa simpan. Snapshot dan backup tidak dianggap identik.
- Restore diuji pada salinan data; rollback aplikasi memperhitungkan kompatibilitas skema.

Layout logis memisahkan system image, user data, model cache, indeks, task state, dan log. Nama direktori fisik ditetapkan dalam spesifikasi filesystem. Pemisahan ini memungkinkan reset indeks atau model tanpa menghapus dokumen pengguna.

## 11 Workspace dan SpaceLink

Workspace menyimpan ID, pemilik, akar berkas yang diizinkan, kebijakan local/cloud, model default, task references, dan konfigurasi retensi. Satu file dapat dirujuk tanpa harus digandakan ke workspace. Perubahan scope memicu evaluasi ulang izin dan cache.

SpaceLink adalah layanan konteks yang mengindeks sumber pilihan pengguna. Integrasi repository yang dirujuk pemilik produk memerlukan audit terpisah; nama yang sama tidak membuktikan kompatibilitas implementasi atau skema.

### 11.1 Model data

| Objek | Field minimum |
| --- | --- |
| Resource | ID stabil, URI, pemilik, tipe, versi, hash, scope, deleted flag |
| Chunk | Resource ID, rentang, versi, hash, tokenizer/embedding version bila ada |
| Relation | Sumber, tujuan, jenis hubungan, bukti, waktu pembaruan |
| Context bundle | Sumber dan versi, penerima, token budget, expiry, redaksi |
| Task memory | Task ID, keputusan, artefak, status, scope, retensi |

Indeks awal diikuti event create, modify, rename, delete, dan perubahan izin. Journal bernomor urut serta checkpoint memungkinkan pemulihan. Celah event memicu rekonsiliasi terbatas, bukan asumsi bahwa indeks pasti mutakhir. Full rescan tersedia sebagai pemulihan.

API minimum: query, resolve, subscribe, invalidate, forget, build_context. Retrieval memeriksa identitas dan versi kebijakan. Izin diperiksa kembali sebelum sumber dipakai untuk aksi atau dikirim ke provider. Cache terikat identitas, workspace, versi sumber, serta kebijakan.

Pencarian teks dan metadata tersedia tanpa embedding. Embedding menjadi peningkatan opsional. Ringkasan tidak menggantikan sumber otoritatif. Bundle menyebut sumber yang hilang atau usang dan tidak melampaui anggaran tokenizer model yang dipakai.

Revokasi memblokir akses baru segera setelah commit kebijakan. Tombstone menghapus sumber dari retrieval; pembersihan fisik indeks mengikuti retensi. Secret dikecualikan secara default. Penghapusan lokal tidak menarik kembali data yang telah terkirim ke cloud.

## 12 Model Manager dan inference

Model Manager menangani katalog lokal, download opsional, checksum, metadata, lokasi file, lisensi yang tersedia, kompatibilitas engine, load/unload, dan estimasi memori. Estimasi harus menyertakan bobot, KV cache, workspace komputasi, serta overhead runtime; ukuran file saja tidak cukup.

P0 memilih satu arsitektur model, format, quantization dan tokenizer. GGUF atau format lain hanya didukung pada subset yang diuji. Parser menolak dimensi, offset, metadata dan ukuran tensor yang tidak valid sebelum alokasi berbahaya. Worker inference berada dalam proses terpisah dengan kuota dan batas waktu.

Space Compute v0 menyediakan device_query, buffer_create/map, queue_create, submit, wait, cancel, release atau kontrak setara yang disetujui. Semua operasi mempunyai aturan ownership, batas buffer, error unsupported dan negosiasi versi. API load_model dan generate berada di AI Runtime, bukan syscall kernel.

Backend CPU scalar menjadi baseline correctness; optimasi SIMD ditambahkan sesudah pembuktian numerik dan deteksi fitur CPU. Cancellation kooperatif digunakan di batas operasi; penghentian paksa proses menjadi fallback. Kernel tetap dapat mempreempt worker yang tidak kooperatif.

Pengujian memakai seed, sampling, context length dan fixture tetap. Perbandingan lintas backend menggunakan toleransi numerik atau kualitas yang dinyatakan; kesamaan token absolut tidak selalu diwajibkan untuk floating-point berbeda. Model teknis kecil dipilih untuk membuktikan pipeline, lalu model fungsional dipilih untuk tugas pengguna setelah benchmark.

Router mematuhi local-only, batas biaya, tujuan pengiriman, dan pilihan pengguna. Perpindahan model membentuk context bundle baru; KV cache dan tokenizer tidak diasumsikan dapat dibagikan. Adapter mencatat status planned, experimental atau verified beserta versi yang diuji.

Codex, Claude Code, Hermes dan provider lain adalah target integrasi, bukan dependency boot. Repository, runtime, autentikasi, lisensi dan kontrak alat masing-masing diaudit. Koneksi chat API tidak otomatis dihitung sebagai port CLI atau agent tersebut.

## 13 Task Service dan Agent Runtime

Task Service menyimpan identitas dan status tugas di luar jendela chat. Agent Runtime menghasilkan rencana dan usulan aksi. Tool Broker mengeksekusi aksi yang lolos validasi. Model tidak memutuskan sendiri apakah sebuah capability sah.

State minimum: queued, running, waiting_approval, paused, succeeded, failed, cancelled, dan needs_reconciliation. Setiap transisi menyimpan alasan, waktu, serta aktor. Menutup UI tidak otomatis menghentikan tugas; logout dan reboot mengikuti kebijakan eksplisit.

| Data tugas | Isi wajib |
| --- | --- |
| Identitas | Task ID, pemilik, workspace dan parent task opsional |
| Scope | Capability, tujuan network, kebijakan data, expiry |
| Resource | Batas RAM, CPU policy, token, waktu dan biaya |
| Eksekusi | Model/adapter version, tools, langkah, attempt dan status |
| Persistensi | Checkpoint, operasi commit, referensi artefak |
| Audit | Persetujuan, sumber, error, exit status dan efek eksternal |

Checkpoint menyimpan state aplikasi yang dapat dipulihkan, bukan menjanjikan serialisasi semua state internal model. Setelah crash atau reboot, task memeriksa validitas izin, sumber dan operasi terakhir sebelum melanjutkan. Operasi eksternal berstatus tidak pasti masuk needs_reconciliation. Tidak ada jaminan exactly-once untuk layanan eksternal tanpa kontrak pendukung.

Retry mempunyai batas percobaan, backoff, dan idempotency key bila didukung. Scheduler tugas membatasi concurrency. Tugas terjadwal yang terlewat saat offline harus memilih skip, run-once, atau meminta pengguna sesuai kebijakan; jangan mengeksekusi seluruh backlog tanpa batas.

Stop menghentikan dispatch baru, meminta cancel operasi aktif, mencabut hak yang relevan, kemudian mematikan worker jika perlu. UI membedakan berhenti diminta, worker berhenti, dan efek eksternal yang sudah selesai. Satu tombol tidak boleh menampilkan semua tindakan seolah telah dibatalkan.

## 14 Space Guard dan kontrak aksi aplikasi

Mode Observe mengizinkan membaca scope yang disetujui. Mode Assist menyiapkan usulan serta preview perubahan. Mode Autonomous scoped mengeksekusi tindakan dalam batas workspace, alat, anggaran dan masa berlaku yang disetujui. Eskalasi scope meminta persetujuan baru tanpa mengulang persetujuan yang masih berlaku untuk tindakan yang sama.

Operasi format disk, perubahan boot, pemasangan driver, pengiriman data sensitif, akses secret, dan publikasi mempunyai kebijakan khusus. Persetujuan terikat parameter dan versi objek; perubahan material setelah preview membatalkan persetujuan lama. Penanganan path memakai identitas objek dan pemeriksaan race, bukan hanya pencocokan string awalan.

### 14.1 Manifest aksi

Manifest memuat action ID, versi schema, executable/service tujuan, input/output, capability, kategori efek, timeout, cancel policy, idempotency, preview, dukungan undo, serta audit fields. Broker memvalidasi ukuran dan tipe input. Output tool dan isi dokumen tetap diperlakukan sebagai data tidak tepercaya.

Aplikasi native menyediakan aksi seperti read_document, propose_patch, apply_patch, export_document, dan spawn_sandboxed_process. Nama final dipatok dalam SDK. API otomatisasi tidak boleh melampaui hak yang tersedia melalui sesi pengguna. Computer vision dapat menjadi fallback untuk aplikasi tanpa aksi terstruktur.

### 14.2 Secret dan connector

Secret diberikan oleh broker hanya ke proses/tujuan yang memerlukan dan tidak dimasukkan ke prompt. Log meredaksi token dan kredensial. Allowlist koneksi harus mempertimbangkan redirect, DNS, proxy, dan koneksi yang sudah terbentuk. Revokasi menjelaskan apakah koneksi aktif dihentikan.

Plugin membawa publisher, hash, signature, ABI range, entry point, scope file/network, quota dan update policy. Signature membuktikan asal dan integritas sesuai kunci tepercaya, bukan keamanan perilaku. Paket tidak dipercaya hanya karena menggunakan protokol MCP; protokol tetap memerlukan enforcement OS.

## 15 Space Shell dan aplikasi bawaan

Space Shell adalah desktop native yang mengutamakan workspace dan tetap mendukung pengelolaan jendela biasa. Compositor, input service dan toolkit menargetkan layanan Space OS. Software rendering dan framebuffer digunakan dahulu; VirtIO GPU display adalah langkah berikutnya dan tidak dianggap bukti GPU compute.

Aplikasi awal adalah terminal, file manager, editor teks/Markdown, settings, Model Manager dan Task Center. Tampilan menyediakan sumber konteks, local/cloud indicator, status indeks, task progress, biaya bila remote, diff, serta Stop. Layar tidak menampilkan rincian internal kernel kecuali pengguna membuka diagnosis.

Input service mengelola routing keyboard/mouse, fokus, dan batas akses antaraplikasi. Clipboard mengikuti sesi serta kebijakan data. Terminal mempunyai PTY atau mekanisme setara agar input interaktif, resize dan penghentian proses dapat digunakan. Compositor tidak menjadi tempat penyimpanan state tugas.

Aksesibilitas minimum mencakup navigasi keyboard, fokus terlihat, scaling, kontras, reduce motion dan semantic action tree. Pilihan bahasa awal adalah Indonesia dan/atau Inggris sesuai kapasitas; encoding internal UTF-8 dan kebutuhan font harus diuji dengan teks Indonesia serta karakter umum.

Crash compositor tidak menghentikan inference tanpa kebijakan yang jelas; console recovery tetap dapat diakses. Crash inference tidak merusak editor atau file manager. Kontrol resource dan status yang akurat lebih penting daripada animasi kompleks pada preview pertama.

## 16 Update installer dan recovery

P0 menyediakan boot image recovery dan diagnosis serial. Sebelum distribusi Developer Preview, paket bertanda tangan, pembaruan terkontrol dan rollback diuji. Sebelum Hardware Preview, installer harus mendeteksi disk target, menampilkan konsekuensi, serta meminta konfirmasi eksplisit sebelum operasi destruktif.

Strategi usulan adalah slot system A/B dengan user data terpisah. Slot baru ditandai kandidat, mempunyai batas percobaan, dan menjadi aktif setelah health check layanan inti. Jika gagal, loader memilih slot yang diketahui baik. Mekanisme state boot harus tahan gangguan dan tidak menganggap firmware selalu menyediakan storage variabel tanpa batas.

Migrasi data memakai backup/checkpoint dan kompatibilitas versi yang dinyatakan. Rollback binary tidak dilakukan apabila format data baru tidak dapat dibaca versi lama tanpa prosedur pemulihan. Recovery dapat memilih slot, menonaktifkan layanan tambahan, memeriksa storage dan mengekspor log tanpa model atau jaringan.

Secure Boot tidak diwajibkan pada laboratorium awal; statusnya ditampilkan jelas. Sebelum klaim trusted boot, tentukan rantai verifikasi loader, kernel, boot image, konfigurasi, kunci, revokasi dan recovery. Hash lokal tanpa sumber tepercaya tidak membuktikan keaslian. Measured boot/TPM dan enkripsi disk penuh merupakan pekerjaan tersendiri, bukan kemampuan implisit.

## 17 Perangkat fisik GPU dan ARM64

Hardware Preview membatasi satu PC referensi dengan daftar CPU, motherboard/firmware, storage, input, display, NIC, dan konfigurasi IOMMU. Driver minimum disesuaikan perangkat: misalnya NVMe atau AHCI, USB xHCI/HID, ACPI yang diperlukan, dan NIC terpilih. VirtIO tidak menggantikan driver perangkat fisik tersebut.

Bring-up PC dapat dimulai lebih awal dengan console atau framebuffer. GPU compute bukan prasyarat eksperimen boot fisik. Suspend/resume, audio, Wi-Fi, Bluetooth dan hotplug luas ditunda sampai ada requirement dan fixture khusus; reboot serta shutdown bersih tetap diuji pada target yang dinyatakan.

GPU dipilih setelah studi dokumentasi, firmware, submission, memory management, compiler, sinkronisasi, timeout, reset dan lisensi. Tidak ada asumsi CUDA, Metal atau driver vendor tersedia untuk kernel baru. Backend harus membuktikan correctness, isolasi buffer dan recovery sebelum klaim percepatan.

ARM64 adalah port tersendiri yang membutuhkan boot contract, page table, interrupt controller, timer, exception, atomics dan perangkat target. Target virtual didahulukan. Apple Silicon memerlukan studi platform khusus dan tidak termasuk otomatis. CPU x86 dengan NPU tetap merupakan platform x86.

## 18 Persyaratan nonfungsional

Stabilitas dan correctness menjadi gerbang awal. Performa diukur sesudah fixture dipatok; tidak ada janji lebih cepat daripada OS lain sebelum benchmark sebanding. Semua metrik menyebut konfigurasi, ukuran sampel, kondisi hangat/dingin dan metode pengukuran.

| Area | Target penerimaan usulan | Kondisi |
| --- | --- | --- |
| Boot | 100 cold boot tanpa panic | Image dan profil virtual dipatok |
| Stress | 8 jam tanpa kernel panic | Campuran IPC, proses dan alokasi |
| Stop | Dispatch baru berhenti maksimal 1 detik | Broker hidup, ukur dari permintaan diterima |
| Worker CPU | Berhenti maksimal 2 detik | Fixture CPU; kill fallback diuji |
| Retrieval | p95 maksimal 300 ms | 100 query tetap, indeks hangat |
| Freshness | p95 maksimal 2 detik | 10.000 file teks total maksimal 100 MiB |
| Izin | Nol hasil tidak berizin pada fixture | Termasuk cache dan revokasi |
| Inferensi | 128 token sesuai baseline/toleransi | Model, seed dan backend dipatok |
| Resource cleanup | Tidak ada pertumbuhan resource tak terbatas | Siklus create/kill dan shared buffer |

Peak RSS atau ukuran setara yang didefinisikan OS, TTFT, token/detik, CPU use, dan latency input dilaporkan. Ambang kecepatan inference dan boot ditetapkan setelah baseline. Optimizer AI tetap nonaktif secara default sampai A/B terkontrol membuktikan manfaat bersih termasuk overhead dan dampak respons UI.

## 19 Matriks requirement dan bukti penerimaan

Setiap ID masuk backlog dengan owner, dependency, test artifact, dan status. P0/P1/P2 mengacu pada tahap rilis, bukan izin untuk melewati dependency.

| ID | Tahap | Requirement dan bukti |
| --- | --- | --- |
| B01 | P0 | UEFI boot ke kernel; 100 cold boot dan log handoff |
| B02 | P0 | BootInfo tervalidasi; versi/offset rusak ditolak |
| B03 | P0 | Root task dimuat dari boot image tanpa driver disk aktif |
| B04 | P0 | Kernel alive, user-space alive dan OS usable dapat dibedakan |
| K01 | P0 | User memory fault mematikan proses uji, bukan kernel |
| K02 | P0 | Preemption menghentikan loop CPU tak kooperatif |
| K03 | P0 | FPU/SIMD context tidak bocor atau rusak antarproses |
| K04 | P0 | Invalid/stale handle dan transfer hak berlebih ditolak |
| K05 | P0 | IPC timeout, peer death dan kuota antrean lulus |
| K06 | P0 | Kill proses melepaskan halaman, handle dan buffer terkait |
| K07 | P0 | Profil SMP hanya dinyatakan didukung setelah uji multicore |
| S01 | P0 | ELF malformed ditolak tanpa kernel crash |
| S02 | P0 | Shell menjalankan, menunggu dan menghentikan program guest |
| D01 | P0 | Model dibaca dari disk guest dan hash cocok setelah reboot |
| C01 | P0 | Compute menguji versi, bounds, cancel dan unsupported op |
| A01 | P0 | 128 token offline di guest tanpa inference host |
| A02 | P0 | Model rusak dan OOM worker tidak menutup console |
| D02 | P1 | Write, flush, disk penuh dan crash recovery sesuai kontrak |
| W01 | P1 | Workspace serta dokumen hasil bertahan setelah reboot |
| L01 | P1 | Modify/rename/delete dan journal gap menghasilkan indeks benar |
| L02 | P1 | Revokasi menghalangi retrieval baru termasuk cache |
| L03 | P1 | Bundle bersumber dan sesuai tokenizer budget |
| T01 | P1 | State tugas persisten setelah UI restart dan reboot |
| T02 | P1 | Efek eksternal tak pasti masuk reconciliation, tidak blind retry |
| G01 | P1 | Agent membuat patch dalam scope; traversal/race diuji |
| G02 | P1 | Stop menghentikan dispatch dan worker sesuai target |
| U01 | P1 | Editor, terminal dan file manager hidup saat inference crash |
| U02 | P1 | Keyboard, fokus, scaling dan recovery console dapat digunakan |
| I01 | P1 | Satu adapter lulus auth, streaming, timeout dan local-only test |
| P01 | P1 | Signature salah ditolak; update gagal dapat dipulihkan |
| E01 | P1 | Skenario dokumen lokal sampai reboot lulus ujung ke ujung |
| H01 | P2 | PC referensi lulus boot, input, storage dan recovery |
| H02 | P2 | GPU terpilih lulus correctness, stress dan reset |
| H03 | P2 | ARM64 virtual lulus boot, isolasi dan inference CPU |

## 20 Strategi pengujian dan keamanan

Unit test dijalankan untuk parser, allocator, schema dan algoritma yang dapat diuji di host. Kelulusan native tetap memakai guest test. Integration test memakai serial log berstruktur dan exit marker yang membedakan timeout, panic dan assertion failure. Build ID di setiap log mengikat hasil ke image.

Fault injection meliputi boot image hilang, memory map tak terduga, image malformed, disk penuh, short write, service crash, IPC peer death, network putus, task dibatalkan, serta reset saat update. Uji power loss virtual harus mencatat keterbatasan cache host dan dilengkapi uji target fisik sebelum klaim durability hardware.

Security testing meliputi isolasi proses, capability escalation, prompt injection, plugin palsu, path traversal, symlink race, kebocoran antarworkspace, parser model, resource exhaustion dan secret disclosure. Fuzzing diprioritaskan pada parser ELF, BootInfo, IPC, filesystem dan model metadata. Temuan isolasi atau kebocoran secret yang diketahui menghalangi rilis publik sampai ditangani.

Audit menyimpan task, identitas, alat, sumber, scope, persetujuan, waktu, status dan artefak. Isi sensitif direduksi; retensi dan kuota log ditentukan. Audit biasa tidak diklaim tahan manipulasi terhadap administrator atau kernel yang sudah dikuasai. Telemetri eksternal bersifat opt-in dan tidak diperlukan untuk boot/offline.

## 21 Roadmap dengan gerbang kelulusan

| Milestone | Deliverable | Gerbang |
| --- | --- | --- |
| M0 Kontrak platform | ADR, target manifest, toolchain, image builder | Build bersih dan dependency terkunci |
| M1 Boot | Loader, BootInfo, kernel entry, log | B01 dan B02 |
| M2 Proteksi | Paging, exceptions, timer, user mode | K01, K02, K03 |
| M3 Bootstrap | Boot image, root task, IPC, capability | B03, K04 sampai K06 |
| M4 OS dasar | ELF, shell, VirtIO block, filesystem baca | B04, S01, S02, D01 |
| M5 Runtime CPU | Library minimum, compute, model parser | C01 dan stress resource |
| M6 Native AI | Tokenizer, model fixture, inference | A01 dan A02 |
| M7 Persistensi | Filesystem tulis, task store, update/recovery dasar | D02, W01 dan recovery tests |
| M8 Workspace | SpaceLink, editor, broker, Task Center, desktop | L01 sampai L03, T01, G01, U01, E01 |
| M9 Developer Preview | Adapter, SDK, paket dan uji gabungan | T02, G02, U02, I01, P01 |
| M10 Hardware | Installer dan PC referensi | H01 |
| M11 Ekspansi | GPU compute dan port ARM64 terpisah | H02 dan H03 masing-masing |

M7 dapat diteliti sebelum M6 selesai, tetapi data persisten wajib siap sebelum workspace dinyatakan lulus. Eksperimen UI di host dan audit engine dapat berjalan lebih awal tanpa mengubah gerbang native. GPU dan ARM64 mempunyai backlog terpisah agar kegagalan salah satunya tidak menahan rilis CPU x86 yang sudah lulus.

Tidak ada tanggal rilis yang dipatok sebelum kapasitas tim diketahui. Setiap milestone menyerahkan source, instruksi build, image, manifest, log, laporan test, dan daftar keterbatasan. Kriteria selesai adalah bukti yang dapat dijalankan ulang, bukan persentase kode.

## 22 Backlog awal dan pembagian tanggung jawab

Urutan awal: tetapkan ADR platform dan kernel; pilih loader; bekukan BootInfo; siapkan image builder dan serial test; buktikan exception; implementasikan allocator dan user mode; lanjutkan preemption, IPC/capability, boot image, ELF dan shell; baru tambahkan storage serta engine CPU.

| Peran | Tanggung jawab | Hasil review |
| --- | --- | --- |
| Pemilik produk | Scope, prioritas, mode otonomi, pengalaman pengguna | Persetujuan milestone produk |
| Lead kernel | Boot, ABI, memori, scheduler, IPC | ADR dan bukti isolasi |
| Lead platform | Driver, filesystem, networking, recovery | Matriks perangkat dan konsistensi data |
| Lead AI | Engine, model, compute, adapter | Correctness dan resource benchmark |
| Lead aplikasi | Workspace, SpaceLink, shell, SDK | Uji alur pengguna dan kontrak aksi |
| QA dan keamanan | CI, fault injection, threat model | Bukti rilis dan daftar blocker |

Satu orang dapat memegang beberapa peran. Dokumen tidak mengasumsikan tim tersedia. Estimasi dilakukan per milestone setelah spike teknis; pekerjaan driver dan port runtime memerlukan contingency yang dinyatakan.

## 23 Risiko dan keputusan terbuka

| Risiko | Dampak | Respons |
| --- | --- | --- |
| Cakupan kernel dan aplikasi terlalu luas | Tidak mencapai OS usable | Bekukan P0, satu target dan satu model |
| IPC/capability belum matang | Bypass izin dan deadlock | Kontrak eksplisit, negative tests, kuota |
| Port engine memerlukan fasilitas besar | Inference tertunda | Audit dependency sebelum M2 selesai |
| GPU tanpa dukungan driver | Klaim percepatan tidak tercapai | CPU dahulu, studi perangkat terpilih |
| Persistensi lemah | Dokumen atau state tugas hilang | Gerbang D02 sebelum workspace |
| Model kecil kurang mampu | Demo produk gagal meski inference lulus | Pisahkan fixture teknis dan kualitas tugas |
| Adapter berubah | Integrasi putus | Versi, contract tests, label experimental |
| Pemakaian ulang kode | Konflik dependency/lisensi | Review komponen sebelum diadopsi |
| Task mengulang efek luar | Duplikasi tindakan | Idempotency dan reconciliation |

Keputusan yang wajib ditutup sebelum tahap terkait: pilihan bootloader; subset ELF; model revokasi capability; filesystem tulis; library port; engine dan model; crash policy layanan kritis; strategi kunci update; PC referensi; repository Hermes yang dimaksud; repository dan skema SpaceLink; serta toolkit grafis.

Usulan arsitektur bukan klaim seluruh keputusan teknis sudah final. ADR mencatat opsi, alasan, tradeoff, dampak migrasi dan bukti eksperimen. Perubahan yang memengaruhi batas native atau gerbang rilis harus direview pemilik produk.

## 24 Dokumentasi implementasi yang diturunkan

PRD ini menjadi acuan kebutuhan produk. Implementasi menurunkan Boot and Kernel Specification, ABI Reference, Service API Reference, Threat Model, Driver Support Matrix, Recovery Guide, dan Test Plan. Dokumen tersebut memiliki versi dan merujuk requirement ID yang sama.

Kernel specification memuat memory layout, state entry, syscall numbering, error semantics, interrupt routing dan aturan concurrency. SDK specification memuat schema serta contoh aplikasi. Release manifest memuat image hash, compatibility, known limitations dan cara pemulihan. Detail tersebut tidak boleh diganti dengan pernyataan umum bahwa sistem sudah AI-native.

Perubahan versi 0.2 dibanding konsep awal: memperjelas bootstrap melalui boot image, status boot, ABI dasar, layanan persisten, kontrak aksi, workspace terpadu, state tugas, storage/recovery, pemisahan fixture inference dan kualitas produk, serta requirement yang dapat dilacak.

## 25 Referensi dan batas penggunaan

Referensi eksternal mendukung istilah dan kontrak teknis. Desain Space OS, nama layanan, milestone dan target penerimaan dalam dokumen ini merupakan usulan proyek. Pemeriksaan referensi dilakukan pada 6 Oktober 2026; versi spesifikasi implementasi tetap harus dipatok dalam ADR.

1. PRD Space OS versi 0.1 tanggal 15 September 2026, salinan Markdown yang diberikan pemilik produk. Menjadi sumber visi, batas native, SpaceLink dan target awal.
2. Odysseus repository, https://github.com/odysseus-dev/odysseus. README menjadi referensi konsep self-hosted workspace dan daftar fitur. Bukan bukti dukungan ABI Space OS; tidak dilakukan audit menyeluruh source code dalam penyusunan PRD ini.
3. Odysseus AI independent guide, https://odysseusai.dev/. Panduan independen yang ditunjukkan pemilik produk; bukan otoritas implementasi kernel atau situs resmi maintainer.
4. UEFI Forum Boot Services, https://uefi.org/specs/UEFI/2.9_A/07_Services_Boot_Services.html. Referensi GetMemoryMap dan ExitBootServices. Versi 2.9A dipakai sebagai halaman rujukan yang telah diperiksa, bukan ketentuan bahwa firmware harus versi tersebut.
5. OASIS Virtual I/O Device Version 1.2 Committee Specification 01, https://docs.oasis-open.org/virtio/virtio/v1.2/virtio-v1.2.html. Referensi perangkat virtual, negosiasi fitur dan virtqueue; subset implementasi ditetapkan dalam ADR.
6. Intel Software Developer Manuals, https://www.intel.com/content/www/us/en/developer/articles/technical/intel-sdm.html. Referensi system programming x86; implementasi Intel/AMD harus memeriksa dokumentasi arsitektur dan fitur platform yang sesuai.

## Lampiran A Istilah operasional

ABI adalah kontrak biner antara program dan platform. IPC adalah komunikasi antarproses. Capability adalah referensi objek dengan hak tertentu yang ditegakkan sistem. Boot image adalah kumpulan program awal yang dimuat ke RAM. Native menunjukkan target eksekusi, bukan asal semua source code. Provenance adalah hubungan hasil dengan sumbernya. Degraded mode mempertahankan fungsi yang masih aman saat sebagian layanan gagal. ADR adalah catatan keputusan arsitektur. Trusted computing base adalah komponen yang harus dipercaya agar jaminan keamanan tertentu berlaku.

## Lampiran B Definisi selesai untuk satu fitur

Sebuah fitur selesai ketika requirement ID mempunyai implementasi pada target yang dinyatakan, test positif dan kegagalan yang relevan lulus, resource cleanup terbukti, dokumentasi kontrak tersedia, serta known limitations dicatat. Fitur yang hanya berjalan di host, mock, atau compatibility tidak diberi label native. Rilis memperoleh persetujuan setelah seluruh requirement wajib pada tahapnya memiliki bukti dan tidak ada blocker isolasi, kehilangan data, atau secret disclosure yang diketahui.
