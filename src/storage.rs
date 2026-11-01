use crate::steam::prepare_game_install_dir;
use crate::torrent::TorrentMeta;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sha1::{Digest, Sha1};
use std::fs::OpenOptions;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResumeManifest {
    pub info_hash_hex: String,
    pub completed_pieces: Vec<bool>,
}

/// Writes a verified piece buffer at its global torrent offset into the target file(s)
/// inside the Steam game folder.
pub fn write_piece_to_disk(
    meta: &TorrentMeta,
    game_install_dir: &Path,
    piece_index: usize,
    piece_data: &[u8],
) -> Result<()> {
    let piece_start = (piece_index as u64) * meta.piece_length;
    let piece_end = piece_start + (piece_data.len() as u64);

    for file_entry in &meta.files {
        let file_start = file_entry.offset;
        let file_end = file_start + file_entry.length;

        // Check overlap between [piece_start, piece_end) and [file_start, file_end)
        if piece_end <= file_start || piece_start >= file_end {
            continue;
        }

        let overlap_start = piece_start.max(file_start);
        let overlap_end = piece_end.min(file_end);
        let overlap_len = (overlap_end - overlap_start) as usize;

        let piece_slice_offset = (overlap_start - piece_start) as usize;
        let file_seek_offset = overlap_start - file_start;

        let target_path = game_install_dir.join(&file_entry.path);
        if let Some(parent) = target_path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        // No truncate: other pieces of the same file may already be written at other offsets.
        let mut file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .read(true)
            .open(&target_path)
            .with_context(|| format!("Failed to open target file {}", target_path.display()))?;

        file.seek(SeekFrom::Start(file_seek_offset))?;
        file.write_all(&piece_data[piece_slice_offset..piece_slice_offset + overlap_len])?;
    }

    Ok(())
}

/// Reads a piece from disk and verifies its SHA-1 hash against `meta.pieces[piece_index]`.
pub fn verify_piece_on_disk(
    meta: &TorrentMeta,
    game_install_dir: &Path,
    piece_index: usize,
) -> bool {
    let expected_len = meta.piece_size(piece_index) as usize;
    if expected_len == 0 {
        return false;
    }

    let piece_start = (piece_index as u64) * meta.piece_length;
    let piece_end = piece_start + (expected_len as u64);
    let mut buf = vec![0u8; expected_len];

    for file_entry in &meta.files {
        let file_start = file_entry.offset;
        let file_end = file_start + file_entry.length;

        if piece_end <= file_start || piece_start >= file_end {
            continue;
        }

        let overlap_start = piece_start.max(file_start);
        let overlap_end = piece_end.min(file_end);
        let overlap_len = (overlap_end - overlap_start) as usize;

        let piece_slice_offset = (overlap_start - piece_start) as usize;
        let file_seek_offset = overlap_start - file_start;

        let target_path = game_install_dir.join(&file_entry.path);
        let Ok(mut file) = OpenOptions::new().read(true).open(&target_path) else {
            return false;
        };
        if file.seek(SeekFrom::Start(file_seek_offset)).is_err() {
            return false;
        }
        if file
            .read_exact(&mut buf[piece_slice_offset..piece_slice_offset + overlap_len])
            .is_err()
        {
            return false;
        }
    }

    let mut hasher = Sha1::new();
    hasher.update(&buf);
    let digest = hasher.finalize();
    digest.as_slice() == meta.pieces[piece_index]
}

