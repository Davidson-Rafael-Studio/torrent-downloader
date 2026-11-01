use crate::job::{TorrentJob, MAX_WORKER_THREADS};
use crate::peer::PeerSession;
use anyhow::{Context, Result};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

const IDLE_WORKER_POLL_INTERVAL: Duration = Duration::from_millis(250);
const REANNOUNCE_INTERVAL: Duration = Duration::from_secs(2);
const NO_PENDING_PIECE_BACKOFF: Duration = Duration::from_millis(100);
const PIECE_RETRY_BACKOFF: Duration = Duration::from_millis(300);
const ALL_PEERS_BACKING_OFF_WAIT: Duration = Duration::from_secs(1);

pub(crate) fn spawn_worker_pool(job: Arc<TorrentJob>, peer_id: [u8; 20]) {
    tokio::spawn(async move {
        job.announce(&peer_id).await;

        // Spawn the full pool once; workers above `target_threads` idle, so the count can be
        // changed live without spawning or killing tasks.
        for worker_index in 0..MAX_WORKER_THREADS {
            tokio::spawn(run_worker_loop(job.clone(), worker_index, peer_id));
        }
    });
}

async fn run_worker_loop(job: Arc<TorrentJob>, worker_index: usize, peer_id: [u8; 20]) {
    let mut session: Option<(SocketAddr, PeerSession)> = None;

    while !job.is_cancelled() && !job.is_complete() {
        let is_idle = job.is_paused() || worker_index >= job.target_threads();
        if is_idle {
            session = None;
            job.wait_for_wake_or_timeout(IDLE_WORKER_POLL_INTERVAL).await;
            continue;
        }

        if !job.has_peers() {
            // Only worker 0 re-announces, so trackers are not hit by every worker at once.
            if worker_index == 0 && job.announce(&peer_id).await {
                continue;
            }
            job.wait_for_wake_or_timeout(REANNOUNCE_INTERVAL).await;
            continue;
        }

        let Some(piece_index) = job.claim_next_pending_piece().await else {
            tokio::time::sleep(NO_PENDING_PIECE_BACKOFF).await;
            continue;
        };

        let Some(peer_addr) = job.pick_peer(worker_index + piece_index) else {
            // Every known peer failed recently; wait for a backoff to expire instead of spinning.
            job.release_piece(piece_index).await;
            job.wait_for_wake_or_timeout(ALL_PEERS_BACKING_OFF_WAIT).await;
            continue;
        };

        job.mark_worker_active();
        match download_piece_from_peer(&job, &mut session, peer_addr, piece_index, &peer_id).await
        {
            Ok(Some(piece_bytes)) => {
                job.record_peer_success(peer_addr);
                job.store_verified_piece(piece_index, piece_bytes).await;
            }
            Ok(None) => {
                // The peer is fine but lacks this piece: hand the piece back without
                // penalizing the peer, so another worker or peer can take it.
                job.release_piece(piece_index).await;
                tokio::time::sleep(NO_PENDING_PIECE_BACKOFF).await;
            }
            Err(error) => {
                session = None;
                job.record_peer_failure(peer_addr);
                job.release_piece(piece_index).await;
                if !job.is_paused() {
                    job.set_status_message(format!("Piece {piece_index} retry: {error:#}"))
                        .await;
                }
                tokio::time::sleep(PIECE_RETRY_BACKOFF).await;
            }
        }
        job.mark_worker_idle();
    }
}

/// Downloads and SHA-1 verifies one piece from `peer_addr`, reusing the worker's session when
/// it is already connected to that peer. Returns `Ok(None)` when the peer lacks the piece.
async fn download_piece_from_peer(
    job: &TorrentJob,
    session: &mut Option<(SocketAddr, PeerSession)>,
    peer_addr: SocketAddr,
    piece_index: usize,
    peer_id: &[u8; 20],
) -> Result<Option<Vec<u8>>> {
    let peer_session = match session {
        Some((connected_addr, peer_session)) if *connected_addr == peer_addr => peer_session,
        _ => {
            let new_session =
                PeerSession::connect(peer_addr, &job.meta.info_hash, peer_id, job.meta.pieces.len())
                    .await
                    .with_context(|| format!("connect to peer {peer_addr}"))?;
            &mut session.insert((peer_addr, new_session)).1
        }
    };

    if !peer_session.has_piece(piece_index) {
        return Ok(None);
    }
    peer_session
        .download_piece(
            piece_index,
            job.meta.piece_size(piece_index),
            &job.meta.pieces[piece_index],
        )
        .await
        .map(Some)
        .with_context(|| format!("download from peer {peer_addr}"))
}
