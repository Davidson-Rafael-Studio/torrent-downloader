---
name: how-to-make-it-more-efficient
description: Use whenever writing, editing, reviewing, or refactoring code in this torrent-downloader Rust project as a pair programmer. Enforces KISS, DRY, clear self-describing names, sparse purposeful comments, efficient algorithms and data structures (performance, memory, latency), and zero dead code. Trigger also on Indonesian phrasing like "bikin fungsi", "optimasi", "lebih cepat", "lebih efisien", "review kode", "rapikan kode", "refactor".
---

# How To Make It More Efficient (torrent-downloader)

Cara berpikir sebagai pair programmer di project ini: **kode sederhana (KISS), tidak berulang (DRY), nama yang menjelaskan dirinya sendiri, efisien secara terukur, dan tanpa kode mati.**

Skill ini melengkapi `do-not-overengineering`: skill itu memutuskan *seberapa besar* solusinya, skill ini memutuskan *bagaimana kodenya ditulis*.

## 1. KISS — Keep It Simple

- Pilih alur paling lurus yang benar. Early return lebih baik daripada `if` bersarang.
- Satu fungsi, satu tugas yang bisa dijelaskan dalam satu kalimat tanpa kata "dan".
- Gunakan fitur bahasa/std yang sudah ada (`Iterator`, `Option`/`Result` combinator, `?`) daripada loop dan flag manual — selama tetap mudah dibaca.
- Jangan generic, trait, macro, atau lifetime rumit kalau tipe konkret cukup.

## 2. DRY — Don't Repeat Yourself

- Logika yang sama muncul **dua kali atau lebih** dengan alasan berubah yang sama → satukan ke satu fungsi/method di modul pemiliknya.
  Contoh di `engine.rs`: transisi ke `Completed` dan reset piece ke `Pending` masing-masing hanya lewat `TorrentJob::mark_completed` dan `TorrentJob::release_piece` — pakai method itu, jangan menulis ulang logikanya.
- Konstanta yang dipakai berulang (ukuran blok 16 KiB, nama file `.torrent_resume.json`) → satu `const` bernama.
- Jangan DRY berlebihan: dua potong kode yang *kebetulan mirip* tapi punya alasan berubah berbeda biarkan terpisah. Abstraksi yang salah lebih mahal daripada duplikasi kecil.

## 3. Penamaan yang Menjelaskan Kegunaan

Nama harus membuat komentar tidak diperlukan.

- **Fungsi:** kata kerja + objek yang menjelaskan hasilnya: `write_piece_to_disk`, `discover_peers`, `claim_next_pending_piece`. Hindari `handle`, `process`, `do_stuff`, `helper`, `manager2`.
- **Boolean:** `is_`, `has_`, `should_`, `can_`: `is_paused`, `has_peers`.
- **Satuan di nama** untuk angka yang punya unit: `speed_bps`, `piece_len_bytes`, `timeout_ms`, `elapsed_secs`.
- **Koleksi:** jamak dan jelas isinya: `peer_addrs`, `completed_pieces`, bukan `list`, `data`, `arr`.
- **Tidak menyingkat** kecuali istilah domain standar (`info_hash`, `peer_id`, `bps`, `tcp`). `cfg`, `res`, `tmp`, `mgr` → tulis lengkap.
- Ikuti konvensi Rust: `snake_case` fungsi/variabel, `PascalCase` tipe, `SCREAMING_SNAKE_CASE` konstanta.
- Nama lama yang menyesatkan pada kode yang sedang diubah → perbaiki sekalian (dalam lingkup tugas).

## 4. Komentar: Sedikit, Tepat, Bermakna

Tulis komentar **hanya** jika kode tidak bisa menjelaskan sendiri **kenapa**:

- Alasan keputusan yang tidak terlihat (kenapa resume state ditulis ke file sementara lalu di-`rename`).
- Invarian, asumsi, atau batas keamanan (path dari `.torrent` tidak tepercaya).
- Format protokol/biner. Contoh bagus yang sudah ada di `peer.rs`: `// Piece: <index:4><begin:4><block...>`
- Workaround bug eksternal, dengan alasannya.
- `///` doc comment untuk fungsi `pub` yang perilakunya tidak jelas dari nama dan tipe.

Dilarang:

- Komentar dekoratif / banner: `// ------ helpers ------`, `// ==== SECTION ====`.
- Komentar yang mengulang kode: `// increment counter` di atas `count += 1`.
- Kode yang di-comment-out. Hapus; riwayat ada di version control.
- `TODO` tanpa alasan dan konteks yang jelas.

## 5. Efisiensi: Performa, Memori, Latensi

Pikirkan biaya **di jalur panas (hot path)** dulu. Di project ini jalur panasnya:

- Loop pesan peer dan request blok (`peer.rs`).
- Klaim piece dan update status oleh banyak worker (`engine.rs`).
- Hashing SHA-1 dan tulis disk (`storage.rs`).
- Render TUI tiap ~80 ms (`tui.rs` → `TorrentJob::snapshot`).

Aturan:

