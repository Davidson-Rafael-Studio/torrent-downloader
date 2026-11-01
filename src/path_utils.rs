use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};

/// Cleans and normalizes a raw path string from terminal input or drag-and-drop.
/// Handles:
/// - Leading/trailing whitespace
/// - Single or double quotes wrapped by terminal drag-and-drop
/// - Backslash-escaped spaces and special characters (e.g. `My\ Game.torrent` on macOS)
/// - `~/` home directory expansion
pub fn normalize_input_path(raw: &str) -> PathBuf {
    let mut s = raw.trim().to_string();

    // Strip wrapping single or double quotes
    if (s.starts_with('\'') && s.ends_with('\'')) || (s.starts_with('"') && s.ends_with('"')) {
        if s.len() >= 2 {
            s = s[1..s.len() - 1].to_string();
        }
    } else {
        // Unescape backslash-escaped characters common in macOS Terminal drag-and-drop
        let mut unescaped = String::with_capacity(s.len());
        let mut chars = s.chars().peekable();
        while let Some(c) = chars.next() {
            if c == '\\' {
                if let Some(&next_c) = chars.peek() {
                    // On Unix/macOS, drag-and-drop escapes spaces, parentheses, brackets, etc.
                    if matches!(
                        next_c,
                        ' ' | '(' | ')' | '[' | ']' | '&' | '\'' | '"' | '!' | '$' | ';'
                    ) {
                        unescaped.push(next_c);
                        chars.next();
                        continue;
                    }
                }
            }
            unescaped.push(c);
        }
        s = unescaped;
    }

    // Expand `~` to user home directory
    if s == "~" {
        if let Some(home) = dirs::home_dir() {
            return home;
        }
    } else if let Some(rest) = s.strip_prefix("~/") {
        if let Some(home) = dirs::home_dir() {
            return home.join(rest);
        }
    }

    PathBuf::from(s)
}

/// Given a normalized path (either a `.torrent` file or a directory containing `.torrent` files),
/// returns all discovered `.torrent` file paths.
pub fn resolve_torrent_paths(path: &Path) -> Result<Vec<PathBuf>> {

    if !path.exists() {
        bail!("Path does not exist: {}", path.display());
    }

    if path.is_file() {
        if is_torrent_file(path) {
            return Ok(vec![path.to_path_buf()]);
        } else {
            bail!(
                "File '{}' is not a .torrent file (expected .torrent extension)",
                path.display()
            );
        }
    }

    if path.is_dir() {
        let mut found = Vec::new();
        collect_torrents_in_dir(path, &mut found, 2)?;
        found.sort();
        if found.is_empty() {
            bail!(
                "No .torrent files found inside directory: {}",
                path.display()
            );
        }
        return Ok(found);
    }

    bail!("Unsupported path: {}", path.display())
}

fn collect_torrents_in_dir(dir: &Path, out: &mut Vec<PathBuf>, max_depth: usize) -> Result<()> {
    let entries = std::fs::read_dir(dir)
        .with_context(|| format!("Failed to read directory {}", dir.display()))?;

    for entry in entries.flatten() {
        let p = entry.path();
        if p.is_file() && is_torrent_file(&p) {
            out.push(p);
        } else if p.is_dir() && max_depth > 0 {
            let _ = collect_torrents_in_dir(&p, out, max_depth - 1);
        }
    }
    Ok(())
}

pub fn is_torrent_file(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| e.eq_ignore_ascii_case("torrent"))
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_normalize_escaped_mac_path() {
        let input = "/Users/test/Downloads/My\\ Cool\\ Game\\ \\(2026\\).torrent";
        let norm = normalize_input_path(input);
        assert_eq!(
            norm,
            PathBuf::from("/Users/test/Downloads/My Cool Game (2026).torrent")
        );
    }

    #[test]
    fn test_normalize_quoted_path() {
        let input = "'/Users/test/Downloads/My Cool Game.torrent'";
        let norm = normalize_input_path(input);
        assert_eq!(
            norm,
            PathBuf::from("/Users/test/Downloads/My Cool Game.torrent")
        );
    }
}
