use crate::torrent::create_sample_torrent_bytes;
use anyhow::Result;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::RwLock;

#[derive(Clone)]
struct SeededTorrent {
    info_hash: [u8; 20],
    piece_length: u64,
    num_pieces: usize,
    raw_data: Arc<Vec<u8>>,
}

/// A lightweight embedded BitTorrent TCP peer + HTTP tracker used when generating
/// sample game `.torrent` files so the user can test multi-threaded BitTorrent
/// downloading, pause/resume, and Steam installation out of the box.
pub struct LocalTestSwarm {
    pub peer_addr: SocketAddr,
    pub tracker_url: String,
    torrents: Arc<RwLock<HashMap<[u8; 20], SeededTorrent>>>,
}

impl LocalTestSwarm {
    pub async fn start() -> Result<Arc<Self>> {
        let peer_listener = TcpListener::bind("127.0.0.1:0").await?;
        let peer_addr = peer_listener.local_addr()?;

        let tracker_listener = TcpListener::bind("127.0.0.1:0").await?;
        let tracker_addr = tracker_listener.local_addr()?;
        let tracker_url = format!("http://{}/announce", tracker_addr);

        let torrents: Arc<RwLock<HashMap<[u8; 20], SeededTorrent>>> =
            Arc::new(RwLock::new(HashMap::new()));

        // Spawn tiny HTTP tracker responder that returns `peer_addr` in compact bencode format
        let peer_ip_port = match peer_addr {
            SocketAddr::V4(v4) => {
                let mut bytes = [0u8; 6];
                bytes[0..4].copy_from_slice(&v4.ip().octets());
                bytes[4..6].copy_from_slice(&v4.port().to_be_bytes());
                bytes
            }
            _ => [127, 0, 0, 1, 0, 0],
        };

        tokio::spawn(async move {
            loop {
                if let Ok((mut socket, _)) = tracker_listener.accept().await {
                    tokio::spawn(async move {
                        let mut req_buf = [0u8; 1024];
                        let _ = socket.read(&mut req_buf).await;
                        // Bencoded response: d8:intervali60e5:peers6:<6-bytes>e
                        let mut body = b"d8:intervali60e5:peers6:".to_vec();
                        body.extend_from_slice(&peer_ip_port);
                        body.push(b'e');

                        let header = format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                            body.len()
                        );
                        let _ = socket.write_all(header.as_bytes()).await;
                        let _ = socket.write_all(&body).await;
                    });
                }
            }
        });

        // Spawn BitTorrent Peer Wire protocol seeder
        let torrents_ref = torrents.clone();
        tokio::spawn(async move {
            loop {
                if let Ok((mut stream, _)) = peer_listener.accept().await {
                    let torrents_map = torrents_ref.clone();
                    tokio::spawn(async move {
                        let mut hs = [0u8; 68];
                        if stream.read_exact(&mut hs).await.is_err() {
                            return;
                        }
                        if hs[0] != 19 || &hs[1..20] != b"BitTorrent protocol" {
                            return;
                        }
                        let mut info_hash = [0u8; 20];
                        info_hash.copy_from_slice(&hs[28..48]);

                        let seeded = {
                            let map = torrents_map.read().await;
                            map.get(&info_hash).cloned()
                        };
                        let Some(seeded) = seeded else {
                            return;
                        };

                        // Reply with handshake
                        let mut resp_hs = [0u8; 68];
                        resp_hs[0] = 19;
                        resp_hs[1..20].copy_from_slice(b"BitTorrent protocol");
                        resp_hs[28..48].copy_from_slice(&seeded.info_hash);
                        resp_hs[48..68].copy_from_slice(b"-TD0100-LOCAL-SEEDER");
                        if stream.write_all(&resp_hs).await.is_err() {
                            return;
                        }

                        // Send bitfield (all 1s)
                        let bf_len = seeded.num_pieces.div_ceil(8);
                        let mut bf_msg = Vec::with_capacity(5 + bf_len);
                        bf_msg.extend_from_slice(&((1 + bf_len) as u32).to_be_bytes());
                        bf_msg.push(5); // Bitfield ID
                        bf_msg.extend(std::iter::repeat_n(0xFFu8, bf_len));
                        let _ = stream.write_all(&bf_msg).await;

                        loop {
                            let mut len_buf = [0u8; 4];
                            if stream.read_exact(&mut len_buf).await.is_err() {
                                break;
                            }
                            let msg_len = u32::from_be_bytes(len_buf);
                            if msg_len == 0 {
                                continue;
                            }
                            let mut payload = vec![0u8; msg_len as usize];
                            if stream.read_exact(&mut payload).await.is_err() {
                                break;
                            }
                            match payload[0] {
                                2 => {
                                    // Interested -> send Unchoke (len=1, id=1)
                                    let unchoke = [0u8, 0, 0, 1, 1];
                                    let _ = stream.write_all(&unchoke).await;
                                }
                                6 if payload.len() >= 13 => {
                                    // Request: <index:4><begin:4><length:4>
                                    let index =
                                        u32::from_be_bytes(payload[1..5].try_into().unwrap())
                                            as u64;
                                    let begin =
                                        u32::from_be_bytes(payload[5..9].try_into().unwrap())
                                            as u64;
                                    let length =
                                        u32::from_be_bytes(payload[9..13].try_into().unwrap())
                                            as usize;

                                    // Slight throttle so the user can observe multi-threaded progress,
                                    // speed meter, pause, and resume in real time!
                                    tokio::time::sleep(std::time::Duration::from_millis(28)).await;

                                    let global_offset =
                                        (index * seeded.piece_length + begin) as usize;
                                    if global_offset + length <= seeded.raw_data.len() {
                                        let block =
                                            &seeded.raw_data[global_offset..global_offset + length];
                                        let resp_len = (9 + block.len()) as u32;
                                        let mut piece_msg =
                                            Vec::with_capacity(4 + resp_len as usize);
                                        piece_msg.extend_from_slice(&resp_len.to_be_bytes());
                                        piece_msg.push(7); // Piece ID
                                        piece_msg.extend_from_slice(&payload[1..9]); // index + begin
                                        piece_msg.extend_from_slice(block);
                                        if stream.write_all(&piece_msg).await.is_err() {
                                            break;
                                        }
                                    }
                                }
                                _ => {}
                            }
                        }
                    });
                }
            }
        });

