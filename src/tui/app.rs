use crate::engine::DownloadManager;
use crate::job::{DownloadStatus, TorrentJob, TorrentJobSnapshot};
use crate::path_utils::{normalize_input_path, resolve_torrent_paths};
use crate::seeder::LocalTestSwarm;
use crate::steam::find_game_executables;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

const PIECE_PREVIEW_LEN: usize = 180;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum InputPrompt {
    AddTorrentOrDir,
    ChangeSteamDir,
}

impl InputPrompt {
    pub(super) fn title(self) -> &'static str {
        match self {
            InputPrompt::AddTorrentOrDir => {
                " Insert or Drag & Drop .torrent File OR Source Directory Path "
            }
            InputPrompt::ChangeSteamDir => " Set Steam Library Destination (steamapps/common) ",
        }
    }

    pub(super) fn hint(self) -> &'static str {
        match self {
            InputPrompt::AddTorrentOrDir => {
                "Drag & drop a .torrent file or folder from Finder/Explorer into this window, or type its path.\nPress [Enter] to start download, or [Esc] to cancel."
            }
            InputPrompt::ChangeSteamDir => {
                "Enter or drag & drop the target Steam 'steamapps/common' folder path.\nPress [Enter] to save, or [Esc] to cancel."
            }
        }
    }
}

pub struct TuiApp {
    pub(super) manager: Arc<DownloadManager>,
    local_swarm: Arc<LocalTestSwarm>,
    pub(super) selected_index: usize,
    pub(super) input_prompt: Option<InputPrompt>,
    pub(super) input_buffer: String,
    pub(super) watched_source_dirs: Vec<PathBuf>,
    /// Torrent files already handed to the manager. Rescans skip them, so a job the user
    /// paused or removed is not resumed or re-added behind their back.
    known_torrent_paths: HashSet<PathBuf>,
    pub(super) executables_by_job_id: HashMap<usize, String>,
    pub(super) status_banner: String,
    next_demo_number: usize,
}

impl TuiApp {
    pub fn new(
        manager: Arc<DownloadManager>,
        local_swarm: Arc<LocalTestSwarm>,
        initial_watched_dirs: Vec<PathBuf>,
    ) -> Self {
        Self {
            manager,
            local_swarm,
            selected_index: 0,
            input_prompt: None,
            input_buffer: String::new(),
            watched_source_dirs: initial_watched_dirs,
            known_torrent_paths: HashSet::new(),
            executables_by_job_id: HashMap::new(),
            status_banner: "Tip: Drag & drop a .torrent file or folder directly into this terminal, or press 'a' to input a path, or 'g' to test with a sample Steam game torrent!".to_string(),
            next_demo_number: 1,
        }
    }

    async fn add_torrents_from_input(&mut self, raw_input: &str) {
        let raw_input = raw_input.trim();
        if raw_input.is_empty() {
            return;
        }

        let input_path = normalize_input_path(raw_input);
        if input_path.is_dir() && !self.watched_source_dirs.contains(&input_path) {
            self.watched_source_dirs.push(input_path.clone());
        }

        let torrent_paths = match resolve_torrent_paths(&input_path) {
            Ok(paths) => paths,
            Err(error) => {
                self.status_banner = format!("Error: {error}");
                return;
            }
        };

        let mut added_count = 0usize;
        let mut last_error = None;
        for torrent_path in torrent_paths {
            match self.manager.add_torrent_file(&torrent_path).await {
                Ok(_) => {
                    added_count += 1;
                    self.known_torrent_paths.insert(torrent_path);
                }
                Err(error) => last_error = Some(error),
            }
        }

        if added_count > 0 {
            self.select_last_job().await;
            self.status_banner = format!(
                "Added {} .torrent file(s) from '{}'. Multi-thread downloading into Steam folder!",
                added_count,
                input_path.display()
            );
        } else if let Some(error) = last_error {
            self.status_banner = format!("Failed to add torrent: {error}");
        }
    }

    async fn spawn_demo_game(&mut self) {
        const DEMO_GAME_NAMES: [&str; 4] = [
            "CyberBlade_2099_Steam",
            "StarDrifter_Odyssey",
            "DungeonForge_Legends",
            "NeonDrift_Racing_GT",
        ];
        let demo_number = self.next_demo_number;
        self.next_demo_number += 1;
        let game_name = format!(
            "{}_{}",
            DEMO_GAME_NAMES[(demo_number - 1) % DEMO_GAME_NAMES.len()],
            demo_number
        );

        let sample_dir = std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join("sample_torrents");

        let torrent_path = match self
            .local_swarm
            .create_and_seed_sample_game(&sample_dir, &game_name, 2)
            .await
        {
            Ok(path) => path,
            Err(error) => {
                self.status_banner = format!("Failed to generate sample game torrent: {error}");
                return;
            }
        };

        match self.manager.add_torrent_file(&torrent_path).await {
            Ok(_) => {
                self.select_last_job().await;
                self.status_banner = format!(
                    "Generated & started '{}' ({}) -> downloading to Steam common dir!",
                    game_name,
                    torrent_path.display()
                );
                self.known_torrent_paths.insert(torrent_path);
            }
            Err(error) => {
                self.status_banner = format!("Error adding sample torrent: {error}");
            }
        }
    }

