mod peer_pool;
mod pieces;
mod tasks;

use crate::peer::PeerConnectionStats;
use crate::storage::save_resume_state;
use crate::torrent::TorrentMeta;
use crate::tracker::{discover_peers, AnnounceEvent};
use anyhow::Result;
use peer_pool::PeerPool;
use std::cmp::Reverse;
use std::collections::{HashMap, VecDeque};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};
use tokio::sync::{Notify, RwLock};

pub const MAX_WORKER_THREADS: usize = 16;
const LISTEN_PORT: u16 = 6881;
const DEFAULT_REANNOUNCE_INTERVAL_SECS: u64 = 300;
/// Bounds on the tracker-requested interval: frequent enough to keep finding fresh peers,
/// never so frequent that trackers see it as abuse.
const MIN_REANNOUNCE_INTERVAL_SECS: u64 = 120;
const MAX_REANNOUNCE_INTERVAL_SECS: u64 = 1800;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DownloadStatus {
    ConnectingTrackers,
    Downloading,
    Paused,
    Completed,
    NoPeersFound,
    Error,
}

impl DownloadStatus {
    pub fn label(&self) -> &'static str {
        match self {
            DownloadStatus::ConnectingTrackers => "TRACKER...",
            DownloadStatus::Downloading => "DOWNLOADING",
            DownloadStatus::Paused => "PAUSED",
            DownloadStatus::Completed => "READY IN STEAM",
            DownloadStatus::NoPeersFound => "NO PEERS (PRESS r)",
            DownloadStatus::Error => "ERROR",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PieceStatus {
    Pending,
    InProgress,
    Completed,
}

pub struct TorrentJob {
    id: usize,
    pub meta: Arc<TorrentMeta>,
    game_install_dir: PathBuf,
    status: RwLock<DownloadStatus>,
    status_message: RwLock<String>,
    piece_states: RwLock<Vec<PieceStatus>>,
    pending_pieces: Mutex<VecDeque<usize>>,
    peer_pool: Mutex<PeerPool>,
    /// Live connection stats keyed by worker index; each worker owns at most one connection.
    connected_peers: Mutex<HashMap<usize, PeerConnectionStats>>,
    has_announced: AtomicBool,
    reannounce_interval_secs: AtomicU64,
    downloaded_bytes: AtomicU64,
    speed_bps: AtomicU64,
    completed_pieces_count: AtomicUsize,
    target_threads: AtomicUsize,
    active_workers: AtomicUsize,
    is_auto_threads: AtomicBool,
    is_paused: AtomicBool,
    is_cancelled: AtomicBool,
    is_resume_state_dirty: AtomicBool,
    wake_workers: Notify,
}

#[derive(Debug, Clone)]
pub struct TorrentJobSnapshot {
    pub id: usize,
    pub name: String,
    pub game_install_dir: PathBuf,
    pub info_hash_hex: String,
    pub status: DownloadStatus,
    pub status_message: String,
    pub total_bytes: u64,
    pub downloaded_bytes: u64,
    pub speed_bps: u64,
    pub total_pieces: usize,
    pub completed_pieces: usize,
    pub target_threads: usize,
    pub active_workers: usize,
    pub is_auto_threads: bool,
    pub peer_count: usize,
    /// Known peers that are not backing off after failed connections.
    pub available_peer_count: usize,
    /// Open connections, fastest first.
    pub connected_peers: Vec<PeerConnectionStats>,
    pub files_count: usize,
    pub piece_states_preview: Vec<PieceStatus>,
}

impl TorrentJobSnapshot {
    pub fn progress_ratio(&self) -> f64 {
        if self.total_bytes == 0 {
            return 0.0;
        }
        (self.downloaded_bytes as f64 / self.total_bytes as f64).clamp(0.0, 1.0)
    }
}

impl TorrentJob {
    /// Builds a job from the per-piece completion state loaded from disk.
    pub(crate) fn new(
        id: usize,
        meta: Arc<TorrentMeta>,
        game_install_dir: PathBuf,
        completed_pieces: &[bool],
        initial_threads: usize,
    ) -> Self {
        let mut piece_states = Vec::with_capacity(completed_pieces.len());
        let mut pending_pieces = VecDeque::new();
        let mut initial_bytes = 0u64;
        for (piece_index, &is_done) in completed_pieces.iter().enumerate() {
            if is_done {
                piece_states.push(PieceStatus::Completed);
                initial_bytes += meta.piece_size(piece_index);
            } else {
                piece_states.push(PieceStatus::Pending);
                pending_pieces.push_back(piece_index);
            }
        }
        let initial_completed = piece_states.len() - pending_pieces.len();

        let (initial_status, initial_message) = if pending_pieces.is_empty() {
            (
                DownloadStatus::Completed,
                format!("Verified 100% complete in {}", game_install_dir.display()),
            )
        } else if initial_completed > 0 {
            (
                DownloadStatus::ConnectingTrackers,
                format!(
                    "Resumed from disk ({}/{} pieces verified)",
                    initial_completed,
                    meta.pieces.len()
                ),
            )
        } else {
            (
                DownloadStatus::ConnectingTrackers,
                "Contacting trackers for peers...".to_string(),
            )
        };

        Self {
            id,
            meta,
            game_install_dir,
            status: RwLock::new(initial_status),
            status_message: RwLock::new(initial_message),
            piece_states: RwLock::new(piece_states),
            pending_pieces: Mutex::new(pending_pieces),
            peer_pool: Mutex::new(PeerPool::default()),
            connected_peers: Mutex::new(HashMap::new()),
            has_announced: AtomicBool::new(false),
            reannounce_interval_secs: AtomicU64::new(DEFAULT_REANNOUNCE_INTERVAL_SECS),
            downloaded_bytes: AtomicU64::new(initial_bytes),
            speed_bps: AtomicU64::new(0),
            completed_pieces_count: AtomicUsize::new(initial_completed),
            target_threads: AtomicUsize::new(initial_threads),
            active_workers: AtomicUsize::new(0),
            is_auto_threads: AtomicBool::new(true),
            is_paused: AtomicBool::new(false),
            is_cancelled: AtomicBool::new(false),
            is_resume_state_dirty: AtomicBool::new(false),
            wake_workers: Notify::new(),
        }
    }

    pub(crate) fn id(&self) -> usize {
        self.id
    }

    pub(crate) fn speed_bps(&self) -> u64 {
        self.speed_bps.load(Ordering::Relaxed)
    }

    pub(crate) fn target_threads(&self) -> usize {
        self.target_threads.load(Ordering::Relaxed)
    }

    pub(crate) fn is_auto_threads(&self) -> bool {
        self.is_auto_threads.load(Ordering::Relaxed)
    }

    pub(crate) fn is_paused(&self) -> bool {
        self.is_paused.load(Ordering::Relaxed)
    }

    pub(crate) fn is_cancelled(&self) -> bool {
        self.is_cancelled.load(Ordering::Relaxed)
    }

    pub(crate) fn cancel(&self) {
        self.is_cancelled.store(true, Ordering::Relaxed);
        self.wake_workers.notify_waiters();
    }

    pub(crate) fn mark_worker_active(&self) {
        self.active_workers.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn mark_worker_idle(&self) {
        self.active_workers.fetch_sub(1, Ordering::Relaxed);
    }

    pub(crate) fn has_peers(&self) -> bool {
        !lock_ignoring_poison(&self.peer_pool).is_empty()
    }

    /// Picks a peer that is not backing off after failures; `spread_offset` spreads
    /// different workers and pieces across peers. `None` means every known peer is backing off.
    pub(crate) fn pick_peer(&self, spread_offset: usize) -> Option<SocketAddr> {
        lock_ignoring_poison(&self.peer_pool).pick(spread_offset, Instant::now())
    }

    pub(crate) fn update_peer_connection(&self, worker_index: usize, stats: PeerConnectionStats) {
        lock_ignoring_poison(&self.connected_peers).insert(worker_index, stats);
    }

    pub(crate) fn forget_peer_connection(&self, worker_index: usize) {
        lock_ignoring_poison(&self.connected_peers).remove(&worker_index);
    }

    pub(crate) fn record_peer_success(&self, peer_addr: SocketAddr) {
        lock_ignoring_poison(&self.peer_pool).record_success(peer_addr);
    }

    pub(crate) fn record_peer_failure(&self, peer_addr: SocketAddr) {
        lock_ignoring_poison(&self.peer_pool)
            .record_failure(peer_addr, Instant::now());
    }

    /// Copies only the first `piece_preview_len` piece states, since callers render a
    /// bounded preview and copying every piece each frame is wasted work on big torrents.
    pub async fn snapshot(&self, piece_preview_len: usize) -> TorrentJobSnapshot {
        let piece_states_preview = self
            .piece_states
            .read()
            .await
            .iter()
            .take(piece_preview_len)
            .copied()
            .collect();

        let peer_pool_counts = {
            let peer_pool = lock_ignoring_poison(&self.peer_pool);
            (peer_pool.len(), peer_pool.available_count(Instant::now()))
        };

        let mut connected_peers: Vec<PeerConnectionStats> =
            lock_ignoring_poison(&self.connected_peers)
                .values()
                .copied()
                .collect();
        connected_peers.sort_unstable_by_key(|peer| Reverse(peer.download_bps));

        TorrentJobSnapshot {
            id: self.id,
            name: self.meta.name.clone(),
            game_install_dir: self.game_install_dir.clone(),
            info_hash_hex: self.meta.info_hash_hex.clone(),
            status: *self.status.read().await,
            status_message: self.status_message.read().await.clone(),
            total_bytes: self.meta.total_length,
            downloaded_bytes: self.downloaded_bytes.load(Ordering::Relaxed),
            speed_bps: self.speed_bps.load(Ordering::Relaxed),
            total_pieces: self.meta.pieces.len(),
            completed_pieces: self.completed_pieces_count.load(Ordering::Relaxed),
            target_threads: self.target_threads.load(Ordering::Relaxed),
            active_workers: self.active_workers.load(Ordering::Relaxed),
            is_auto_threads: self.is_auto_threads.load(Ordering::Relaxed),
            peer_count: peer_pool_counts.0,
            available_peer_count: peer_pool_counts.1,
            connected_peers,
            files_count: self.meta.files.len(),
            piece_states_preview,
        }
    }

    // `is_paused` only changes while the status lock is held, so `set_status_unless_paused`
    // can never slip a background status in between a pause and its `Paused` status.
    pub async fn pause(&self) {
        {
            let mut status = self.status.write().await;
            if *status == DownloadStatus::Completed {
                return;
            }
            self.is_paused.store(true, Ordering::Relaxed);
            *status = DownloadStatus::Paused;
        }
        self.speed_bps.store(0, Ordering::Relaxed);
        self.set_status_message("Download paused.").await;
    }

    pub async fn resume(&self) {
        let has_peers = self.has_peers();
        {
            let mut status = self.status.write().await;
            if *status == DownloadStatus::Completed {
                return;
            }
            self.is_paused.store(false, Ordering::Relaxed);
            *status = if has_peers {
                DownloadStatus::Downloading
            } else {
                DownloadStatus::ConnectingTrackers
            };
        }
        let message = if has_peers {
            "Resuming multi-threaded download..."
        } else {
            "Re-announcing to trackers for peers..."
        };
        self.set_status_message(message).await;
        self.wake_workers.notify_waiters();
    }

    /// Returns whether the status was applied; background tasks use this so they never
    /// overwrite a pause the user made or a finished download.
    async fn set_status_unless_paused(&self, new_status: DownloadStatus) -> bool {
        let mut status = self.status.write().await;
        if self.is_paused.load(Ordering::Relaxed) || *status == DownloadStatus::Completed {
            return false;
        }
        *status = new_status;
        true
    }

    pub async fn toggle_pause(&self) -> DownloadStatus {
        if self.is_paused.load(Ordering::Relaxed) {
            self.resume().await;
        } else {
            self.pause().await;
        }
        *self.status.read().await
    }

    /// Manual changes switch this job off automatic thread tuning.
    pub fn adjust_threads_manually(&self, delta: isize) -> usize {
        self.is_auto_threads.store(false, Ordering::Relaxed);
        self.change_target_threads(delta)
    }

    /// Returns whether automatic thread tuning is now on.
    pub fn toggle_auto_threads(&self) -> bool {
        let was_auto = self.is_auto_threads.fetch_xor(true, Ordering::Relaxed);
        !was_auto
    }

    pub(crate) async fn is_tunable(&self) -> bool {
        self.is_auto_threads.load(Ordering::Relaxed)
            && !self.is_cancelled.load(Ordering::Relaxed)
            && *self.status.read().await == DownloadStatus::Downloading
    }

    /// Adds `delta` to the worker thread count, clamped to `1..=MAX_WORKER_THREADS`,
    /// and returns the new count.
    pub(crate) fn change_target_threads(&self, delta: isize) -> usize {
        let current = self.target_threads.load(Ordering::Relaxed);
        let updated = current
            .saturating_add_signed(delta)
            .clamp(1, MAX_WORKER_THREADS);
        self.target_threads.store(updated, Ordering::Relaxed);
        self.wake_workers.notify_waiters();
        updated
    }

    pub(crate) fn is_complete(&self) -> bool {
        // Acquire pairs with the Release in `complete_piece`, so whoever sees the final count
        // also sees the dirty flag set for that piece.
        self.completed_pieces_count.load(Ordering::Acquire) >= self.meta.pieces.len()
    }

    pub(crate) async fn set_status_message(&self, message: impl Into<String>) {
        *self.status_message.write().await = message.into();
    }


    async fn persist_resume_state(&self) -> Result<()> {
        let completed_pieces: Vec<bool> = self
            .piece_states
            .read()
            .await
            .iter()
            .map(|&state| state == PieceStatus::Completed)
            .collect();
        let meta = self.meta.clone();
        let game_install_dir = self.game_install_dir.clone();
        tokio::task::spawn_blocking(move || {
            save_resume_state(&meta, &game_install_dir, &completed_pieces)
        })
        .await?
    }

    /// Contacts the trackers, merges the peers they return into the pool, and returns
    /// whether the job now knows any peers.
    pub(crate) async fn announce(&self, peer_id: &[u8; 20]) -> bool {
        let event = if self.has_announced.swap(true, Ordering::Relaxed) {
            AnnounceEvent::Periodic
        } else {
            AnnounceEvent::Started
        };
        let response = discover_peers(&self.meta, peer_id, LISTEN_PORT, event).await;
        if let Some(interval) = response.interval {
            let interval_secs = interval
                .as_secs()
                .clamp(MIN_REANNOUNCE_INTERVAL_SECS, MAX_REANNOUNCE_INTERVAL_SECS);
            self.reannounce_interval_secs
                .store(interval_secs, Ordering::Relaxed);
        }

        let (new_peer_count, known_peer_count) = {
            let mut peer_pool = lock_ignoring_poison(&self.peer_pool);
            let new_peer_count = peer_pool.merge(response.peers);
            (new_peer_count, peer_pool.len())
        };

        if known_peer_count == 0 {
            if self
                .set_status_unless_paused(DownloadStatus::NoPeersFound)
                .await
            {
                self.set_status_message(
                    "0 peers returned by tracker(s). Press 'r' to retry announce.",
                )
                .await;
            }
            return false;
        }

        if self
            .set_status_unless_paused(DownloadStatus::Downloading)
            .await
        {
            self.set_status_message(format!(
                "{} peer(s) known (+{} new from trackers) across {} worker threads",
                known_peer_count,
                new_peer_count,
                self.target_threads.load(Ordering::Relaxed)
            ))
            .await;
        }
        true
    }

    fn reannounce_interval(&self) -> Duration {
        Duration::from_secs(self.reannounce_interval_secs.load(Ordering::Relaxed))
    }

    pub(crate) async fn wait_for_wake_or_timeout(&self, timeout: Duration) {
        let _ = tokio::time::timeout(timeout, self.wake_workers.notified()).await;
    }
}

/// The mutexes in a job guard plain values (indices, peer entries) that stay valid even if a
/// holder panicked, so a poisoned lock is recovered instead of propagating the panic.
fn lock_ignoring_poison<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}