- **Kompleksitas dulu, mikro-optimasi belakangan.** Turunkan O(n²) ke O(n) / O(log n) sebelum memikirkan hal lain.
- **Hindari alokasi dan `clone()` di loop panas.** Pakai `&[u8]`/`&str`, `Vec::with_capacity`, reuse buffer.
- **Kerja sebanding kebutuhan.** Jangan menyalin/menghitung data yang tidak dipakai.
  Contoh: `snapshot(piece_preview_len)` hanya menyalin piece yang ditampilkan TUI, dan daftar executable game di-cache, bukan dibaca dari disk tiap frame.
- **Jaga lock tetap singkat.** Jangan pegang `RwLock` sambil `await` I/O; ambil data, lepas lock, baru kerja.
- **I/O blocking dan CPU berat di async** → `tokio::task::spawn_blocking` (seperti tulis disk sekarang). Jangan `std::fs` berat langsung di task async yang panas.
- **Atomics:** `Relaxed` untuk statistik (speed, counter tampilan); `SeqCst`/`Acquire`-`Release` hanya bila urutan memengaruhi kebenaran.
- **I/O berulang:** perhatikan biaya per kejadian. Contoh: resume manifest disimpan oleh satu penulis per job (`spawn_resume_state_saver`) maksimal sekali per detik, bukan setiap piece — jangan tambahkan penulisan manifest langsung dari worker.
- **Ukur sebelum klaim lebih cepat.** Untuk optimasi non-trivial, sebutkan kompleksitas sebelum/sesudah atau jalankan pembanding sederhana (`std::time::Instant` di test/demo).

### Pilih Struktur Data yang Cocok

| Kebutuhan | Struktur | Catatan |
|---|---|---|
| Akses per indeks piece | `Vec<T>` | O(1), cache-friendly; sudah dipakai untuk `piece_states` |
| Antrian piece yang belum diklaim | `VecDeque<usize>` | Klaim O(1); dipakai `pending_pieces` di `engine.rs` |
| Cari/cek keberadaan berdasarkan key | `HashMap` / `HashSet` | O(1) rata-rata; contoh dedup peer `SocketAddr` |
| Butuh urutan terurut / range | `BTreeMap` / `BTreeSet` | O(log n) |
| Ambil prioritas tertinggi (misal rarest-first) | `BinaryHeap` | O(log n) push/pop |
| Bitfield piece (protokol, jutaan piece) | bit per piece dalam `Vec<u8>` | Memori 8× lebih kecil dari `Vec<bool>`; pakai hanya bila ukuran penting |
| Koleksi kecil (< ~16 item) | `Vec` + scan linear | Sering lebih cepat dari `HashMap`; jangan dipaksa |
| Buffer byte jaringan | `Vec<u8>` reuse / `&[u8]` | Hindari alokasi per pesan |

### Kapan Readability Boleh Menang

Korbankan sedikit efisiensi demi keterbacaan jika:

- Kode **bukan** jalur panas dan dijalankan jarang (parsing CLI, scan folder saat startup, setup).
- Data dijamin kecil.
- Versi "cepat" butuh `unsafe`, trik bit, atau logika yang sulit diverifikasi tanpa keuntungan terukur.

Jika memilih readability di jalur panas, tulis satu baris alasan singkat.

## 6. Nol Kode Mati

Kode yang tidak dipanggil adalah kompleksitas tanpa manfaat.

- **Jangan menulis** fungsi, method, field, enum variant, parameter, atau `pub` yang belum dipakai "untuk nanti".
- **Hapus** kode yang tergantikan oleh perubahan Anda di commit yang sama — fungsi lama, import, field, cabang `match` yang tidak mungkin terjadi.
- **Jangan membungkam** peringatan dengan `#[allow(dead_code)]` atau prefix `_` untuk menyembunyikan kode tak terpakai. Hapus kodenya. (Prefix `_` sah hanya untuk nilai yang sengaja diabaikan dari API pihak lain.)
- **Jangan buat wrapper satu baris** yang hanya meneruskan ke fungsi lain tanpa menambah makna.
- Fungsi khusus test tinggal di `#[cfg(test)] mod tests`, bukan di kode produksi.
- Setelah perubahan, cek: `cargo build` dan `cargo clippy --all-targets` tidak boleh punya peringatan `dead_code` / `unused` baru.

## 7. Checklist Sebelum Menyerahkan Kode

1. Apakah ada cara yang lebih sederhana dengan hasil sama? (KISS)
2. Apakah ada logika yang sekarang terduplikasi? (DRY)
3. Apakah setiap nama menjelaskan kegunaannya tanpa perlu komentar?
4. Apakah setiap komentar menjelaskan *kenapa*, bukan *apa*? Tidak ada banner atau kode yang di-comment-out?
5. Apakah kompleksitas dan alokasi di jalur panas sudah wajar? Struktur datanya cocok?
6. Apakah ada fungsi/field/import yang tidak terpakai atau tergantikan? Hapus.
7. `cargo build`, `cargo test`, `cargo clippy --all-targets` bersih dari peringatan baru.

## Format Jawaban Saat Menulis/Review Kode

- Tunjukkan kodenya, lalu ringkas: **apa yang diubah**, **kompleksitas/biaya penting** (jika relevan), dan **kode apa yang dihapus**.
- Jika menukar efisiensi dengan readability, sebutkan tradeoff-nya dalam satu kalimat.
- Saat review, urutkan temuan: kebenaran → kode mati → efisiensi jalur panas → DRY → penamaan → komentar.
