---
name: do-not-overengineering
description: Use for ANY feature, refactor, fix, or architecture/dependency decision in this torrent-downloader Rust project (BitTorrent client, ratatui TUI, tokio engine, resume state, Steam folder integration, trackers, peers, tests). Keeps changes proportional to the real goal of a single-user local desktop tool and steers away from over-engineered ideas (daemons, web UIs, databases, crate workspaces, single-impl traits, actor frameworks, mock frameworks, extra crates). Trigger also on Indonesian phrasing like "tambah fitur", "refactor", "arsitektur", "pakai database", "biar scalable", "terlalu ribet".
---

# Do Not Over-Engineer (torrent-downloader)

Tujuan: setiap perubahan pada project ini harus **efektif, kecil seperlunya, dan langsung bisa dipakai** — bukan disiapkan untuk skala atau kebutuhan imajiner.

## Konteks Project (pegang ini sebagai skala nyata)

- **Apa:** satu binary Rust (`torrent-downloader`) untuk mengunduh `.torrent` game langsung ke folder Steam `steamapps/common`.
- **Siapa:** satu user, di mesin lokalnya sendiri. Bukan server, bukan multi-tenant, bukan SaaS.
- **Ukuran:** satu crate, ~3.000 baris, modul flat di `src/`.
- **Stack yang sudah ada:** `tokio`, `ratatui` + `crossterm`, `clap`, `serde` + `serde_json` + `serde_bencode`, `sha1`, `reqwest` (blocking, rustls), `anyhow`, `rand`, `dirs`, `chrono`.
- **State runtime:** `Arc<DownloadManager>` berisi `RwLock<Vec<Arc<TorrentJob>>>`; tiap job memakai `RwLock`, atomics, dan `Notify` untuk pause/resume.
- **Persistensi:** `ResumeManifest` (JSON `.torrent_resume.json`) + verifikasi ulang SHA-1 piece di disk.
- **Mode:** TUI interaktif (default) dan `--headless`.
- **Test:** unit test di dalam modul + integration test memakai `LocalTestSwarm` (seeder lokal sungguhan di `seeder.rs`) dan temp dir.

## Peta Kepemilikan Modul — taruh perubahan di sini

| Kebutuhan | Modul pemilik |
|---|---|
| Parsing `.torrent`, bencode, info hash, contoh torrent | `torrent.rs` |
| Announce tracker HTTP/UDP, daftar peer | `tracker.rs` |
| Wire protocol peer (handshake, request/piece) | `peer.rs` |
| Penjadwalan piece, worker, pause/resume, multi-download | `engine.rs` |
| Tulis piece ke disk, verifikasi, resume state | `storage.rs` |
| Deteksi folder Steam, nama folder game, executable | `steam.rs` |
| Normalisasi path input, drag-and-drop, scan folder | `path_utils.rs` |
| Tampilan dan keybinding | `tui.rs` |
| Flag CLI, mode headless, wiring awal | `main.rs` |
| Swarm lokal untuk demo dan test | `seeder.rs` |

Perluas modul pemilik dulu. Buat modul baru hanya jika tanggung jawabnya benar-benar baru (misal DHT) dan tidak cocok di modul mana pun.

## Langkah Wajib Sebelum Mengubah Kode

1. **Tegaskan tujuan utama** dalam satu kalimat. Contoh: "User ingin membatasi kecepatan download total."
2. **Cek apakah sudah ada.** Cari di modul pemilik — banyak perilaku (pause, resume, scan folder, thread scaling) sudah ada.
3. **Pilih anak tangga terendah yang cukup** (lihat di bawah).
4. **Sebutkan kapan perlu naik level** — sinyal konkret, bukan "nanti kalau besar".

Jika user mengusulkan solusi berat, jelaskan singkat kenapa berlebihan untuk tool lokal satu user, lalu tawarkan alternatif. Jika user tetap memilihnya (misal untuk belajar), hormati keputusannya.

## Tangga Solusi untuk Project Ini

1. **Sudah ada / cukup konfigurasi** — flag CLI atau keybinding yang sudah tersedia.
2. **Ubah fungsi yang ada** di modul pemilik.
3. **Tambah fungsi/field kecil** di modul pemilik (misal field baru di `TorrentJob` atau `ResumeManifest`).
4. **Pakai std atau dependency yang sudah ada** (`tokio`, `serde_json`, `sha1`, `reqwest`, ...).
5. **Modul baru** di crate yang sama.
6. **Dependency baru** — hanya jika implementasi sendiri jelas lebih rumit/berisiko; jelaskan tradeoff-nya.
7. **Perubahan arsitektur** (proses terpisah, crate workspace, UI lain) — hanya atas permintaan eksplisit user.

## Contoh Alternatif Khusus Project Ini

