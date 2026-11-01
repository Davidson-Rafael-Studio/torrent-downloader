# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

A single-binary Rust BitTorrent client (`torrent-downloader`) for one user on their own machine. It downloads game `.torrent` files straight into the Steam `steamapps/common` folder, with a ratatui TUI (default) and a `--headless` progress mode.

## Commands

```bash
cargo build                      # debug build
cargo run                        # interactive TUI
cargo run -- --demo              # TUI with two sample games served by the built-in local swarm
cargo run -- --headless <file.torrent|dir> [--steam-dir DIR] [--threads N]
cargo test                       # all tests (unit + end-to-end against the local swarm)
cargo test <name_substring>      # single test, e.g. cargo test test_rescan
cargo test job::peer_pool        # one module's tests
cargo clippy --all-targets       # lint gate: keep it at zero warnings, no #[allow] to silence
```

- Tests start a real tracker + seeder on `127.0.0.1` (`src/seeder.rs`) and write to `std::env::temp_dir()`; no network access is needed.
- `--demo` and the TUI `g` key write `.torrent` files to `./sample_torrents` in the current directory (git-ignored). Run demos from a scratch directory when you don't want that.
- The TUI calls `tokio::task::block_in_place`, so it must run on the multi-thread runtime (`#[tokio::main]`); don't drive it from a current-thread test runtime.

## Architecture

Dependencies point one way: `tui` → `engine` → `worker` → `job` → `storage` / `tracker` / `peer` / `torrent`. `job` never depends on `engine`, `worker`, or `tui`.

**`engine.rs`: `DownloadManager`.** It owns the queue (`jobs: RwLock<Vec<Arc<TorrentJob>>>`) and the Steam target dir. `add_torrent_file` parses the torrent, verifies existing data off-thread (`spawn_blocking` → `storage::load_or_verify_resume_state`), builds a `TorrentJob`, then spawns that job's tasks: speed meter, resume saver, periodic announce (all in `job/tasks.rs`) and the worker pool (`worker.rs`). Re-adding a torrent that is already queued returns the existing job **unchanged**, so a paused job is never resumed by a re-add. `DownloadManager::new` also spawns the cross-job thread tuner, which holds only a `Weak` and stops when the manager is dropped.

**`job/`: one torrent's state; most invariants live here.** Fields are private. Other modules use `pub(crate)` methods only.
- Status: `is_paused` changes only while the `status` write lock is held. Background code changes status only via `set_status_unless_paused`, which never overrides a user pause or `Completed`. `Completed` is terminal.
- Pieces (`job/pieces.rs`): unclaimed pieces sit in a `VecDeque` (`pending_pieces`). Workers claim with `claim_pending_piece_where(peer_has_piece)`, and on any failure `release_piece`. `store_verified_piece` writes off-thread, and the worker whose increment reaches the total calls `mark_completed` exactly once.
- Resume state: workers only set `is_resume_state_dirty`. `run_resume_state_saver` is the **only** writer of `.torrent_resume.json` (at most once per second), so stale snapshots can't overwrite newer ones. `completed_pieces_count` uses Release on increment and Acquire in `is_complete`, so whoever sees the final count also sees the dirty flag. `storage::write_file_atomically` writes a unique temp file and then renames it; there is deliberately no fsync.
- Peers: `job/peer_pool.rs` is pure logic. Announces **merge** peers and keep each peer's failure history. Failed peers back off 5 s → 10 s → … up to 5 min. A peer that merely lacks a piece (`download_piece_from_peer` returns `Ok(None)`) is not penalized.
- Announces: the first announce sends `event=started`. `run_periodic_announce` re-announces on the shortest tracker interval, clamped to 2–30 min.

**`worker.rs`.** Spawns `MAX_WORKER_THREADS` (16) tasks once per job. Workers whose index is ≥ `target_threads` idle, so the thread count changes live without spawning or killing tasks. Only worker 0 re-announces when the pool is empty.

**`thread_tuner.rs`.** Pure hill-climbing on the **total** speed of all downloads: it probes ±1 worker per 3 s tick and keeps the change only if total speed rises ≥5% (add) or drops ≤5% (remove). Jobs in manual mode (`+`/`-` in the TUI) are not tuned but still count toward the total. It is tested by simulation, with no I/O.

**`tui/`.**
- `mod.rs`: event loop, and a rescan of watched dirs every 3 s.
- `app.rs`: `TuiApp` state and key actions. `known_torrent_paths` makes rescans skip files already handed to the manager, so paused or removed jobs stay that way.
- `draw.rs`: pure rendering from `TorrentJobSnapshot`s. `snapshot(piece_preview_len)` copies only the piece preview it renders.

**`peer.rs`: request pipeline.** A `PeerSession` keeps `request_queue_limit` block requests outstanding across several `PieceInProgress` at once. The limit is about 2 s of data at the measured rate, clamped to 8–64; the 64 cap stays below the per-peer request queues other clients enforce. The worker tops up pieces while `wants_more_pieces` (requests in flight + unrequested blocks < limit) and only claims pieces the peer has (`claim_pending_piece_where`). `download_next_piece` returns one verified piece and leaves the rest in flight. On a choke, requested blocks go back to `Missing` and the session waits (30 s read timeout while choked, 6 s while downloading). On any error, the worker releases all of its active pieces. Each worker reports its connection stats into `connected_peers`, which the TUI Peers panel renders.

**Protocol scope.** Trackers are HTTP/UDP only. There is no DHT, no PEX, no incoming connections, and no uploading.

**Known gap.** The queue is not persisted. After a restart, only CLI inputs, `--src-dir`, and watched dirs are re-added. Data on disk resumes from `.torrent_resume.json` once the torrent is added again.

## Project skills (`.claude/skills/`)

Apply these when changing code here:
- **`do-not-overengineering`:** size solutions for a single-user local tool. No daemons, web UIs, databases, workspaces, or single-implementation traits. Extend the owning module first. Never trade away SHA-1 verification, path safety, or resume-file compatibility (`#[serde(default)]` for new manifest fields).
- **`how-to-make-it-more-efficient`:** KISS/DRY, self-describing names with units (`speed_bps`, `timeout_ms`), comments only for *why*, and no dead code (delete replaced code rather than adding `#[allow(dead_code)]`). Watch hot paths: the peer loop, piece claiming, hashing/disk, and the 80 ms TUI frame.
- **`do-not-make-godfile`:** one responsibility per file. Re-check any file over ~400 production lines and split along a seam over ~600. Split by moving code unchanged, one seam at a time, with tests after each step. `job/mod.rs` (~476 lines) is the current watch item.
