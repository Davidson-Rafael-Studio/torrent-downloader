# Steam Torrent Game Downloader (`torrent-downloader`)

A multi-threaded Rust BitTorrent downloader with an interactive **Ratatui Terminal UI**, designed to download game `.torrent` files directly into your Steam library (`steamapps/common`) with full support for **multi-download**, **multi-threading**, **pause/resume**, and **drag-and-drop**.

## Features

- **Drag & Drop `.torrent` Files or Source Folders**:
  - Drag and drop any `.torrent` file or folder containing `.torrent` files directly from Finder / Explorer into the terminal window.
  - Or press `a` inside the TUI to paste/type a `.torrent` file path or source directory path.
  - Watched source directories are re-scanned every 3 seconds, and only **newly added** `.torrent` files are picked up. A torrent you paused stays paused, and a torrent you removed with `x` is not re-added (add it again manually with `a` or drag & drop if you want it back).
- **Multi-Threaded Piece Downloader**:
  - Each worker holds one peer connection and keeps several pieces in flight on it, so the connection never idles between pieces. Every piece is SHA-1 verified before it is written.
  - **Adaptive request pipeline:** each connection keeps enough 16 KiB block requests outstanding to cover ~2 seconds at its measured speed (8 to 64 requests). On a 100 ms link this lifts one connection from ~1.3 MB/s to ~10 MB/s, which matters most when only a few peers are available.
  - A peer that chokes us keeps its connection: outstanding requests are re-sent after it unchokes (up to 30 s of waiting), instead of disconnecting and backing off.
  - **Adaptive workers (default):** every 3 seconds the app adds one worker to a torrent and keeps it only if the **total** speed of all downloads rises by at least 5%; otherwise it undoes the change. It also tries removing workers and keeps the removal when the total speed drops by no more than 5%. The worker count therefore follows your real connection speed, stops growing once the line is saturated, and never grows one torrent by taking bandwidth from another. Failed probes are retried after ~30 seconds, so it keeps up if your connection gets faster or slower.
  - **Manual override:** `+` / `-` set workers for the selected torrent by hand (`1` to `16`) and switch it to manual mode; `t` toggles it back to auto. The Threads column shows `active/target auto|manual`.
  - Visual real-time **Piece Map** (`█` Verified, `▒` Active Worker, `░` Remaining).
- **Pause & Persistent Resume**:
  - Press `Space` or `p` to pause/resume the selected torrent, or `P` / `R` to pause/resume all torrents.
  - A pause is never overridden by background work: tracker announces and finished pieces do not flip a paused torrent back to downloading.
  - Progress is saved to `.torrent_resume.json` inside the game folder at most once per second by a single writer per torrent, using write-to-temp-file + rename, so the file is never left half-written if the app crashes.
  - On restart, the resume file is loaded; if it is missing or invalid, pieces already on disk are re-verified by SHA-1. Quitting may lose up to the last second of progress, which only means a few pieces are downloaded again.
- **Finding More Peers**:
  - HTTP and UDP trackers are supported, and every tracker in the `.torrent` file is used.
  - Each announce asks for up to 200 peers (trackers default to about 50).
  - Trackers are re-announced on the interval they request (clamped to 2–30 minutes, default 5), and new peers are **merged** into the list, so peers that left the swarm are replaced by ones that joined later.
  - Peers that fail (connection timeout, choking, bad data) are skipped for 5 s, then 10 s, 20 s, ... up to 5 minutes, so workers stop wasting time on unreachable peers. A peer that simply lacks a piece is not penalized.
  - The Peers column shows `available/known`: peers currently worth trying out of all peers found.
  - The **Peers panel** in the details area lists every open connection, fastest first, with its speed (or `CHOKED`) and current request depth (`q64`). It tells you whether a slow download is caused by choking peers, slow peers, or too few connections.
  - If no peers are found the torrent shows `NO PEERS (PRESS r)`. Pressing `r` shows `TRACKER...` while re-announcing, then switches to `DOWNLOADING` once peers are found, or back to `NO PEERS` if none are.
- **Multi-Download Queue**:
  - Download multiple game torrents simultaneously with independent worker pools and progress tracking.
  - Adding a torrent that is already in the queue returns the existing download unchanged.