| Over-engineered | Alternatif sederhana | Naik level jika... |
|---|---|---|
| SQLite / sled / Redis untuk resume state atau riwayat download | Tambah field di `ResumeManifest` dengan `#[serde(default)]` agar file lama tetap terbaca | Perlu query kompleks lintas ribuan torrent |
| Daemon + REST/gRPC API + web UI (React) / Electron / Tauri | Tetap TUI; untuk otomasi pakai `--headless` dan flag CLI | User eksplisit butuh kontrol jarak jauh dari perangkat lain |
| Pecah jadi Cargo workspace (`core`, `net`, `ui`, ...) | Satu crate; pecah file modul hanya jika satu file sudah sulit dibaca | Bagian kode perlu dipublikasikan sebagai library terpisah |
| `trait Storage` / `trait PeerSource` / `dyn Tracker` dengan satu implementasi | Fungsi konkret; sumber peer baru cukup ditambahkan di `discover_peers` | Ada dua implementasi nyata yang dipilih saat runtime |
| Actor framework (actix) atau message bus channel untuk semua state | Pola yang sudah ada: `Arc` + `RwLock` + atomics + `Notify` | Terbukti ada contention/deadlock yang tidak bisa diatasi pola sekarang |
| Thread pool sendiri / rayon untuk hashing | `tokio::task::spawn_blocking` (sudah dipakai untuk tulis disk) | Profiling menunjukkan hashing jadi bottleneck utama |
| Config berlapis (TOML + env + figment) | Flag `clap` dengan default yang wajar | User minta pengaturan tersimpan antar-run; cukup satu file JSON via `serde_json` + `dirs` |
| `mockall` / mock jaringan berbasis trait | `LocalTestSwarm` + temp dir, seperti test di `engine.rs` | Perlu mensimulasikan perilaku peer jahat/rusak yang tidak bisa dibuat seeder |
| `tracing` + OpenTelemetry + metrics exporter | `last_message` pada job (tampil di TUI) dan `println!`/`eprintln!` di headless | Perlu debug log ke file; cukup tulis ke satu file log |
| Rate limiter generik / crate token bucket | Satu budget `AtomicU64` di `DownloadManager` yang di-reset per detik | Butuh limit per-peer dan per-torrent sekaligus dengan prioritas |
| Sistem plugin untuk sumber torrent | Tambah cabang di `path_utils.rs` (misal dukungan magnet link) | — |
| Crate kecil untuk hal sepele (`thiserror`, `once_cell`, `parking_lot`, `itertools`) | `anyhow` (sudah ada), `std::sync::OnceLock`, `tokio::sync` | Kebutuhan nyata yang tidak dipenuhi std/dependency yang ada |

## Yang TIDAK Boleh Dikorbankan Demi Sederhana

- **Verifikasi SHA-1** setiap piece sebelum ditandai selesai.
- **Keamanan path:** path file dari `.torrent` adalah input tidak tepercaya — tolak `..`, path absolut, dan komponen aneh agar tidak menulis di luar folder game. Nama folder tetap lewat `sanitize_folder_name`.
- **Kompatibilitas resume:** file `.torrent_resume.json` lama harus tetap bisa dibaca (`#[serde(default)]`, jangan rename field tanpa migrasi).
- **Tidak kehilangan data download:** jangan hapus/timpa file game tanpa alasan; pause/cancel harus aman di tengah piece.
- **Kebenaran protokol** BitTorrent dan penanganan error jaringan (timeout, peer putus) — gunakan `anyhow::Context`, jangan `unwrap()` di jalur jaringan/disk.
- **Test yang relevan** untuk perilaku yang diubah.

## Verifikasi (bukti terkecil yang cukup)

- Selalu: `cargo build` dan `cargo test`.
- Bila menyentuh banyak kode: `cargo clippy`.
- Perubahan engine/storage/peer: tambah atau perluas test berbasis `LocalTestSwarm` di modul terkait.
- Perubahan parsing/path: unit test di `torrent.rs` / `path_utils.rs`.
- Perubahan TUI: jalankan `cargo run -- --demo` dan cek manual.

## Format Jawaban

Saat memberi rekomendasi atau rencana, ringkas:

1. **Tujuan utama:** satu kalimat.
2. **Modul yang diubah:** dari peta kepemilikan.
3. **Rekomendasi:** perubahan paling sederhana yang cukup, dengan alasan singkat.
4. **Kenapa tidak [opsi berat]:** satu-dua kalimat, hanya jika user mengusulkannya atau opsi itu umum dipertimbangkan.
5. **Kapan naik level:** sinyal konkret.
6. **Verifikasi:** perintah/test yang akan dijalankan.

Berikan satu rekomendasi utama; alternatif hanya jika benar-benar relevan.
