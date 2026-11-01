use anyhow::{bail, Context, Result};
use sha1::{Digest, Sha1};
use std::net::SocketAddr;
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;

pub const BLOCK_SIZE: u32 = 16 * 1024;
const MIN_REQUESTS_IN_FLIGHT: usize = 8;
/// Many clients silently drop requests beyond their own queue limit (often 250, sometimes
/// less); staying well below keeps every request answered.
const MAX_REQUESTS_IN_FLIGHT: usize = 64;
/// Keep enough requests outstanding to cover this much transfer at the measured rate, so the
/// connection never idles for a round trip while waiting for new requests to arrive.
const REQUEST_QUEUE_TIME: Duration = Duration::from_secs(2);
const RATE_SAMPLE_INTERVAL: Duration = Duration::from_millis(500);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(4);
const WRITE_TIMEOUT: Duration = Duration::from_secs(5);
/// Silence allowed while blocks are outstanding before the peer counts as stalled.
const DOWNLOAD_READ_TIMEOUT: Duration = Duration::from_secs(6);
/// Silence allowed while choked: seeders rotate their unchoke slots every 10-30 seconds.
const CHOKED_READ_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_MESSAGE_LEN: usize = 2 * 1024 * 1024;

const MSG_CHOKE: u8 = 0;
const MSG_UNCHOKE: u8 = 1;
const MSG_INTERESTED: u8 = 2;
const MSG_HAVE: u8 = 4;
const MSG_BITFIELD: u8 = 5;
const MSG_REQUEST: u8 = 6;
const MSG_PIECE: u8 = 7;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BlockState {
    Missing,
    Requested,
    Received,
}

/// A piece being assembled from 16 KiB blocks, which may arrive in any order.
pub struct PieceInProgress {
    index: usize,
    expected_hash: [u8; 20],
    buffer: Vec<u8>,
    block_states: Vec<BlockState>,
    received_blocks: usize,
}

impl PieceInProgress {
    pub fn new(index: usize, length: u64, expected_hash: [u8; 20]) -> Self {
        let length = length as usize;
        let block_count = length.div_ceil(BLOCK_SIZE as usize);
        Self {
            index,
            expected_hash,
            buffer: vec![0; length],
            block_states: vec![BlockState::Missing; block_count],
            received_blocks: 0,
        }
    }

    pub fn index(&self) -> usize {
        self.index
    }

    pub fn into_bytes(self) -> Vec<u8> {
        self.buffer
    }

    /// Returns `(begin, length)` of block `block`; the last block may be shorter.
    fn block_range(&self, block: usize) -> (u32, u32) {
        let begin = block as u32 * BLOCK_SIZE;
        let length = (self.buffer.len() as u32 - begin).min(BLOCK_SIZE);
        (begin, length)
    }

    fn missing_block_count(&self) -> usize {
        self.block_states
            .iter()
            .filter(|&&state| state == BlockState::Missing)
            .count()
    }

    /// Marks the next missing block as requested and returns its `(begin, length)`.
    fn take_next_missing_block(&mut self) -> Option<(u32, u32)> {
        let block = self
            .block_states
            .iter()
            .position(|&state| state == BlockState::Missing)?;
        self.block_states[block] = BlockState::Requested;
        Some(self.block_range(block))
    }

    /// Stores a block we requested. Returns `false` for anything else (misaligned,
    /// wrong size, duplicate, or never requested), which the caller ignores.
    fn store_block(&mut self, begin: u32, data: &[u8]) -> bool {
        if !begin.is_multiple_of(BLOCK_SIZE) {
            return false;
        }
        let block = (begin / BLOCK_SIZE) as usize;
        if self.block_states.get(block) != Some(&BlockState::Requested) {
            return false;
        }
        let (_, expected_length) = self.block_range(block);
        if data.len() != expected_length as usize {
            return false;
        }
        self.buffer[begin as usize..begin as usize + data.len()].copy_from_slice(data);
        self.block_states[block] = BlockState::Received;
        self.received_blocks += 1;
        true
    }

    /// A choke discards the peer's pending requests, so they must be sent again later.
    fn forget_requests(&mut self) {
        for state in &mut self.block_states {
            if *state == BlockState::Requested {
                *state = BlockState::Missing;
            }
        }
    }

    fn is_complete(&self) -> bool {
        self.received_blocks == self.block_states.len()
    }

