use std::collections::HashMap;
use std::net::SocketAddr;
use std::time::{Duration, Instant};

const BASE_RETRY_DELAY: Duration = Duration::from_secs(5);
const MAX_RETRY_DELAY: Duration = Duration::from_secs(300);

#[derive(Debug)]
struct PeerEntry {
    addr: SocketAddr,
    consecutive_failures: u32,
    retry_after: Option<Instant>,
}

impl PeerEntry {
    fn is_available(&self, now: Instant) -> bool {
        self.retry_after.is_none_or(|retry_after| now >= retry_after)
    }
}

/// Every peer a job has heard of, with failure backoff: a peer that fails is skipped for
/// 5 s, then 10 s, 20 s, ... up to 5 minutes, so workers stop wasting connection timeouts on
/// unreachable peers while still re-checking them now and then.
#[derive(Debug, Default)]
pub(super) struct PeerPool {
    peers: Vec<PeerEntry>,
    index_by_addr: HashMap<SocketAddr, usize>,
}

impl PeerPool {
    pub(super) fn len(&self) -> usize {
        self.peers.len()
    }

    pub(super) fn is_empty(&self) -> bool {
        self.peers.is_empty()
    }

    pub(super) fn available_count(&self, now: Instant) -> usize {
        self.peers
            .iter()
            .filter(|entry| entry.is_available(now))
            .count()
    }

    /// Adds peers not seen before and returns how many were new. Known peers keep their
    /// failure history, so a re-announce cannot reset the backoff of a dead peer.
    pub(super) fn merge(&mut self, addrs: impl IntoIterator<Item = SocketAddr>) -> usize {
        let known_before = self.peers.len();
        for addr in addrs {
            if self.index_by_addr.contains_key(&addr) {
                continue;
            }
            self.index_by_addr.insert(addr, self.peers.len());
            self.peers.push(PeerEntry {
                addr,
                consecutive_failures: 0,
                retry_after: None,
            });
        }
        self.peers.len() - known_before
    }

    /// Picks one of the peers that are not backing off; `spread_offset` selects among them
    /// so different workers and pieces land on different peers.
    pub(super) fn pick(&self, spread_offset: usize, now: Instant) -> Option<SocketAddr> {
        let available_count = self.available_count(now);
        if available_count == 0 {
            return None;
        }
        self.peers
            .iter()
            .filter(|entry| entry.is_available(now))
            .nth(spread_offset % available_count)
            .map(|entry| entry.addr)
    }

    pub(super) fn record_failure(&mut self, addr: SocketAddr, now: Instant) {
        let Some(&index) = self.index_by_addr.get(&addr) else {
            return;
        };
        let entry = &mut self.peers[index];
        entry.consecutive_failures = entry.consecutive_failures.saturating_add(1);
        let doublings = (entry.consecutive_failures - 1).min(16);
        let retry_delay = BASE_RETRY_DELAY
            .saturating_mul(1 << doublings)
            .min(MAX_RETRY_DELAY);
        entry.retry_after = Some(now + retry_delay);
    }

    pub(super) fn record_success(&mut self, addr: SocketAddr) {
        if let Some(&index) = self.index_by_addr.get(&addr) {
            let entry = &mut self.peers[index];
            entry.consecutive_failures = 0;
            entry.retry_after = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn peer(last_octet: u8) -> SocketAddr {
        SocketAddr::from(([10, 0, 0, last_octet], 6881))
    }

    #[test]
    fn test_merge_adds_only_new_peers_and_keeps_failure_history() {
        let now = Instant::now();
        let mut pool = PeerPool::default();
        assert_eq!(pool.merge([peer(1), peer(2), peer(1)]), 2);

        pool.record_failure(peer(1), now);
        assert_eq!(pool.merge([peer(1), peer(3)]), 1);
        assert_eq!(pool.len(), 3);
        assert_eq!(pool.available_count(now), 2);
    }

    #[test]
    fn test_pick_skips_backing_off_peers_until_retry_time() {
        let now = Instant::now();
        let mut pool = PeerPool::default();
        pool.merge([peer(1), peer(2)]);
        pool.record_failure(peer(1), now);

        for spread_offset in 0..4 {
            assert_eq!(pool.pick(spread_offset, now), Some(peer(2)));
        }
        assert_eq!(pool.pick(0, now + BASE_RETRY_DELAY), Some(peer(1)));
    }

    #[test]
    fn test_backoff_doubles_caps_and_resets_on_success() {
        let now = Instant::now();
        let mut pool = PeerPool::default();
        pool.merge([peer(1)]);

        pool.record_failure(peer(1), now);
        pool.record_failure(peer(1), now);
        assert_eq!(pool.pick(0, now + BASE_RETRY_DELAY), None);
        assert_eq!(pool.pick(0, now + BASE_RETRY_DELAY * 2), Some(peer(1)));

        for _ in 0..20 {
            pool.record_failure(peer(1), now);
        }
        assert_eq!(pool.pick(0, now + MAX_RETRY_DELAY), Some(peer(1)));

        pool.record_success(peer(1));
        assert_eq!(pool.pick(0, now), Some(peer(1)));
    }

    #[test]
    fn test_pick_returns_none_when_every_peer_is_backing_off() {
        let now = Instant::now();
        let mut pool = PeerPool::default();
        assert_eq!(pool.pick(0, now), None);

        pool.merge([peer(1)]);
        pool.record_failure(peer(1), now);
        assert!(!pool.is_empty());
        assert_eq!(pool.pick(0, now), None);
    }
}
