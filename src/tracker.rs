use crate::torrent::TorrentMeta;
use anyhow::{bail, Context, Result};
use rand::Rng;
use serde::Deserialize;
use std::collections::HashSet;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::time::Duration;
use tokio::net::UdpSocket;

/// Peers requested per announce. Trackers default to ~50; asking for more gives the job
/// more candidates when many of them turn out to be unreachable.
const PEERS_WANTED: u32 = 200;

#[derive(Debug, Deserialize)]
struct TrackerResponse {
    #[serde(rename = "failure reason", default)]
    failure_reason: Option<String>,
    #[serde(default)]
    interval: Option<i64>,
    #[serde(default)]
    peers: Option<serde_bencode::value::Value>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AnnounceEvent {
    /// First announce of this download.
    Started,
    /// Regular re-announce to refresh the peer list.
    Periodic,
}

#[derive(Debug, Clone, Copy)]
struct AnnounceRequest {
    info_hash: [u8; 20],
    peer_id: [u8; 20],
    port: u16,
    left: u64,
    event: AnnounceEvent,
}

#[derive(Debug, Default)]
pub struct AnnounceResponse {
    pub peers: Vec<SocketAddr>,
    /// Shortest re-announce interval any tracker asked for.
    pub interval: Option<Duration>,
}

/// Queries all trackers in `meta.trackers` concurrently (HTTP/HTTPS and UDP) and returns
/// the deduplicated peers plus the shortest re-announce interval reported.
pub async fn discover_peers(
    meta: &TorrentMeta,
    peer_id: &[u8; 20],
    listen_port: u16,
    event: AnnounceEvent,
) -> AnnounceResponse {
    let request = AnnounceRequest {
        info_hash: meta.info_hash,
        peer_id: *peer_id,
        port: listen_port,
        left: meta.total_length,
        event,
    };

    let mut handles = Vec::with_capacity(meta.trackers.len());
    for tracker_url in &meta.trackers {
        let url = tracker_url.clone();
        handles.push(tokio::spawn(async move {
            if url.starts_with("http://") || url.starts_with("https://") {
                announce_http(&url, request).await
            } else if url.starts_with("udp://") {
                announce_udp(&url, request).await
            } else {
                bail!("Unsupported tracker scheme: {}", url)
            }
        }));
    }

    let mut combined = AnnounceResponse::default();
    let mut seen_peers = HashSet::new();
    for handle in handles {
        let Ok(Ok(response)) = handle.await else {
            continue;
        };
        combined.peers.extend(
            response
                .peers
                .into_iter()
                .filter(|addr| seen_peers.insert(*addr)),
        );
        combined.interval = match (combined.interval, response.interval) {
            (Some(current), Some(reported)) => Some(current.min(reported)),
            (current, reported) => current.or(reported),
        };
    }
    combined
}

async fn announce_http(tracker_url: &str, request: AnnounceRequest) -> Result<AnnounceResponse> {
    let encoded_hash = urlencoding::encode_binary(&request.info_hash);
    let encoded_peer_id = urlencoding::encode_binary(&request.peer_id);
    let separator = if tracker_url.contains('?') { '&' } else { '?' };
    let event_param = match request.event {
        AnnounceEvent::Started => "&event=started",
        AnnounceEvent::Periodic => "",
    };

    let full_url = format!(
        "{}{}info_hash={}&peer_id={}&port={}&uploaded=0&downloaded=0&left={}&compact=1&numwant={}{}",
        tracker_url,
        separator,
        encoded_hash,
        encoded_peer_id,
        request.port,
        request.left,
        PEERS_WANTED,
        event_param
    );

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(6))
        .build()?;

    let resp_bytes = client
        .get(&full_url)
        .send()
        .await?
        .bytes()
        .await?;

    let parsed: TrackerResponse = serde_bencode::from_bytes(&resp_bytes)
        .context("Failed to decode tracker bencode response")?;

    if let Some(reason) = parsed.failure_reason {
        bail!("Tracker returned failure: {}", reason);
    }

    let mut addrs = Vec::new();
    if let Some(peers_val) = parsed.peers {
        match peers_val {
            // Compact peer format: 6 bytes per peer (4-byte IPv4 + 2-byte big-endian port)
            serde_bencode::value::Value::Bytes(bytes) => {
                for chunk in bytes.chunks_exact(6) {
                    let ip = Ipv4Addr::new(chunk[0], chunk[1], chunk[2], chunk[3]);
                    let p = u16::from_be_bytes([chunk[4], chunk[5]]);
                    if p > 0 {
                        addrs.push(SocketAddr::V4(SocketAddrV4::new(ip, p)));
                    }
                }
            }
            // Dictionary peer format
            serde_bencode::value::Value::List(list) => {
                for item in list {
                    if let serde_bencode::value::Value::Dict(dict) = item {
                        let ip_str = dict.get(b"ip".as_ref()).and_then(|v| match v {
                            serde_bencode::value::Value::Bytes(b) => std::str::from_utf8(b).ok(),
                            _ => None,
                        });
                        let port_val = dict.get(b"port".as_ref()).and_then(|v| match v {
                            serde_bencode::value::Value::Int(i) => Some(*i as u16),
                            _ => None,
                        });
                        if let (Some(ip_s), Some(p)) = (ip_str, port_val) {
                            if let Ok(ip) = ip_s.parse() {
                                addrs.push(SocketAddr::new(ip, p));
                            }
                        }
                    }
                }
            }
            _ => {}
        }
    }