- **Steam Directory Integration**:
  - Automatically detects your platform's Steam `steamapps/common` path:
    - **macOS**: `~/Library/Application Support/Steam/steamapps/common`
    - **Linux**: `~/.steam/steam/steamapps/common`
    - **Windows**: `C:\Program Files (x86)\Steam\steamapps\common`
  - Press `d` in the TUI (or pass `--steam-dir`) to customize the target Steam library folder.
  - Detected game executables are shown in the details panel once a download completes.

## Quick Start

### 1. Launch Interactive TUI
```bash
cargo run
```
Inside the TUI:
- **Drag & drop** a `.torrent` file or directory directly into the terminal window
- Or press **`a`** to enter a `.torrent` file or folder path
- Or press **`g`** to generate & download a sample Steam game `.torrent` using the built-in local BitTorrent swarm

### 2. Launch with Demo Game Torrents
```bash
cargo run -- --demo
```
Demo `.torrent` files are written to `./sample_torrents` in the current directory, which is also watched.

### 3. Pass `.torrent` Files or a Source Directory via CLI
```bash
# Single .torrent file
cargo run -- "/path/to/game.torrent"

# Source directory containing multiple .torrent files
cargo run -- --src-dir "/path/to/torrents_folder" --threads 8
```

### 4. Headless Mode (no TUI)
```bash
cargo run -- --headless "/path/to/game.torrent"
```
Prints progress lines and exits when every download is completed or has failed.

## CLI Options

| Option | Description |
|---|---|
| `TORRENT_FILE_OR_DIR...` | `.torrent` files or directories containing them |
| `-s, --src-dir <DIR>` | Source directory to import and watch for new `.torrent` files |
| `-d, --steam-dir <DIR>` | Target Steam `steamapps/common` directory (auto-detected by default) |
| `-t, --threads <N>` | Starting workers per torrent, clamped to `1..=16` (default `4`); adjusted automatically afterwards |
| `--demo` | Generate and seed sample game torrents on startup |
| `--headless` | Print progress instead of running the TUI |

## TUI Keybindings

| Key | Action |
|---|---|
| `a` / `i` / drag & drop | Add a `.torrent` file or folder |
| `↑` `↓` / `k` `j` | Select torrent |
| `Space` / `p` | Pause / resume selected torrent |
| `r` | Resume selected torrent (re-announces to trackers if it has no peers) |
| `P` / `R` | Pause all / resume all |
| `+` / `-` | More / fewer workers for the selected torrent (switches it to manual) |
| `t` | Toggle automatic worker tuning for the selected torrent |
| `g` | Generate and download a demo game |
| `d` | Change Steam destination directory |
| `x` / `Delete` | Remove selected torrent from the queue (downloaded files are kept) |
| `q` / `Esc` / `Ctrl+C` | Quit |

## Development

```bash
cargo build
cargo test                 # unit tests + end-to-end tests against the built-in local swarm
cargo clippy --all-targets
```

| Module | Responsibility |
|---|---|
| `src/main.rs` | CLI flags, startup, headless mode |
| `src/tui/mod.rs` | Terminal setup and the TUI event loop |
| `src/tui/app.rs` | TUI state, keybindings, and user actions |
| `src/tui/draw.rs` | Rendering the TUI from state |
| `src/engine.rs` | Download queue (add/remove/pause all) and adaptive thread tuning across jobs |
| `src/job/mod.rs` | One torrent job's state: pause/resume, status, peers, announces |
| `src/job/pieces.rs` | Piece claiming, release, completion, and disk writes |
| `src/job/tasks.rs` | A job's background loops: speed meter, resume-state saver, periodic re-announce |
| `src/job/peer_pool.rs` | Known peers with failure backoff (pure logic) |
| `src/worker.rs` | Piece-download workers for a job |
| `src/thread_tuner.rs` | Adaptive worker-count decisions (pure logic, simulated in tests) |
| `src/storage.rs` | Writing pieces to disk, resume manifest, on-disk verification |
| `src/torrent.rs` | `.torrent` parsing and info hash |
| `src/tracker.rs` | HTTP/UDP tracker announces |
| `src/peer.rs` | Peer wire protocol and the adaptive multi-piece request pipeline |
| `src/steam.rs` | Steam folder detection and game folder naming |
| `src/path_utils.rs` | Input path normalization and `.torrent` discovery |
| `src/seeder.rs` | Local tracker + seeder used by `--demo`, `g`, and tests |
