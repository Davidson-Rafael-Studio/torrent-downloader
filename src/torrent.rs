use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_bytes::ByteBuf;
use sha1::{Digest, Sha1};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TorrentFileEntry {
    pub path: PathBuf,
    pub length: u64,
    pub offset: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TorrentMeta {
    pub source_torrent_path: PathBuf,
    pub name: String,
    pub info_hash: [u8; 20],
    pub info_hash_hex: String,
    pub trackers: Vec<String>,
    pub piece_length: u64,
    pub pieces: Vec<[u8; 20]>,
    pub total_length: u64,
    pub files: Vec<TorrentFileEntry>,
    pub is_multi_file: bool,
}

#[derive(Debug, Deserialize)]
struct RawTorrent {
    #[serde(default)]
    announce: Option<String>,
    #[serde(rename = "announce-list", default)]
    announce_list: Option<Vec<Vec<String>>>,
    info: RawInfo,
}

#[derive(Debug, Deserialize)]
struct RawInfo {
    name: String,
    #[serde(rename = "piece length")]
    piece_length: u64,
    pieces: ByteBuf,
    #[serde(default)]
    length: Option<u64>,
    #[serde(default)]
    files: Option<Vec<RawFileItem>>,
}

#[derive(Debug, Deserialize)]
struct RawFileItem {
    length: u64,
    path: Vec<String>,
}

impl TorrentMeta {
    /// Parses a `.torrent` file from disk.
    pub fn from_file(path: &Path) -> Result<Self> {
        let raw_bytes = std::fs::read(path)
            .with_context(|| format!("Failed to read torrent file: {}", path.display()))?;
        Self::from_bytes(&raw_bytes, path.to_path_buf())
    }

    pub fn from_bytes(raw_bytes: &[u8], source_torrent_path: PathBuf) -> Result<Self> {
        // Parse generic bencode Value to compute exact SHA-1 of the `info` dictionary
        // preserving all keys in canonical bencode order.
        let root_val: serde_bencode::value::Value = serde_bencode::from_bytes(raw_bytes)
            .context("Invalid bencode format in .torrent file")?;

        let info_hash = compute_info_hash(&root_val)?;
        let info_hash_hex = info_hash.iter().map(|b| format!("{:02x}", b)).collect();

        let parsed: RawTorrent = serde_bencode::from_bytes(raw_bytes)
            .context("Failed to parse standard .torrent metadata fields")?;

        if !parsed.info.pieces.len().is_multiple_of(20) {
            bail!(
                "Invalid pieces buffer length ({}): not a multiple of 20 bytes",
                parsed.info.pieces.len()
            );
        }

        let pieces: Vec<[u8; 20]> = parsed
            .info
            .pieces
            .chunks_exact(20)
            .map(|chunk| {
                let mut arr = [0u8; 20];
                arr.copy_from_slice(chunk);
                arr
            })
            .collect();

        // Collect all unique tracker URLs
        let mut trackers = Vec::new();
        if let Some(primary) = parsed.announce {
            let trimmed = primary.trim().to_string();
            if !trimmed.is_empty() {
                trackers.push(trimmed);
            }
        }
        if let Some(tiers) = parsed.announce_list {
            for tier in tiers {
                for url in tier {
                    let trimmed = url.trim().to_string();
                    if !trimmed.is_empty() && !trackers.contains(&trimmed) {
                        trackers.push(trimmed);
                    }
                }
            }
        }

        // Extract single-file or multi-file layout
        let mut files = Vec::new();
        let mut total_length = 0u64;
        let is_multi_file;

        if let Some(raw_files) = parsed.info.files {
            is_multi_file = true;
            for item in raw_files {
                let mut rel_path = PathBuf::new();
                for part in &item.path {
                    // Prevent path traversal
                    if part != ".." && part != "." && !part.is_empty() {
                        rel_path.push(part);
                    }
                }
                if rel_path.as_os_str().is_empty() {
                    rel_path.push("file.bin");
                }
                files.push(TorrentFileEntry {
                    path: rel_path,
                    length: item.length,
                    offset: total_length,
                });
                total_length += item.length;
            }
        } else if let Some(len) = parsed.info.length {
            is_multi_file = false;
            files.push(TorrentFileEntry {
                path: PathBuf::from(&parsed.info.name),
                length: len,
                offset: 0,
            });
            total_length = len;
        } else {
            bail!("Torrent info dictionary has neither 'length' nor 'files'");
        }

        Ok(Self {
            source_torrent_path,
            name: parsed.info.name,
            info_hash,
            info_hash_hex,
            trackers,
            piece_length: parsed.info.piece_length.max(1),
            pieces,
            total_length,
            files,
            is_multi_file,
        })
    }

    /// Returns the byte length of a given piece index.
    pub fn piece_size(&self, piece_index: usize) -> u64 {
        let num_pieces = self.pieces.len();
        if num_pieces == 0 || piece_index >= num_pieces {
            return 0;
        }
        if piece_index == num_pieces - 1 {
            let rem = self.total_length % self.piece_length;
            if rem == 0 {
                self.piece_length
            } else {
                rem
            }
        } else {
            self.piece_length
        }
    }
}

fn compute_info_hash(root_val: &serde_bencode::value::Value) -> Result<[u8; 20]> {
    if let serde_bencode::value::Value::Dict(dict) = root_val {
        if let Some(info_val) = dict.get(b"info".as_ref()) {
            let encoded_info = serde_bencode::to_bytes(info_val)
                .context("Failed to re-encode info dictionary for SHA-1")?;
            let mut hasher = Sha1::new();
            hasher.update(&encoded_info);
            let digest = hasher.finalize();
            let mut out = [0u8; 20];
            out.copy_from_slice(&digest);
            return Ok(out);
        }
    }
    bail!("Missing 'info' dictionary in .torrent root")
}

/// Helper to create a synthetic `.torrent` file on disk (useful for testing or demoing).
pub fn create_sample_torrent_bytes(
    game_name: &str,
    files_data: &[(&str, &[u8])],
    piece_length: u64,
    tracker_url: &str,
) -> Result<Vec<u8>> {
    use serde_bencode::value::Value;

    let mut combined = Vec::new();
    let mut file_list = Vec::new();

    for (rel_path, data) in files_data {
        combined.extend_from_slice(data);
        let mut fdict = HashMap::new();
        fdict.insert(b"length".to_vec(), Value::Int(data.len() as i64));
        let path_parts: Vec<Value> = rel_path
            .split('/')
            .map(|p| Value::Bytes(p.as_bytes().to_vec()))
            .collect();
        fdict.insert(b"path".to_vec(), Value::List(path_parts));
        file_list.push(Value::Dict(fdict));
    }

    let mut pieces_concat = Vec::new();
    for chunk in combined.chunks(piece_length as usize) {
        let mut hasher = Sha1::new();
        hasher.update(chunk);
        pieces_concat.extend_from_slice(&hasher.finalize());
    }

    let mut info = HashMap::new();
    info.insert(b"name".to_vec(), Value::Bytes(game_name.as_bytes().to_vec()));
    info.insert(b"piece length".to_vec(), Value::Int(piece_length as i64));
    info.insert(b"pieces".to_vec(), Value::Bytes(pieces_concat));
    info.insert(b"files".to_vec(), Value::List(file_list));

    let mut root = HashMap::new();
    root.insert(
        b"announce".to_vec(),
        Value::Bytes(tracker_url.as_bytes().to_vec()),
    );
    root.insert(b"info".to_vec(), Value::Dict(info));

    Ok(serde_bencode::to_bytes(&Value::Dict(root))?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_multi_file_torrent() {
        let raw = create_sample_torrent_bytes(
            "CyberSpaceRunner",
            &[
                ("CyberSpaceRunner.exe", b"MZ-FAKE-GAME-BINARY-DATA-1234567890"),
                ("README.txt", b"Run CyberSpaceRunner via Steam!"),
            ],
            32,
            "http://tracker.example.com:6969/announce",
        )
        .unwrap();

        let meta = TorrentMeta::from_bytes(&raw, PathBuf::from("CyberSpaceRunner.torrent")).unwrap();
        assert_eq!(meta.name, "CyberSpaceRunner");
        assert!(meta.is_multi_file);
        assert_eq!(meta.files.len(), 2);
        assert_eq!(meta.pieces.len(), 3);
    }
}
