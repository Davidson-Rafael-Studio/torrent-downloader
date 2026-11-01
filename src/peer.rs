use anyhow::{bail, Result};
use sha1::{Digest, Sha1};
use std::net::SocketAddr;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

pub const BLOCK_SIZE: u32 = 16 * 1024; // 16 KiB standard BitTorrent block size

pub struct PeerSession {
    stream: TcpStream,
    pub bitfield: Vec<u8>,
    pub choked: bool,
}

impl PeerSession {
    /// Connects to a peer, performs the 68-byte BitTorrent handshake, and sends `Interested`.
    pub async fn connect(
        addr: SocketAddr,
        info_hash: &[u8; 20],
        peer_id: &[u8; 20],
        num_pieces: usize,
    ) -> Result<Self> {
        let mut stream =
            tokio::time::timeout(Duration::from_secs(4), TcpStream::connect(addr)).await??;

        // Build 68-byte handshake
        let mut hs = [0u8; 68];
        hs[0] = 19;
        hs[1..20].copy_from_slice(b"BitTorrent protocol");
        // hs[20..28] reserved zeros
        hs[28..48].copy_from_slice(info_hash);
        hs[48..68].copy_from_slice(peer_id);

        tokio::time::timeout(Duration::from_secs(4), stream.write_all(&hs)).await??;

        let mut resp = [0u8; 68];
        tokio::time::timeout(Duration::from_secs(4), stream.read_exact(&mut resp)).await??;

        if resp[0] != 19 || &resp[1..20] != b"BitTorrent protocol" {
            bail!("Invalid BitTorrent handshake header from peer {}", addr);
        }
        if &resp[28..48] != info_hash {
            bail!("Peer info_hash mismatch from {}", addr);
        }

        // Send Interested (length = 1, id = 2)
        let interested_msg = [0u8, 0, 0, 1, 2];
        tokio::time::timeout(Duration::from_secs(4), stream.write_all(&interested_msg)).await??;

        let bitfield_len = num_pieces.div_ceil(8);
        Ok(Self {
            stream,
            bitfield: vec![0xFF; bitfield_len], // optimistic until Bitfield received
            choked: true,
        })
    }

    pub fn has_piece(&self, piece_index: usize) -> bool {
        let byte_idx = piece_index / 8;
        let bit_idx = 7 - (piece_index % 8);
        if byte_idx >= self.bitfield.len() {
            return true;
        }
        (self.bitfield[byte_idx] & (1 << bit_idx)) != 0
    }

    /// Downloads and verifies a single piece from the connected peer.
    pub async fn download_piece(
        &mut self,
        piece_index: usize,
        piece_len: u64,
        expected_hash: &[u8; 20],
    ) -> Result<Vec<u8>> {
        // Wait until unchoked
        let mut wait_attempts = 0;
        while self.choked {
            if wait_attempts > 15 {
                bail!("Peer remained choked");
            }
            self.read_next_control_message().await?;
            wait_attempts += 1;
        }

        let total_len = piece_len as u32;
        let mut piece_buf = vec![0u8; total_len as usize];
        let mut received = 0u32;

        // Pipeline block requests in batches of up to 8 blocks
        let mut next_offset = 0u32;
        let mut in_flight = 0usize;

        while received < total_len {
            while next_offset < total_len && in_flight < 8 {
                let block_len = (total_len - next_offset).min(BLOCK_SIZE);
                self.send_request(piece_index as u32, next_offset, block_len)
                    .await?;
                next_offset += block_len;
                in_flight += 1;
            }

            let (msg_id, payload) = self.read_raw_message().await?;
            match msg_id {
                0 => {
                    self.choked = true;
                    bail!("Peer choked mid-piece");
                }
                1 => {
                    self.choked = false;
                }
                4 => {
                    // Have
                    if payload.len() >= 4 {
                        let idx = u32::from_be_bytes(payload[0..4].try_into()?) as usize;
                        let byte_idx = idx / 8;
                        let bit_idx = 7 - (idx % 8);
                        if byte_idx < self.bitfield.len() {
                            self.bitfield[byte_idx] |= 1 << bit_idx;
                        }
                    }
                }
                5 => {
                    // Bitfield
                    self.bitfield = payload;
                }
                7 => {
                    // Piece: <index:4><begin:4><block...>
                    if payload.len() < 8 {
                        bail!("Malformed Piece message");
                    }
                    let idx = u32::from_be_bytes(payload[0..4].try_into()?);
                    let begin = u32::from_be_bytes(payload[4..8].try_into()?);
                    let data = &payload[8..];
                    if idx == piece_index as u32
                        && (begin as usize + data.len()) <= piece_buf.len()
                    {
                        piece_buf[begin as usize..begin as usize + data.len()]
                            .copy_from_slice(data);
                        received += data.len() as u32;
                        in_flight = in_flight.saturating_sub(1);
                    }
                }
                _ => {}
            }
        }

        // Verify SHA-1 hash of the assembled piece
        let mut hasher = Sha1::new();
        hasher.update(&piece_buf);
        let digest = hasher.finalize();
        if digest.as_slice() != expected_hash {
            bail!("Piece {} failed SHA-1 verification", piece_index);
        }

        Ok(piece_buf)
    }

    async fn send_request(&mut self, index: u32, begin: u32, length: u32) -> Result<()> {
        let mut buf = [0u8; 17];
        buf[0..4].copy_from_slice(&13u32.to_be_bytes());
        buf[4] = 6; // Request message ID
        buf[5..9].copy_from_slice(&index.to_be_bytes());
        buf[9..13].copy_from_slice(&begin.to_be_bytes());
        buf[13..17].copy_from_slice(&length.to_be_bytes());
        tokio::time::timeout(Duration::from_secs(5), self.stream.write_all(&buf)).await??;
        Ok(())
    }

    async fn read_next_control_message(&mut self) -> Result<()> {
        let (msg_id, payload) = self.read_raw_message().await?;
        match msg_id {
            0 => self.choked = true,
            1 => self.choked = false,
            4 => {
                if payload.len() >= 4 {
                    let idx = u32::from_be_bytes(payload[0..4].try_into()?) as usize;
                    let byte_idx = idx / 8;
                    let bit_idx = 7 - (idx % 8);
                    if byte_idx < self.bitfield.len() {
                        self.bitfield[byte_idx] |= 1 << bit_idx;
                    }
                }
            }
            5 => self.bitfield = payload,
            _ => {}
        }
        Ok(())
    }

    async fn read_raw_message(&mut self) -> Result<(u8, Vec<u8>)> {
        loop {
            let mut len_buf = [0u8; 4];
            tokio::time::timeout(Duration::from_secs(6), self.stream.read_exact(&mut len_buf))
                .await??;
            let len = u32::from_be_bytes(len_buf);
            if len == 0 {
                // Keep-alive message
                continue;
            }
            if len > 2 * 1024 * 1024 {
                bail!("Peer message too large: {} bytes", len);
            }
            let mut body = vec![0u8; len as usize];
            tokio::time::timeout(Duration::from_secs(8), self.stream.read_exact(&mut body))
                .await??;
            let msg_id = body[0];
            let payload = body[1..].to_vec();
            return Ok((msg_id, payload));
        }
    }
}
