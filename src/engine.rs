use crate::job::{TorrentJob, MAX_WORKER_THREADS};
use crate::storage::load_or_verify_resume_state;
use crate::thread_tuner::{ThreadTuner, TunableJob};
use crate::torrent::TorrentMeta;
use crate::worker::spawn_worker_pool;
use anyhow::Result;
use rand::Rng;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Weak};
use std::time::Duration;
use tokio::sync::RwLock;

/// Long enough for a new connection to ramp up and for the smoothed speed to settle.
const THREAD_TUNE_INTERVAL: Duration = Duration::from_secs(3);

pub struct DownloadManager {
    pub jobs: RwLock<Vec<Arc<TorrentJob>>>,
    pub steam_common_dir: RwLock<PathBuf>,
    default_threads: usize,
    next_id: AtomicUsize,
    peer_id: [u8; 20],
}

impl DownloadManager {
    pub fn new(steam_common_dir: PathBuf, default_threads: usize) -> Arc<Self> {
        let mut peer_id = *b"-TD0100-RUSTSTEAM000";
        let mut rng = rand::thread_rng();
        for digit in &mut peer_id[14..20] {
            *digit = b'0' + rng.gen_range(0..10);
        }

        let manager = Arc::new(Self {
            jobs: RwLock::new(Vec::new()),
            steam_common_dir: RwLock::new(steam_common_dir),
            default_threads: default_threads.clamp(1, MAX_WORKER_THREADS),
            next_id: AtomicUsize::new(1),
            peer_id,
        });
        spawn_thread_tuner(Arc::downgrade(&manager));
        manager
    }

    /// Adds a `.torrent` file to the queue and starts downloading it into the Steam folder.
    /// Re-adding a torrent that is already queued returns the existing job untouched,
    /// so a paused job stays paused.
    pub async fn add_torrent_file(self: &Arc<Self>, path: &Path) -> Result<Arc<TorrentJob>> {
        let meta = Arc::new(TorrentMeta::from_file(path)?);

        let existing_job = self
            .jobs
            .read()
            .await
            .iter()
            .find(|job| job.meta.info_hash == meta.info_hash)
            .cloned();
        if let Some(job) = existing_job {
            return Ok(job);
        }

        let steam_common_dir = self.steam_common_dir.read().await.clone();
        let meta_to_verify = meta.clone();
        let (game_install_dir, completed_pieces) = tokio::task::spawn_blocking(move || {
            load_or_verify_resume_state(&meta_to_verify, &steam_common_dir)
        })
        .await??;

        let job = Arc::new(TorrentJob::new(
            self.next_id.fetch_add(1, Ordering::Relaxed),
            meta,
            game_install_dir,
            &completed_pieces,
            self.default_threads,
        ));

        self.jobs.write().await.push(job.clone());

        if !job.is_complete() {
            let speed_meter_job = job.clone();
            tokio::spawn(async move { speed_meter_job.run_speed_meter().await });
            let resume_saver_job = job.clone();
            tokio::spawn(async move { resume_saver_job.run_resume_state_saver().await });
            let announce_job = job.clone();
            let peer_id = self.peer_id;
            tokio::spawn(async move { announce_job.run_periodic_announce(peer_id).await });
            spawn_worker_pool(job.clone(), self.peer_id);
        }

        Ok(job)
    }

    pub async fn pause_all(&self) {
        for job in self.jobs.read().await.iter() {
            job.pause().await;
        }
    }

    pub async fn resume_all(&self) {
        for job in self.jobs.read().await.iter() {
            job.resume().await;
        }
    }

    pub async fn remove_job(&self, index: usize) {
        let mut jobs = self.jobs.write().await;
        if index < jobs.len() {
            let removed = jobs.remove(index);
            removed.cancel();
        }
    }
}

