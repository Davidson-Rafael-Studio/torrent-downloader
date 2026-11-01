use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

/// Detects the default Steam `steamapps/common` directory on the system.
/// If Steam is not found on the system yet, returns the standard platform path
/// (or a local fallback if home dir isn't available) and creates it when needed.
pub fn detect_steam_common_dir() -> PathBuf {
    if let Some(home) = dirs::home_dir() {
        // macOS default Steam library path
        #[cfg(target_os = "macos")]
        {
            let mac_steam = home
                .join("Library")
                .join("Application Support")
                .join("Steam")
                .join("steamapps")
                .join("common");
            return mac_steam;
        }

        // Linux default Steam library paths
        #[cfg(target_os = "linux")]
        {
            let candidates = [
                home.join(".steam/steam/steamapps/common"),
                home.join(".local/share/Steam/steamapps/common"),
                home.join(".var/app/com.valvesoftware.Steam/.local/share/Steam/steamapps/common"),
            ];
            for candidate in &candidates {
                if candidate.exists() {
                    return candidate.clone();
                }
            }
            return candidates[0].clone();
        }

        // Windows default Steam library path
        #[cfg(target_os = "windows")]
        {
            let win_candidates = [
                PathBuf::from(r"C:\Program Files (x86)\Steam\steamapps\common"),
                PathBuf::from(r"C:\Program Files\Steam\steamapps\common"),
            ];
            for candidate in &win_candidates {
                if candidate.exists() {
                    return candidate.clone();
                }
            }
            return win_candidates[0].clone();
        }
    }

    PathBuf::from("./steamapps/common")
}

/// Ensures the target Steam game directory exists and returns its full path.
pub fn prepare_game_install_dir(steam_common_dir: &Path, game_folder_name: &str) -> Result<PathBuf> {
    let sanitized_name = sanitize_folder_name(game_folder_name);
    let install_dir = steam_common_dir.join(sanitized_name);
    std::fs::create_dir_all(&install_dir).with_context(|| {
        format!(
            "Failed to create Steam game directory at {}",
            install_dir.display()
        )
    })?;
    Ok(install_dir)
}

/// Strips `.torrent` extension or unsafe path characters to form a clean game folder name.
pub fn sanitize_folder_name(raw_name: &str) -> String {
    let base = raw_name
        .strip_suffix(".torrent")
        .or_else(|| raw_name.strip_suffix(".TORRENT"))
        .unwrap_or(raw_name);

    let cleaned: String = base
        .chars()
        .map(|c| match c {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' | '\0' => '_',
            _ => c,
        })
        .collect();

    let trimmed = cleaned.trim();
    if trimmed.is_empty() {
        "Downloaded_Steam_Game".to_string()
    } else {
        trimmed.to_string()
    }
}

/// Scans a completed game directory for candidate executables (`.app`, `.exe`, `.sh`, or binaries)
/// so the user can see what executable was downloaded or register it in Steam later.
pub fn find_game_executables(game_dir: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    if !game_dir.exists() {
        return found;
    }

    if let Ok(entries) = std::fs::read_dir(game_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
                if ext.eq_ignore_ascii_case("exe")
                    || ext.eq_ignore_ascii_case("app")
                    || ext.eq_ignore_ascii_case("sh")
                    || ext.eq_ignore_ascii_case("bin")
                {
                    found.push(path);
                }
            }
        }
    }
    found
}
