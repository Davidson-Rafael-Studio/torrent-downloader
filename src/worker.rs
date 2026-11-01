use crate::job::{TorrentJob, MAX_WORKER_THREADS};
use crate::peer::{PeerSession, PieceInProgress};
use std::sync::Arc;
use std::time::Duration;

const IDLE_WORKER_POLL_INTERVAL: Duration = Duration::from_millis(250);
const REANNOUNCE_INTERVAL: Duration = Duration::from_secs(2);
const NO_PENDING_PIECE_BACKOFF: Duration = Duration::from_millis(100);
const PEER_RETRY_BACKOFF: Duration = Duration::from_millis(300);
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

/// Each worker owns at most one peer connection and keeps several pieces in flight on it, so
/// the connection stays busy across piece boundaries.
async fn run_worker_loop(job: Arc<TorrentJob>, worker_index: usize, peer_id: [u8; 20]) {
    let mut session: Option<PeerSession> = None;
    let mut active_pieces: Vec<PieceInProgress> = Vec::new();
    let mut connect_attempts = 0usize;

    while !job.is_cancelled() && !job.is_complete() {
        let is_idle = job.is_paused() || worker_index >= job.target_threads();
        if is_idle {
            disconnect(&job, worker_index, &mut session, &mut active_pieces).await;
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

        let peer_session = match session.as_mut() {
            Some(peer_session) => peer_session,
            None => {
                // Adding the attempt count makes every reconnect try the next available peer.
                let Some(peer_addr) = job.pick_peer(worker_index + connect_attempts) else {
                    // Every known peer failed recently; wait for a backoff to expire.
                    job.wait_for_wake_or_timeout(ALL_PEERS_BACKING_OFF_WAIT).await;
                    continue;
                };
                connect_attempts += 1;
                match PeerSession::connect(
                    peer_addr,
                    &job.meta.info_hash,
                    &peer_id,
                    job.meta.pieces.len(),
                )
                .await
                {
                    Ok(new_session) => {
                        job.update_peer_connection(worker_index, new_session.stats());
                        session.insert(new_session)
                    }
                    Err(error) => {
                        job.record_peer_failure(peer_addr);
                        if !job.is_paused() {
                            job.set_status_message(format!(
                                "Peer {peer_addr} unreachable: {error:#}"
                            ))
                            .await;
                        }
                        tokio::time::sleep(PEER_RETRY_BACKOFF).await;
                        continue;
                    }
                }
            }
        };

        while peer_session.wants_more_pieces(&active_pieces) {
            let Some(piece_index) = job
                .claim_pending_piece_where(|piece_index| peer_session.has_piece(piece_index))
                .await
            else {
                break;
            };
            active_pieces.push(PieceInProgress::new(
                piece_index,
                job.meta.piece_size(piece_index),
                job.meta.pieces[piece_index],
            ));
        }

        if active_pieces.is_empty() {
            if job.has_pending_pieces() {
                // This peer has none of the remaining pieces; free the slot for another peer
                // without penalizing this one.
                disconnect(&job, worker_index, &mut session, &mut active_pieces).await;
            }
            tokio::time::sleep(NO_PENDING_PIECE_BACKOFF).await;
            continue;
        }

        job.mark_worker_active();
        let download_result = peer_session.download_next_piece(&mut active_pieces).await;
        let peer_addr = peer_session.addr();
        let peer_stats = peer_session.stats();
        job.mark_worker_idle();

        match download_result {
            Ok(piece) => {
                job.record_peer_success(peer_addr);
                job.update_peer_connection(worker_index, peer_stats);
                job.store_verified_piece(piece.index(), piece.into_bytes())
                    .await;
            }
            Err(error) => {
                job.record_peer_failure(peer_addr);
                disconnect(&job, worker_index, &mut session, &mut active_pieces).await;
                if !job.is_paused() {
                    job.set_status_message(format!("Peer {peer_addr} dropped: {error:#}"))
                        .await;
                }
                tokio::time::sleep(PEER_RETRY_BACKOFF).await;
            }
        }
    }
    disconnect(&job, worker_index, &mut session, &mut active_pieces).await;
}

/// Drops the worker's connection and hands its unfinished pieces back to the queue.
async fn disconnect(
    job: &TorrentJob,
    worker_index: usize,
    session: &mut Option<PeerSession>,
    active_pieces: &mut Vec<PieceInProgress>,
) {
    *session = None;
    job.forget_peer_connection(worker_index);
    for piece in active_pieces.drain(..) {
        job.release_piece(piece.index()).await;
    }
}
