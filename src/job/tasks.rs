use super::TorrentJob;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

const RESUME_SAVE_INTERVAL: Duration = Duration::from_secs(1);
const SPEED_SAMPLE_INTERVAL: Duration = Duration::from_millis(500);

impl TorrentJob {
    pub(crate) async fn run_speed_meter(&self) {
        let mut previous_bytes = self.downloaded_bytes.load(Ordering::Relaxed);
        let mut previous_sample_at = Instant::now();
        loop {
            tokio::time::sleep(SPEED_SAMPLE_INTERVAL).await;
            if self.is_cancelled() || self.is_complete() {
                self.speed_bps.store(0, Ordering::Relaxed);
                break;
            }

            let current_bytes = self.downloaded_bytes.load(Ordering::Relaxed);
            if self.is_paused() {
                self.speed_bps.store(0, Ordering::Relaxed);
            } else {
                let elapsed_secs = previous_sample_at.elapsed().as_secs_f64().max(0.1);
                let sample_bps =
                    (current_bytes.saturating_sub(previous_bytes) as f64 / elapsed_secs) as u64;
                let previous_bps = self.speed_bps.load(Ordering::Relaxed);
                // Moving average weighted 5/8 toward the newest sample: still responsive,
                // but hides the burstiness of whole pieces landing at once.
                let smoothed_bps = if previous_bps == 0 {
                    sample_bps
                } else {
                    (previous_bps * 3 + sample_bps * 5) / 8
                };
                self.speed_bps.store(smoothed_bps, Ordering::Relaxed);
            }
            previous_bytes = current_bytes;
            previous_sample_at = Instant::now();
        }
    }

    /// The only writer of the resume manifest. Workers just flag the state dirty, so saves are
    /// serialized (an older snapshot can never overwrite a newer one) and the manifest is
    /// serialized at most once per interval instead of once per piece.
    pub(crate) async fn run_resume_state_saver(&self) {
        let mut save_interval = tokio::time::interval(RESUME_SAVE_INTERVAL);
        loop {
            save_interval.tick().await;
            let is_finished = self.is_cancelled() || self.is_complete();

            if self.is_resume_state_dirty.swap(false, Ordering::Relaxed) {
                if let Err(error) = self.persist_resume_state().await {
                    self.is_resume_state_dirty.store(true, Ordering::Relaxed);
                    self.set_status_message(format!("Failed to save resume state: {error:#}"))
                        .await;
                }
            }

            if is_finished {
                break;
            }
        }
    }

    /// Re-announces on the tracker's interval and merges new peers into the pool, so peers
    /// that left the swarm are replaced by ones that joined after the download started.
    pub(crate) async fn run_periodic_announce(&self, peer_id: [u8; 20]) {
        loop {
            tokio::time::sleep(self.reannounce_interval()).await;
            if self.is_cancelled() || self.is_complete() {
                break;
            }
            if !self.is_paused() {
                self.announce(&peer_id).await;
            }
        }
    }
}