        Ok(Arc::new(Self {
            peer_addr,
            tracker_url,
            torrents,
        }))
    }

    /// Creates a sample game `.torrent` file on disk and registers its payload in the local swarm
    /// so it can be downloaded via real multi-threaded BitTorrent TCP connections.
    pub async fn create_and_seed_sample_game(
        &self,
        output_dir: &Path,
        game_name: &str,
        size_mb: usize,
    ) -> Result<PathBuf> {
        std::fs::create_dir_all(output_dir)?;
        let piece_length = 64 * 1024u64; // 64 KiB pieces for smooth progress visualization

        // Create realistic game files (executable + data pack + readme)
        let exe_name = format!("{}.exe", game_name);
        let app_script = format!("{}.sh", game_name);
        let exe_bytes = format!(
            "#!/bin/sh\necho 'Launching {} from Steam library!'\n",
            game_name
        )
        .into_bytes();

        let pack_size = (size_mb.max(1) * 1024 * 1024).saturating_sub(exe_bytes.len() + 256);
        let mut assets_pack = vec![0u8; pack_size];
        for (i, b) in assets_pack.iter_mut().enumerate() {
            *b = ((i * 31 + 17) & 0xFF) as u8;
        }
        let readme_bytes = format!(
            "Game: {}\nInstalled in Steam steamapps/common/{}\nReady to add/play in Steam!\n",
            game_name, game_name
        )
        .into_bytes();

        let files: Vec<(&str, &[u8])> = vec![
            (&exe_name, &exe_bytes),
            (&app_script, &exe_bytes),
            ("data/assets_01.pak", &assets_pack),
            ("README_STEAM.txt", &readme_bytes),
        ];

        let mut combined = Vec::new();
        for (_, data) in &files {
            combined.extend_from_slice(data);
        }

        let torrent_bytes =
            create_sample_torrent_bytes(game_name, &files, piece_length, &self.tracker_url)?;
        let torrent_path = output_dir.join(format!("{}.torrent", game_name));
        std::fs::write(&torrent_path, &torrent_bytes)?;

        let meta = crate::torrent::TorrentMeta::from_bytes(&torrent_bytes, torrent_path.clone())?;
        let seeded = SeededTorrent {
            info_hash: meta.info_hash,
            piece_length,
            num_pieces: meta.pieces.len(),
            raw_data: Arc::new(combined),
        };

        self.torrents.write().await.insert(meta.info_hash, seeded);
        Ok(torrent_path)
    }
}