    pub(super) async fn rescan_watched_dirs(&mut self) {
        for dir in &self.watched_source_dirs {
            let Ok(torrent_paths) = resolve_torrent_paths(dir) else {
                continue;
            };
            for torrent_path in torrent_paths {
                if self.known_torrent_paths.contains(&torrent_path) {
                    continue;
                }
                // Failed files stay unknown, so a .torrent still being copied is retried later.
                if self.manager.add_torrent_file(&torrent_path).await.is_ok() {
                    self.known_torrent_paths.insert(torrent_path);
                }
            }
        }
    }

    async fn change_steam_dir(&mut self, raw_input: &str) {
        let new_dir = normalize_input_path(raw_input);
        if let Err(error) = std::fs::create_dir_all(&new_dir) {
            self.status_banner = format!(
                "Failed to create Steam directory {}: {}",
                new_dir.display(),
                error
            );
            return;
        }
        self.status_banner = format!(
            "Updated Steam destination directory to: {}",
            new_dir.display()
        );
        *self.manager.steam_common_dir.write().await = new_dir;
    }

    pub(super) async fn collect_snapshots(&self) -> Vec<TorrentJobSnapshot> {
        let jobs = self.manager.jobs.read().await;
        let mut snapshots = Vec::with_capacity(jobs.len());
        for job in jobs.iter() {
            snapshots.push(job.snapshot(PIECE_PREVIEW_LEN).await);
        }
        snapshots
    }

    /// Scans the selected job's folder for executables once it completes, instead of
    /// reading the directory on every frame.
    pub(super) fn cache_selected_executables(&mut self, snapshots: &[TorrentJobSnapshot]) {
        let Some(selected) = snapshots.get(self.selected_index) else {
            return;
        };
        if selected.status == DownloadStatus::Completed {
            self.executables_by_job_id
                .entry(selected.id)
                .or_insert_with(|| describe_game_executables(&selected.game_install_dir));
        }
    }

    async fn selected_job(&self) -> Option<Arc<TorrentJob>> {
        self.manager
            .jobs
            .read()
            .await
            .get(self.selected_index)
            .cloned()
    }

    async fn select_last_job(&mut self) {
        self.selected_index = self.manager.jobs.read().await.len().saturating_sub(1);
    }

    fn move_selection(&mut self, step: isize, job_count: usize) {
        if job_count == 0 {
            return;
        }
        self.selected_index =
            (self.selected_index as isize + step).rem_euclid(job_count as isize) as usize;
    }

    fn open_input(&mut self, prompt: InputPrompt, initial_text: String) {
        self.input_prompt = Some(prompt);
        self.input_buffer = initial_text;
    }

    pub(super) async fn handle_paste(&mut self, pasted: &str) {
        match self.input_prompt {
            None => self.add_torrents_from_input(pasted).await,
            Some(_) => self.input_buffer.push_str(pasted.trim()),
        }
    }