    Ok(AnnounceResponse {
        peers: addrs,
        interval: parsed
            .interval
            .and_then(|secs| u64::try_from(secs).ok())
            .map(Duration::from_secs),
    })
}

/// BEP 15 UDP Tracker Protocol
async fn announce_udp(tracker_url: &str, request: AnnounceRequest) -> Result<AnnounceResponse> {
    let stripped = tracker_url
        .strip_prefix("udp://")
        .context("Not a udp:// URL")?;
    let host_port = stripped.split('/').next().unwrap_or(stripped);

    let socket = UdpSocket::bind("0.0.0.0:0").await?;
    tokio::time::timeout(Duration::from_secs(4), socket.connect(host_port)).await??;

    let tx_id: u32 = rand::thread_rng().gen();
    let protocol_id: u64 = 0x41727101980; // BEP 15 magic constant

    let mut connect_req = [0u8; 16];
    connect_req[0..8].copy_from_slice(&protocol_id.to_be_bytes());
    connect_req[8..12].copy_from_slice(&0u32.to_be_bytes()); // action = 0 (connect)
    connect_req[12..16].copy_from_slice(&tx_id.to_be_bytes());

    socket.send(&connect_req).await?;

    let mut connect_resp = [0u8; 16];
    let n = tokio::time::timeout(Duration::from_secs(4), socket.recv(&mut connect_resp)).await??;
    if n < 16 {
        bail!("UDP tracker connect response too short");
    }

    let resp_action = u32::from_be_bytes(connect_resp[0..4].try_into()?);
    let resp_tx = u32::from_be_bytes(connect_resp[4..8].try_into()?);
    if resp_action != 0 || resp_tx != tx_id {
        bail!("UDP tracker connect transaction mismatch");
    }
    let conn_id = u64::from_be_bytes(connect_resp[8..16].try_into()?);

    // Build announce request (98 bytes)
    let mut ann_req = [0u8; 98];
    let ann_tx: u32 = rand::thread_rng().gen();
    ann_req[0..8].copy_from_slice(&conn_id.to_be_bytes());
    ann_req[8..12].copy_from_slice(&1u32.to_be_bytes()); // action = 1 (announce)
    ann_req[12..16].copy_from_slice(&ann_tx.to_be_bytes());
    ann_req[16..36].copy_from_slice(&request.info_hash);
    ann_req[36..56].copy_from_slice(&request.peer_id);
    ann_req[56..64].copy_from_slice(&0u64.to_be_bytes()); // downloaded
    ann_req[64..72].copy_from_slice(&request.left.to_be_bytes()); // left
    ann_req[72..80].copy_from_slice(&0u64.to_be_bytes()); // uploaded
    let event_code: u32 = match request.event {
        AnnounceEvent::Started => 2,
        AnnounceEvent::Periodic => 0,
    };
    ann_req[80..84].copy_from_slice(&event_code.to_be_bytes()); // event: 2 = started, 0 = none
    ann_req[84..88].copy_from_slice(&0u32.to_be_bytes()); // IP default
    let key: u32 = rand::thread_rng().gen();
    ann_req[88..92].copy_from_slice(&key.to_be_bytes());
    ann_req[92..96].copy_from_slice(&PEERS_WANTED.to_be_bytes()); // num_want
    ann_req[96..98].copy_from_slice(&request.port.to_be_bytes());

    socket.send(&ann_req).await?;

    let mut ann_resp = vec![0u8; 2048];
    let len = tokio::time::timeout(Duration::from_secs(4), socket.recv(&mut ann_resp)).await??;
    if len < 20 {
        bail!("UDP tracker announce response too short");
    }

    let action = u32::from_be_bytes(ann_resp[0..4].try_into()?);
    let rx_tx = u32::from_be_bytes(ann_resp[4..8].try_into()?);
    if action != 1 || rx_tx != ann_tx {
        bail!("UDP tracker announce transaction mismatch");
    }

    // Announce response: <action:4><transaction:4><interval:4><leechers:4><seeders:4><peers...>
    let interval_secs = u32::from_be_bytes(ann_resp[8..12].try_into()?);
    let mut peers = Vec::new();
    for chunk in ann_resp[20..len].chunks_exact(6) {
        let ip = Ipv4Addr::new(chunk[0], chunk[1], chunk[2], chunk[3]);
        let p = u16::from_be_bytes([chunk[4], chunk[5]]);
        if p > 0 {
            peers.push(SocketAddr::V4(SocketAddrV4::new(ip, p)));
        }
    }

    Ok(AnnounceResponse {
        peers,
        interval: Some(Duration::from_secs(u64::from(interval_secs))),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::seeder::LocalTestSwarm;

    #[tokio::test]
    async fn test_announce_returns_peers_and_tracker_interval() {
        let temp_root = std::env::temp_dir().join("torrent_downloader_test_announce");
        let _ = std::fs::remove_dir_all(&temp_root);
        let swarm = LocalTestSwarm::start().await.unwrap();
        let torrent = swarm
            .create_and_seed_sample_game(&temp_root, "TestGameAnnounce", 1)
            .await
            .unwrap();
        let meta = TorrentMeta::from_file(&torrent).unwrap();

        let response = discover_peers(&meta, &[7u8; 20], 6881, AnnounceEvent::Started).await;

        assert_eq!(response.peers.len(), 1);
        assert_eq!(response.interval, Some(Duration::from_secs(60)));
        let _ = std::fs::remove_dir_all(&temp_root);
    }
}