/// Loads or verifies existing piece completion state for a torrent in the Steam game folder.
pub fn load_or_verify_resume_state(
    meta: &TorrentMeta,
    steam_common_dir: &Path,
) -> Result<(PathBuf, Vec<bool>)> {
    let game_install_dir = prepare_game_install_dir(steam_common_dir, &meta.name)?;
    let manifest_path = game_install_dir.join(".torrent_resume.json");
    let num_pieces = meta.pieces.len();

    if manifest_path.exists() {
        if let Ok(raw) = std::fs::read_to_string(&manifest_path) {
            if let Ok(manifest) = serde_json::from_str::<ResumeManifest>(&raw) {
                if manifest.info_hash_hex == meta.info_hash_hex
                    && manifest.completed_pieces.len() == num_pieces
                {
                    return Ok((game_install_dir, manifest.completed_pieces));
                }
            }
        }
    }

    let completed: Vec<bool> = (0..num_pieces)
        .map(|piece_index| verify_piece_on_disk(meta, &game_install_dir, piece_index))
        .collect();

    let _ = save_resume_state(meta, &game_install_dir, &completed);
    Ok((game_install_dir, completed))
}

pub fn save_resume_state(
    meta: &TorrentMeta,
    game_install_dir: &Path,
    completed_pieces: &[bool],
) -> Result<()> {
    let manifest_path = game_install_dir.join(".torrent_resume.json");
    let manifest = ResumeManifest {
        info_hash_hex: meta.info_hash_hex.clone(),
        completed_pieces: completed_pieces.to_vec(),
    };
    let json = serde_json::to_string(&manifest)?;
    write_file_atomically(&manifest_path, json.as_bytes())
        .with_context(|| format!("Failed to save resume state: {}", manifest_path.display()))
}

/// Writes `data` to a uniquely named temp file next to `path`, then renames it over `path`.
/// If the app crashes mid-write the previous file stays intact, and concurrent writers never
/// share a temp file. There is no fsync, so a power loss can still lose the latest write;
/// the resume loader then falls back to re-verifying pieces on disk.
fn write_file_atomically(path: &Path, data: &[u8]) -> Result<()> {
    static TEMP_FILE_COUNTER: AtomicU64 = AtomicU64::new(0);
    let unique_suffix = TEMP_FILE_COUNTER.fetch_add(1, Ordering::Relaxed);
    let file_name = path
        .file_name()
        .context("Target path has no file name")?
        .to_string_lossy();
    let temp_path = path.with_file_name(format!(
        "{file_name}.{}.{unique_suffix}.tmp",
        std::process::id()
    ));

    let result = std::fs::write(&temp_path, data)
        .and_then(|()| std::fs::rename(&temp_path, path))
        .map_err(anyhow::Error::from);

    if result.is_err() {
        let _ = std::fs::remove_file(&temp_path);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::torrent::create_sample_torrent_bytes;

    fn sample_meta() -> TorrentMeta {
        let raw = create_sample_torrent_bytes(
            "ResumeTestGame",
            &[("ResumeTestGame.exe", b"MZ-FAKE-GAME-BINARY-DATA-1234567890")],
            8,
            "http://tracker.example.com:6969/announce",
        )
        .unwrap();
        TorrentMeta::from_bytes(&raw, PathBuf::from("ResumeTestGame.torrent")).unwrap()
    }

    #[test]
    fn test_concurrent_resume_saves_leave_valid_manifest_and_no_temp_files() {
        let meta = sample_meta();
        let dir = std::env::temp_dir().join(format!("torrent_resume_atomic_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let num_pieces = meta.pieces.len();

        std::thread::scope(|s| {
            for t in 0..8 {
                let (meta, dir) = (&meta, &dir);
                s.spawn(move || {
                    for i in 0..50 {
                        let completed: Vec<bool> =
                            (0..num_pieces).map(|p| (p + t + i) % 2 == 0).collect();
                        save_resume_state(meta, dir, &completed).unwrap();
                    }
                });
            }
        });

        let raw = std::fs::read_to_string(dir.join(".torrent_resume.json")).unwrap();
        let manifest: ResumeManifest = serde_json::from_str(&raw).unwrap();
        assert_eq!(manifest.info_hash_hex, meta.info_hash_hex);
        assert_eq!(manifest.completed_pieces.len(), num_pieces);

        let leftovers: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "temp files left behind: {leftovers:?}");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
