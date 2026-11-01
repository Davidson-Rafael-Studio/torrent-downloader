---
name: do-not-make-godfile
description: Use whenever adding code, creating files or modules, or refactoring in this torrent-downloader Rust project — especially when a change would grow an already large file (engine.rs, tui.rs) or introduce a Manager/Helper/Utils-style type. Prevents god files and god objects that violate the Single Responsibility Principle, and shows how to split by responsibility safely without over-splitting. Trigger also on "where should this go", "file too big", "split module", "refactor", "pisah file", "file terlalu besar".
---

# Do Not Make a God File

A **god file** (or god object) is a source file, struct, or module that knows too much, controls too many objects, and mixes unrelated logic — for example UI rendering, disk I/O, networking, logging, and business rules in one place. It violates the Single Responsibility Principle, is hard to test in isolation, raises cognitive load, causes merge conflicts, and makes every refactor risky.

Goal: every file and type has **one reason to change**. Code lands in the module that owns that responsibility, and a module is split along responsibility seams before it turns into a god file — but never split into fragments for the sake of line counts.

This skill complements `do-not-overengineering` (how big the solution is) and `how-to-make-it-more-efficient` (how the code is written). It decides **where code lives**.

## The One-Sentence Test

Before adding code to a file or type, describe its responsibility in one sentence **without "and"**:

- `storage.rs`: "Persists verified pieces and resume state on disk." → OK (one concern: disk persistence).
- `thread_tuner.rs`: "Decides worker-count changes from measured total speed." → OK.
- A file that "draws the UI **and** parses torrents **and** talks to trackers" → god file. Split.

If the new code makes the sentence need "and", it belongs somewhere else.

## Warning Signs

Treat any of these as a stop-and-check signal:

- **Size (production code, excluding `#[cfg(test)]`):**
  - More than ~400 lines: check whether the file still has one responsibility.
  - More than ~600 lines: split along a seam before (or as part of) adding more to it.
  - A function longer than ~50 lines, or nested more than 3 levels deep.
- **Names:** `Manager`, `Helper`, `Utils`, `Common`, `Misc`, `Core`, `Handler`, `Service`, `stuff.rs`, `helpers.rs`, `utils.rs` — names that describe nothing specific and attract unrelated code.
- **Imports from unrelated worlds in one file:** e.g. `ratatui` + `std::fs` + `tokio::net` + `sha1` together means UI, disk, network, and hashing are mixed.
- **Struct with disjoint field groups:** some methods touch only fields A–D, others only E–H. That is two types sharing one name.
- **Wide `pub` surface:** many `pub` items used by different, unrelated callers.
- **Change magnet:** unrelated features keep editing the same file, or two people editing it at once collide.
- **Hard to test:** testing one behavior needs a terminal, a network swarm, and a temp dir at once because nothing can be constructed in isolation.

## Where Code Belongs in This Project

| Responsibility | Owner |
|---|---|
| CLI flags, startup wiring, headless output | `main.rs` |
| Terminal setup and event loop | `tui/mod.rs` |
| TUI state, key handling, user actions | `tui/app.rs` |
| Drawing the TUI (pure rendering from state) | `tui/draw.rs` |
| Download queue, adding/removing jobs, cross-job thread tuning | `engine.rs` |
| One torrent job's state and the rules that keep it consistent (pause, status, peers, announces) | `job/mod.rs` |
| Piece claiming, release, completion, disk writes | `job/pieces.rs` |
| A job's background loops (speed meter, resume saver, periodic re-announce) | `job/tasks.rs` |
| Known peers and failure backoff (pure, no I/O) | `job/peer_pool.rs` |
| Piece-download workers of one job | `worker.rs` |
| Worker-count decisions (pure, no I/O) | `thread_tuner.rs` |
| Disk writes, resume manifest, on-disk verification | `storage.rs` |
| `.torrent` parsing, info hash | `torrent.rs` |
| Tracker announces | `tracker.rs` |
| Peer wire protocol and the adaptive request pipeline | `peer.rs` |
| Steam paths, game folder naming, executables | `steam.rs` |
| Input path normalization, `.torrent` discovery | `path_utils.rs` |
| Local tracker/seeder for demo and tests | `seeder.rs` |