/// Re-balances the worker counts of auto-tuned jobs against the total download speed.
/// Holds only a `Weak` so the task ends once the manager is dropped.
fn spawn_thread_tuner(manager: Weak<DownloadManager>) {
    tokio::spawn(async move {
        let mut tuner = ThreadTuner::new(MAX_WORKER_THREADS);
        let mut tune_interval = tokio::time::interval(THREAD_TUNE_INTERVAL);
        loop {
            tune_interval.tick().await;
            let Some(manager) = manager.upgrade() else {
                break;
            };
            let jobs = manager.jobs.read().await.clone();

            // Manual and auto jobs share the same link, so all of them count toward the total.
            let mut total_speed_bps = 0;
            let mut tunable_jobs = Vec::new();
            for job in &jobs {
                total_speed_bps += job.speed_bps();
                if job.is_tunable().await {
                    tunable_jobs.push(TunableJob {
                        id: job.id(),
                        worker_count: job.target_threads(),
                    });
                }
            }

            let Some(adjustment) = tuner.step(total_speed_bps, &tunable_jobs) else {
                continue;
            };
            let adjusted_job = jobs.iter().find(|job| job.id() == adjustment.job_id);
            if let Some(job) = adjusted_job.filter(|job| job.is_auto_threads()) {
                job.change_target_threads(adjustment.delta);
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::job::DownloadStatus;
    use crate::seeder::LocalTestSwarm;
    use crate::storage::ResumeManifest;

    #[tokio::test]
    async fn test_multi_download_pause_resume_and_steam_save() {
        let temp_root = std::env::temp_dir().join("torrent_downloader_test_steam");
        let _ = std::fs::remove_dir_all(&temp_root);
        let steam_common = temp_root.join("steamapps/common");
        let torrents_dir = temp_root.join("torrents");

        let swarm = LocalTestSwarm::start().await.unwrap();
        let alpha_torrent = swarm
            .create_and_seed_sample_game(&torrents_dir, "TestGameAlpha", 1)
            .await
            .unwrap();
        let beta_torrent = swarm
            .create_and_seed_sample_game(&torrents_dir, "TestGameBeta", 1)
            .await
            .unwrap();

        let manager = DownloadManager::new(steam_common.clone(), 4);
        let alpha_job = manager.add_torrent_file(&alpha_torrent).await.unwrap();
        let beta_job = manager.add_torrent_file(&beta_torrent).await.unwrap();

        tokio::time::sleep(Duration::from_millis(250)).await;

        alpha_job.pause().await;
        assert_eq!(alpha_job.snapshot(0).await.status, DownloadStatus::Paused);

        tokio::time::sleep(Duration::from_millis(150)).await;
        alpha_job.resume().await;

        let mut has_seen_connected_peer = false;
        for _ in 0..80 {
            let beta_snapshot = beta_job.snapshot(0).await;
            has_seen_connected_peer |= !beta_snapshot.connected_peers.is_empty();
            let alpha_status = alpha_job.snapshot(0).await.status;
            if alpha_status == DownloadStatus::Completed
                && beta_snapshot.status == DownloadStatus::Completed
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        assert!(has_seen_connected_peer, "peer stats must be reported while downloading");

        assert_eq!(alpha_job.snapshot(0).await.status, DownloadStatus::Completed);
        assert_eq!(beta_job.snapshot(0).await.status, DownloadStatus::Completed);
        assert!(steam_common.join("TestGameAlpha/TestGameAlpha.exe").exists());
        assert!(steam_common.join("TestGameBeta/TestGameBeta.exe").exists());
        let mut has_cleared_peer_stats = false;
        for _ in 0..20 {
            if beta_job.snapshot(0).await.connected_peers.is_empty() {
                has_cleared_peer_stats = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(
            has_cleared_peer_stats,
            "workers must drop their peer stats once the download completes"
        );

        let manifest_path = steam_common.join("TestGameAlpha/.torrent_resume.json");
        let mut is_manifest_complete = false;
        for _ in 0..30 {
            if let Ok(raw) = std::fs::read_to_string(&manifest_path) {
                let manifest: ResumeManifest = serde_json::from_str(&raw).unwrap();
                if manifest.completed_pieces.iter().all(|&is_done| is_done) {
                    is_manifest_complete = true;
                    break;
                }
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        assert!(
            is_manifest_complete,
            "resume manifest must end with every piece completed"
        );

        let _ = std::fs::remove_dir_all(&temp_root);
    }

    #[tokio::test]
    async fn test_readding_paused_torrent_keeps_it_paused_and_threads_are_clamped() {
        let temp_root = std::env::temp_dir().join("torrent_downloader_test_readd");
        let _ = std::fs::remove_dir_all(&temp_root);
        let swarm = LocalTestSwarm::start().await.unwrap();
        let torrent = swarm
            .create_and_seed_sample_game(&temp_root.join("torrents"), "TestGameReadd", 2)
            .await
            .unwrap();
        let manager = DownloadManager::new(temp_root.join("steamapps/common"), 4);

        let job = manager.add_torrent_file(&torrent).await.unwrap();
        job.pause().await;
        let readded_job = manager.add_torrent_file(&torrent).await.unwrap();

        assert!(Arc::ptr_eq(&job, &readded_job));
        assert_eq!(job.snapshot(0).await.status, DownloadStatus::Paused);
        assert_eq!(manager.jobs.read().await.len(), 1);

        assert_eq!(job.change_target_threads(100), MAX_WORKER_THREADS);
        assert_eq!(job.change_target_threads(-100), 1);

        let _ = std::fs::remove_dir_all(&temp_root);
    }

    #[tokio::test]
    async fn test_pause_survives_failed_announce_and_resume_without_peers_is_not_downloading() {
        let temp_root = std::env::temp_dir().join("torrent_downloader_test_no_peers");
        let _ = std::fs::remove_dir_all(&temp_root);
        std::fs::create_dir_all(&temp_root).unwrap();
        let unreachable_tracker = "http://127.0.0.1:1/announce";
        let torrent_bytes = crate::torrent::create_sample_torrent_bytes(
            "TestGameNoPeers",
            &[("TestGameNoPeers.exe", b"MZ-FAKE-GAME-BINARY-DATA-1234567890")],
            16,
            unreachable_tracker,
        )
        .unwrap();
        let torrent = temp_root.join("TestGameNoPeers.torrent");
        std::fs::write(&torrent, torrent_bytes).unwrap();
        let manager = DownloadManager::new(temp_root.join("steamapps/common"), 4);

        let job = manager.add_torrent_file(&torrent).await.unwrap();
        job.pause().await;
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert_eq!(job.snapshot(0).await.status, DownloadStatus::Paused);

        job.resume().await;
        assert_eq!(job.snapshot(0).await.status, DownloadStatus::ConnectingTrackers);

        let mut observed_statuses = Vec::new();
        for _ in 0..30 {
            let status = job.snapshot(0).await.status;
            observed_statuses.push(status);
            if status == DownloadStatus::NoPeersFound {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        assert_eq!(observed_statuses.last(), Some(&DownloadStatus::NoPeersFound));
        assert!(!observed_statuses.contains(&DownloadStatus::Downloading));

        manager.remove_job(0).await;
        let _ = std::fs::remove_dir_all(&temp_root);
    }
}