    /// Returns `true` when the user asked to quit.
    pub(super) async fn handle_key(&mut self, key: KeyEvent, snapshots: &[TorrentJobSnapshot]) -> bool {
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            return true;
        }
        match self.input_prompt {
            None => return self.handle_queue_key(key.code, snapshots).await,
            Some(prompt) => self.handle_input_key(prompt, key.code).await,
        }
        false
    }

    async fn handle_input_key(&mut self, prompt: InputPrompt, key_code: KeyCode) {
        match key_code {
            KeyCode::Esc => {
                self.input_prompt = None;
                self.input_buffer.clear();
            }
            KeyCode::Enter => {
                self.input_prompt = None;
                let submitted = std::mem::take(&mut self.input_buffer);
                match prompt {
                    InputPrompt::AddTorrentOrDir => self.add_torrents_from_input(&submitted).await,
                    InputPrompt::ChangeSteamDir => self.change_steam_dir(&submitted).await,
                }
            }
            KeyCode::Backspace => {
                self.input_buffer.pop();
            }
            KeyCode::Char(typed) => self.input_buffer.push(typed),
            _ => {}
        }
    }

    /// Returns `true` when the user asked to quit.
    async fn handle_queue_key(&mut self, key_code: KeyCode, snapshots: &[TorrentJobSnapshot]) -> bool {
        match key_code {
            KeyCode::Char('q') | KeyCode::Esc => return true,
            KeyCode::Char('a' | 'i') => self.open_input(InputPrompt::AddTorrentOrDir, String::new()),
            // Some terminals deliver drag-and-drop as raw keystrokes instead of a Paste event.
            // Dropped paths start with one of these characters, so start capturing a path.
            KeyCode::Char(first_char @ ('/' | '~' | '\'' | '"')) => {
                self.open_input(InputPrompt::AddTorrentOrDir, first_char.to_string())
            }
            KeyCode::Char('d') => {
                let current_dir = self
                    .manager
                    .steam_common_dir
                    .read()
                    .await
                    .to_string_lossy()
                    .into_owned();
                self.open_input(InputPrompt::ChangeSteamDir, current_dir);
            }
            KeyCode::Char('g') => self.spawn_demo_game().await,
            KeyCode::Down | KeyCode::Char('j') => self.move_selection(1, snapshots.len()),
            KeyCode::Up | KeyCode::Char('k') => self.move_selection(-1, snapshots.len()),
            KeyCode::Char(' ' | 'p') => {
                if let Some(job) = self.selected_job().await {
                    let status = job.toggle_pause().await;
                    self.status_banner = format!("'{}' is now {}", job.meta.name, status.label());
                }
            }
            KeyCode::Char('r') => {
                if let Some(job) = self.selected_job().await {
                    job.resume().await;
                    self.status_banner = format!("Resumed '{}'", job.meta.name);
                }
            }
            KeyCode::Char('P') => {
                self.manager.pause_all().await;
                self.status_banner = "Paused all active torrent downloads.".to_string();
            }
            KeyCode::Char('R') => {
                self.manager.resume_all().await;
                self.status_banner = "Resumed all torrent downloads.".to_string();
            }
            KeyCode::Char('+' | '=') => self.change_selected_threads(1).await,
            KeyCode::Char('-' | '_') => self.change_selected_threads(-1).await,
            KeyCode::Char('t') => {
                if let Some(job) = self.selected_job().await {
                    let state = if job.toggle_auto_threads() { "on" } else { "off" };
                    self.status_banner =
                        format!("Automatic thread tuning for '{}' is {}", job.meta.name, state);
                }
            }
            KeyCode::Char('x') | KeyCode::Delete => self.remove_selected_job(snapshots).await,
            _ => {}
        }
        false
    }

    async fn change_selected_threads(&mut self, delta: isize) {
        if let Some(job) = self.selected_job().await {
            let thread_count = job.adjust_threads_manually(delta);
            self.status_banner = format!(
                "Worker threads for '{}' set to {} (manual, press 't' for auto)",
                job.meta.name, thread_count
            );
        }
    }

    async fn remove_selected_job(&mut self, snapshots: &[TorrentJobSnapshot]) {
        let Some(selected) = snapshots.get(self.selected_index) else {
            return;
        };
        self.manager.remove_job(self.selected_index).await;
        self.executables_by_job_id.remove(&selected.id);
        self.status_banner = format!("Removed '{}' from download queue.", selected.name);
    }
}

fn describe_game_executables(game_dir: &Path) -> String {
    let executable_names: Vec<String> = find_game_executables(game_dir)
        .iter()
        .filter_map(|path| path.file_name())
        .map(|name| name.to_string_lossy().into_owned())
        .collect();
    if executable_names.is_empty() {
        "None found".to_string()
    } else {
        executable_names.join(", ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_rescan_keeps_paused_job_paused_and_removed_job_removed() {
        let temp_root = std::env::temp_dir().join("torrent_downloader_test_rescan");
        let _ = std::fs::remove_dir_all(&temp_root);
        let torrents_dir = temp_root.join("torrents");
        let swarm = LocalTestSwarm::start().await.unwrap();
        let torrent = swarm
            .create_and_seed_sample_game(&torrents_dir, "TestGameRescan", 2)
            .await
            .unwrap();
        let manager = DownloadManager::new(temp_root.join("steamapps/common"), 4);
        let mut app = TuiApp::new(manager.clone(), swarm, vec![torrents_dir]);

        app.add_torrents_from_input(&torrent.to_string_lossy()).await;
        let job = app.selected_job().await.unwrap();
        job.pause().await;

        app.rescan_watched_dirs().await;
        assert_eq!(job.snapshot(0).await.status, DownloadStatus::Paused);

        manager.remove_job(0).await;
        app.rescan_watched_dirs().await;
        assert!(manager.jobs.read().await.is_empty());

        let _ = std::fs::remove_dir_all(&temp_root);
    }
}