    fn is_hash_valid(&self) -> bool {
        Sha1::digest(&self.buffer).as_slice() == self.expected_hash
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PeerConnectionStats {
    pub addr: SocketAddr,
    pub download_bps: u64,
    pub is_choked: bool,
    pub request_queue_limit: usize,
}

pub struct PeerSession {
    addr: SocketAddr,
    stream: BufReader<TcpStream>,
    bitfield: Vec<u8>,
    is_choked: bool,
    requests_in_flight: usize,
    request_queue_limit: usize,
    download_bps: u64,
    bytes_since_sample: u64,
    sample_started_at: Instant,
}

impl PeerSession {
    /// Connects to a peer, performs the 68-byte BitTorrent handshake, and sends `Interested`.
    pub async fn connect(
        addr: SocketAddr,
        info_hash: &[u8; 20],
        peer_id: &[u8; 20],
        num_pieces: usize,
    ) -> Result<Self> {
        let mut stream = tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect(addr)).await??;

        // Handshake: <pstrlen=19><"BitTorrent protocol"><reserved:8><info_hash:20><peer_id:20>
        let mut handshake = [0u8; 68];
        handshake[0] = 19;
        handshake[1..20].copy_from_slice(b"BitTorrent protocol");
        handshake[28..48].copy_from_slice(info_hash);
        handshake[48..68].copy_from_slice(peer_id);
        tokio::time::timeout(CONNECT_TIMEOUT, stream.write_all(&handshake)).await??;

        let mut response = [0u8; 68];
        tokio::time::timeout(CONNECT_TIMEOUT, stream.read_exact(&mut response)).await??;
        if response[0] != 19 || &response[1..20] != b"BitTorrent protocol" {
            bail!("Invalid BitTorrent handshake header from peer {}", addr);
        }
        if &response[28..48] != info_hash {
            bail!("Peer info_hash mismatch from {}", addr);
        }

        let interested = [0, 0, 0, 1, MSG_INTERESTED];
        tokio::time::timeout(CONNECT_TIMEOUT, stream.write_all(&interested)).await??;

        Ok(Self {
            addr,
            stream: BufReader::new(stream),
            // Optimistically assume every piece until the peer's Bitfield arrives.
            bitfield: vec![0xFF; num_pieces.div_ceil(8)],
            is_choked: true,
            requests_in_flight: 0,
            request_queue_limit: MIN_REQUESTS_IN_FLIGHT,
            download_bps: 0,
            bytes_since_sample: 0,
            sample_started_at: Instant::now(),
        })
    }

    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    pub fn has_piece(&self, piece_index: usize) -> bool {
        let byte_index = piece_index / 8;
        let bit_index = 7 - (piece_index % 8);
        match self.bitfield.get(byte_index) {
            Some(byte) => byte & (1 << bit_index) != 0,
            None => true,
        }
    }

    pub fn stats(&self) -> PeerConnectionStats {
        PeerConnectionStats {
            addr: self.addr,
            download_bps: self.download_bps,
            is_choked: self.is_choked,
            request_queue_limit: self.request_queue_limit,
        }
    }

    /// Whether the request pipeline has room that the active pieces cannot fill, meaning the
    /// caller should add another piece before downloading.
    pub fn wants_more_pieces(&self, active_pieces: &[PieceInProgress]) -> bool {
        let missing_blocks: usize = active_pieces
            .iter()
            .map(PieceInProgress::missing_block_count)
            .sum();
        self.requests_in_flight + missing_blocks < self.request_queue_limit
    }

    /// Downloads until one of `active_pieces` is complete and SHA-1 verified, keeping up to
    /// `request_queue_limit` block requests outstanding across all of them so the connection
    /// never idles between pieces. The finished piece is removed from `active_pieces`; on error
    /// every unfinished piece is left there for the caller to release.
    pub async fn download_next_piece(
        &mut self,
        active_pieces: &mut Vec<PieceInProgress>,
    ) -> Result<PieceInProgress> {
        if active_pieces.is_empty() {
            bail!("no pieces to download from peer {}", self.addr);
        }
        loop {
            if !self.is_choked {
                self.send_pending_requests(active_pieces).await?;
            }
            let read_timeout = if self.is_choked {
                CHOKED_READ_TIMEOUT
            } else {
                DOWNLOAD_READ_TIMEOUT
            };
            let message = self.read_message(read_timeout).await?;
            let payload = &message[1..];
            match message[0] {
                MSG_CHOKE => {
                    self.is_choked = true;
                    self.requests_in_flight = 0;
                    active_pieces
                        .iter_mut()
                        .for_each(PieceInProgress::forget_requests);
                }
                MSG_UNCHOKE => {
                    self.is_choked = false;
                    self.bytes_since_sample = 0;
                    self.sample_started_at = Instant::now();
                }
                MSG_HAVE => self.mark_have(payload),
                MSG_BITFIELD => self.bitfield = payload.to_vec(),
                MSG_PIECE => {
                    let Some(position) = self.store_block(payload, active_pieces)? else {
                        continue;
                    };
                    if !active_pieces[position].is_complete() {
                        continue;
                    }
                    if !active_pieces[position].is_hash_valid() {
                        bail!(
                            "piece {} from peer {} failed SHA-1 verification",
                            active_pieces[position].index,
                            self.addr
                        );
                    }
                    return Ok(active_pieces.swap_remove(position));
                }
                _ => {}
            }
        }
    }

    /// Requests missing blocks across the active pieces, in piece order, until the queue limit
    /// is reached, batching them into a single write.
    async fn send_pending_requests(&mut self, active_pieces: &mut [PieceInProgress]) -> Result<()> {
        let mut request_bytes = Vec::new();
        for piece in active_pieces.iter_mut() {
            while self.requests_in_flight < self.request_queue_limit {
                let Some((begin, length)) = piece.take_next_missing_block() else {
                    break;
                };
                // Request: <len=13><id=6><index:4><begin:4><length:4>
                request_bytes.extend_from_slice(&13u32.to_be_bytes());
                request_bytes.push(MSG_REQUEST);
                request_bytes.extend_from_slice(&(piece.index as u32).to_be_bytes());
                request_bytes.extend_from_slice(&begin.to_be_bytes());
                request_bytes.extend_from_slice(&length.to_be_bytes());
                self.requests_in_flight += 1;
            }
        }
        if !request_bytes.is_empty() {
            tokio::time::timeout(WRITE_TIMEOUT, self.stream.write_all(&request_bytes))
                .await
                .context("timed out sending block requests")??;
        }
        Ok(())
    }

    /// Stores a received block and returns the position of its piece in `active_pieces`,
    /// or `None` when the block was not one we are waiting for.
    fn store_block(
        &mut self,
        payload: &[u8],
        active_pieces: &mut [PieceInProgress],
    ) -> Result<Option<usize>> {
        // Piece: <index:4><begin:4><block...>
        if payload.len() < 8 {
            bail!("Malformed Piece message from peer {}", self.addr);
        }
        let piece_index = u32::from_be_bytes(payload[0..4].try_into()?) as usize;
        let begin = u32::from_be_bytes(payload[4..8].try_into()?);
        let data = &payload[8..];

        let Some(position) = active_pieces
            .iter()
            .position(|piece| piece.index == piece_index)
        else {
            return Ok(None);
        };
        if !active_pieces[position].store_block(begin, data) {
            return Ok(None);
        }
        self.requests_in_flight = self.requests_in_flight.saturating_sub(1);
        self.record_received_bytes(data.len());
        Ok(Some(position))
    }

    fn record_received_bytes(&mut self, byte_count: usize) {
        self.bytes_since_sample += byte_count as u64;
        let elapsed = self.sample_started_at.elapsed();
        if elapsed < RATE_SAMPLE_INTERVAL {
            return;
        }
        let sample_bps = (self.bytes_since_sample as f64 / elapsed.as_secs_f64()) as u64;
        self.download_bps = if self.download_bps == 0 {
            sample_bps
        } else {
            (self.download_bps + sample_bps) / 2
        };
        self.request_queue_limit = request_queue_limit_for(self.download_bps);
        self.bytes_since_sample = 0;
        self.sample_started_at = Instant::now();
    }

    fn mark_have(&mut self, payload: &[u8]) {
        let Some(index_bytes) = payload.get(0..4) else {
            return;
        };
        let piece_index = u32::from_be_bytes([
            index_bytes[0],
            index_bytes[1],
            index_bytes[2],
            index_bytes[3],
        ]) as usize;
        if let Some(byte) = self.bitfield.get_mut(piece_index / 8) {
            *byte |= 1 << (7 - (piece_index % 8));
        }
    }

    /// Reads the next non-keep-alive message and returns its body, starting with the id byte.
    async fn read_message(&mut self, read_timeout: Duration) -> Result<Vec<u8>> {
        loop {
            let mut length_bytes = [0u8; 4];
            tokio::time::timeout(read_timeout, self.stream.read_exact(&mut length_bytes))
                .await
                .with_context(|| format!("peer {} stopped responding", self.addr))??;
            let length = u32::from_be_bytes(length_bytes) as usize;
            if length == 0 {
                continue;
            }
            if length > MAX_MESSAGE_LEN {
                bail!("Peer message too large: {} bytes", length);
            }
            let mut body = vec![0u8; length];
            tokio::time::timeout(read_timeout, self.stream.read_exact(&mut body))
                .await
                .with_context(|| format!("peer {} stopped responding", self.addr))??;
            return Ok(body);
        }
    }
}

/// Number of block requests to keep outstanding for a connection downloading at
/// `download_bps`: enough to cover `REQUEST_QUEUE_TIME`, within the min/max bounds.
fn request_queue_limit_for(download_bps: u64) -> usize {
    let bytes_to_cover = download_bps as f64 * REQUEST_QUEUE_TIME.as_secs_f64();
    let blocks = (bytes_to_cover / f64::from(BLOCK_SIZE)) as usize;
    blocks.clamp(MIN_REQUESTS_IN_FLIGHT, MAX_REQUESTS_IN_FLIGHT)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::sync::Arc;
    use tokio::net::TcpListener;
    use tokio::sync::mpsc;

    #[test]
    fn test_request_queue_limit_scales_with_rate_within_bounds() {
        assert_eq!(request_queue_limit_for(0), MIN_REQUESTS_IN_FLIGHT);
        // 256 KiB/s for 2 s = 512 KiB = 32 blocks
        assert_eq!(request_queue_limit_for(256 * 1024), 32);
        assert_eq!(request_queue_limit_for(100 * 1024 * 1024), MAX_REQUESTS_IN_FLIGHT);
    }

    #[test]
    fn test_piece_accepts_requested_blocks_in_any_order_and_rejects_others() {
        let data: Vec<u8> = (0..40_000u32).map(|i| (i % 251) as u8).collect();
        let hash: [u8; 20] = Sha1::digest(&data).into();
        let mut piece = PieceInProgress::new(3, data.len() as u64, hash);

        assert_eq!(piece.take_next_missing_block(), Some((0, BLOCK_SIZE)));
        assert_eq!(piece.take_next_missing_block(), Some((BLOCK_SIZE, BLOCK_SIZE)));
        assert_eq!(piece.take_next_missing_block(), Some((2 * BLOCK_SIZE, 40_000 - 2 * BLOCK_SIZE)));
        assert_eq!(piece.take_next_missing_block(), None);

        let block = |begin: u32, length: u32| &data[begin as usize..(begin + length) as usize];
        assert!(!piece.store_block(1, block(1, 10)), "misaligned block");
        assert!(piece.store_block(2 * BLOCK_SIZE, block(2 * BLOCK_SIZE, 40_000 - 2 * BLOCK_SIZE)));
        assert!(!piece.store_block(2 * BLOCK_SIZE, block(2 * BLOCK_SIZE, 40_000 - 2 * BLOCK_SIZE)), "duplicate");
        assert!(piece.store_block(0, block(0, BLOCK_SIZE)));
        assert!(!piece.is_complete());
        assert!(piece.store_block(BLOCK_SIZE, block(BLOCK_SIZE, BLOCK_SIZE)));
        assert!(piece.is_complete());
        assert!(piece.is_hash_valid());
    }

    #[test]
    fn test_choke_makes_outstanding_blocks_requestable_again() {
        let mut piece = PieceInProgress::new(0, u64::from(BLOCK_SIZE) * 2, [0; 20]);
        piece.take_next_missing_block();
        piece.take_next_missing_block();
        assert_eq!(piece.missing_block_count(), 0);

        piece.forget_requests();
        assert_eq!(piece.missing_block_count(), 2);
        assert_eq!(piece.take_next_missing_block(), Some((0, BLOCK_SIZE)));
    }

    /// A seeder that answers every request after `latency`, independently of other requests,
    /// like a fast peer on a long-distance link.
    async fn spawn_latency_seeder(
        data: Arc<Vec<u8>>,
        piece_length: u64,
        info_hash: [u8; 20],
        latency: Duration,
    ) -> SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut handshake = [0u8; 68];
            stream.read_exact(&mut handshake).await.unwrap();
            handshake[28..48].copy_from_slice(&info_hash);
            stream.write_all(&handshake).await.unwrap();
            stream.write_all(&[0, 0, 0, 1, MSG_UNCHOKE]).await.unwrap();

            let (mut reader, mut writer) = stream.into_split();
            let (outgoing_tx, mut outgoing_rx) = mpsc::unbounded_channel::<Vec<u8>>();
            tokio::spawn(async move {
                while let Some(message) = outgoing_rx.recv().await {
                    if writer.write_all(&message).await.is_err() {
                        break;
                    }
                }
            });
            loop {
                let mut length_bytes = [0u8; 4];
                if reader.read_exact(&mut length_bytes).await.is_err() {
                    break;
                }
                let mut body = vec![0u8; u32::from_be_bytes(length_bytes) as usize];
                if reader.read_exact(&mut body).await.is_err() {
                    break;
                }
                if body.first() != Some(&MSG_REQUEST) {
                    continue;
                }
                let index = u32::from_be_bytes(body[1..5].try_into().unwrap()) as u64;
                let begin = u32::from_be_bytes(body[5..9].try_into().unwrap()) as u64;
                let length = u32::from_be_bytes(body[9..13].try_into().unwrap()) as usize;
                let offset = (index * piece_length + begin) as usize;
                let mut message = ((9 + length) as u32).to_be_bytes().to_vec();
                message.push(MSG_PIECE);
                message.extend_from_slice(&body[1..9]);
                message.extend_from_slice(&data[offset..offset + length]);
                let outgoing_tx = outgoing_tx.clone();
                tokio::spawn(async move {
                    tokio::time::sleep(latency).await;
                    let _ = outgoing_tx.send(message);
                });
            }
        });
        addr
    }

    #[tokio::test]
    async fn test_pipeline_deepens_on_high_latency_link() {
        const PIECE_LENGTH: u64 = 256 * 1024;
        const PIECE_COUNT: usize = 16;
        let latency = Duration::from_millis(100);
        let data: Arc<Vec<u8>> = Arc::new(
            (0..PIECE_LENGTH as usize * PIECE_COUNT)
                .map(|i| (i * 31 % 251) as u8)
                .collect(),
        );
        let piece_hashes: Vec<[u8; 20]> = data
            .chunks(PIECE_LENGTH as usize)
            .map(|chunk| Sha1::digest(chunk).into())
            .collect();
        let info_hash = [9u8; 20];
        let addr = spawn_latency_seeder(data.clone(), PIECE_LENGTH, info_hash, latency).await;

        let started_at = Instant::now();
        let mut session = PeerSession::connect(addr, &info_hash, &[1u8; 20], PIECE_COUNT)
            .await
            .unwrap();
        let mut pending_pieces: VecDeque<usize> = (0..PIECE_COUNT).collect();
        let mut active_pieces = Vec::new();
        let mut finished_pieces = 0;
        while finished_pieces < PIECE_COUNT {
            while session.wants_more_pieces(&active_pieces) {
                let Some(index) = pending_pieces.pop_front() else {
                    break;
                };
                active_pieces.push(PieceInProgress::new(index, PIECE_LENGTH, piece_hashes[index]));
            }
            let piece = session.download_next_piece(&mut active_pieces).await.unwrap();
            let offset = piece.index() * PIECE_LENGTH as usize;
            assert_eq!(piece.into_bytes(), data[offset..offset + PIECE_LENGTH as usize]);
            finished_pieces += 1;
        }
        let elapsed = started_at.elapsed();

        // With the old fixed 8 requests in flight this needs 256 blocks / 8 * 100 ms = 3.2 s.
        assert!(
            elapsed < Duration::from_secs(2),
            "4 MiB over a 100 ms link took {elapsed:?}"
        );
        assert_eq!(session.stats().request_queue_limit, MAX_REQUESTS_IN_FLIGHT);
    }
}
