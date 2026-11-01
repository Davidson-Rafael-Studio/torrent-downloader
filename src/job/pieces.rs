use super::{lock_ignoring_poison, DownloadStatus, PieceStatus, TorrentJob};
use crate::storage::write_piece_to_disk;
use anyhow::Result;
use std::sync::atomic::Ordering;

impl TorrentJob {
    async fn mark_completed(&self) {
        *self.status.write().await = DownloadStatus::Completed;
        self.speed_bps.store(0, Ordering::Relaxed);
        self.set_status_message(format!(
            "Completed! Saved to Steam: {}",
            self.game_install_dir.display()
        ))
        .await;
    }

    pub(crate) fn has_pending_pieces(&self) -> bool {
        !lock_ignoring_poison(&self.pending_pieces).is_empty()
    }

    /// Claims the first pending piece for which `peer_has_piece` is true. The queue is scanned
    /// in order, which finds a match immediately for seeders and only costs more for partial peers.
    pub(crate) async fn claim_pending_piece_where(
        &self,
        peer_has_piece: impl Fn(usize) -> bool,
    ) -> Option<usize> {
        let piece_index = {
            let mut pending_pieces = lock_ignoring_poison(&self.pending_pieces);
            let position = pending_pieces
                .iter()
                .position(|&piece_index| peer_has_piece(piece_index))?;
            pending_pieces.remove(position)?
        };
        self.piece_states.write().await[piece_index] = PieceStatus::InProgress;
        Some(piece_index)
    }

    pub(crate) async fn release_piece(&self, piece_index: usize) {
        self.piece_states.write().await[piece_index] = PieceStatus::Pending;
        lock_ignoring_poison(&self.pending_pieces).push_back(piece_index);
    }

    /// Records a verified piece and returns the new completed-piece count.
    async fn complete_piece(&self, piece_index: usize) -> usize {
        self.piece_states.write().await[piece_index] = PieceStatus::Completed;
        self.is_resume_state_dirty.store(true, Ordering::Relaxed);
        self.downloaded_bytes
            .fetch_add(self.meta.piece_size(piece_index), Ordering::Relaxed);
        self.completed_pieces_count
            .fetch_add(1, Ordering::Release)
            + 1
    }

    async fn write_piece(&self, piece_index: usize, piece_bytes: Vec<u8>) -> Result<()> {
        let meta = self.meta.clone();
        let game_install_dir = self.game_install_dir.clone();
        tokio::task::spawn_blocking(move || {
            write_piece_to_disk(&meta, &game_install_dir, piece_index, &piece_bytes)
        })
        .await?
    }

    pub(crate) async fn store_verified_piece(&self, piece_index: usize, piece_bytes: Vec<u8>) {
        if let Err(error) = self.write_piece(piece_index, piece_bytes).await {
            self.release_piece(piece_index).await;
            *self.status.write().await = DownloadStatus::Error;
            self.set_status_message(format!(
                "Disk write error for piece {piece_index}: {error:#}"
            ))
            .await;
            return;
        }

        let completed_count = self.complete_piece(piece_index).await;
        if completed_count == self.meta.pieces.len() {
            self.mark_completed().await;
        } else if self
            .set_status_unless_paused(DownloadStatus::Downloading)
            .await
        {
            // Setting Downloading above also clears an Error from an earlier failed disk write.
            self.set_status_message(format!(
                "Downloading piece {}/{} via {} thread(s)",
                completed_count,
                self.meta.pieces.len(),
                self.target_threads.load(Ordering::Relaxed)
            ))
            .await;
        }
    }
}