Rules:

- **Extend the owner** when the new code shares its responsibility.
- **Create a new module** when the code is a new responsibility that fits no owner (as `thread_tuner.rs` did for adaptive tuning). Name it after what it owns, never after a vague role.
- **Never put decision logic inside I/O or UI code.** Keep it in a pure type/function that takes plain data and returns a decision (see `ThreadTuner::step`): it becomes trivially testable and reusable by both the TUI and headless mode.
- **UI delegates disk and network work.** `tui.rs` calls `DownloadManager`, `TorrentJob`, `steam.rs`, and `path_utils.rs` instead of reading torrents, writing pieces, or opening sockets itself.
- **`DownloadManager` owns the queue, not everything.** New concerns (e.g. bandwidth limits, history, scheduling) get their own type or module that the manager uses, instead of more fields and methods on the manager.

## Current Hotspots (as of 2026-11-01)

`engine.rs` and `tui.rs` were split on 2026-11-01 into the modules in the table above. Every file is now under ~410 production lines except:

| File | Production lines | Status | Natural seam if it must grow |
|---|---|---|---|
| `job/mod.rs` | ~476 | One responsibility, but over the ~400 check; background loops, peer pool and piece bookkeeping already moved out | Next seam: status/pause handling (`pause`, `resume`, `toggle_pause`, `set_status_unless_paused`) into `job/status.rs` |

Keep the boundaries: `tui/draw.rs` only renders, `tui/app.rs` never draws, `worker.rs` changes job state only through `TorrentJob` methods, and `job/` never depends on `engine.rs`, `worker.rs`, or the TUI.

## How to Split Safely

1. **Tests first.** Make sure `cargo test` passes and the behavior you are moving is covered. Add a test before moving if it is not.
2. **Move, don't rewrite.** Move code to the new module unchanged; fix only paths, `use` lines, and visibility. Behavior changes go in a separate step.
3. **One seam per step.** Split one responsibility at a time, and run `cargo build && cargo test && cargo clippy --all-targets` after each.
4. **Smallest visibility.** Items used only inside the new module stay private; use `pub(crate)` for crate-internal sharing instead of `pub`.
5. **No cycles.** Dependencies point one way (e.g. `tui → engine → storage`, never back). If two new modules need each other, the seam is wrong.
6. **Move tests with their code.** Each module keeps its own `#[cfg(test)] mod tests`.
7. **Delete what the move leaves behind** — old imports, re-exports nobody needs, empty helpers.

## Do Not Over-Split Either

A god file is one failure mode; a pile of fragments is the opposite one.

- Don't create a file per function or per tiny struct.
- Don't split by kind (`types.rs`, `traits.rs`, `constants.rs`, `errors.rs` holding everything of that kind). Split by **responsibility**, so related types, constants, and functions stay together.
- Don't add a trait or interface just to "decouple" a split; plain modules and concrete types are enough until a second implementation exists.
- Don't split a cohesive file only because it crossed a line count. If the one-sentence test passes and it is easy to navigate, leave it — but stop adding unrelated code to it.

## Checklist Before Finishing a Change

1. Does every file I touched still pass the one-sentence test?
2. Did I add a `Manager`/`Helper`/`Utils`-style name or grow an existing one with unrelated methods?
3. Did any file cross ~400 / ~600 production lines? If so, did I check or perform the split?
4. Is decision logic separated from UI, disk, and network code, and unit-testable on its own?
5. Do dependencies still point one way, with the smallest visibility needed?
6. `cargo build`, `cargo test`, `cargo clippy --all-targets` are clean.

## Answer Format

When proposing or reviewing where code goes:

- State the owning module (from the table) and the one-sentence responsibility.
- If a split is needed, list the seams, the order of moves, and the proof run after each.
- If you decided **not** to split, say why in one sentence.
