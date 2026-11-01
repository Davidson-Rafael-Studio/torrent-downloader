pub mod engine;
pub mod job;
pub mod path_utils;
pub mod peer;
pub mod seeder;
pub mod steam;
pub mod storage;
pub mod thread_tuner;
pub mod torrent;
pub mod tracker;
pub mod tui;
pub mod worker;

use crate::engine::DownloadManager;
use crate::job::DownloadStatus;
use crate::path_utils::{normalize_input_path, resolve_torrent_paths};
use crate::seeder::LocalTestSwarm;
use crate::steam::detect_steam_common_dir;
use crate::tui::{format_bytes, run_tui, thread_mode_label, TuiApp};
use anyhow::Result;
use clap::Parser;
use std::path::PathBuf;
use std::time::Duration;

#[derive(Parser, Debug)]
#[command(
    name = "torrent-downloader",
    version,
    about = "Multi-threaded BitTorrent game downloader with Pause/Resume, Multi-Download, Drag & Drop TUI, and Steam folder integration"
)]
struct Cli {
    /// One or more .torrent files or directories containing .torrent files (can also be dragged & dropped in the TUI)
    #[arg(value_name = "TORRENT_FILE_OR_DIR")]
    inputs: Vec<String>,

    /// Source directory containing .torrent files to import and watch
    #[arg(short = 's', long = "src-dir")]
    src_dir: Option<String>,

    /// Target Steam `steamapps/common` directory (auto-detected by default)
    #[arg(short = 'd', long = "steam-dir")]
    steam_dir: Option<String>,

    /// Starting number of worker threads per torrent (1..16); tuned automatically to the
    /// connection speed unless changed manually in the TUI
    #[arg(short = 't', long = "threads", default_value_t = 4)]
    threads: usize,

    /// Spawn sample Steam game .torrent files on startup to demonstrate multi-threaded downloading & pause/resume
    #[arg(long = "demo")]
    demo: bool,

    /// Run in headless CLI progress mode instead of the interactive Ratatui TUI
    #[arg(long = "headless")]
    headless: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    let steam_dir = match &cli.steam_dir {
        Some(custom) => normalize_input_path(custom),
        None => detect_steam_common_dir(),
    };
    std::fs::create_dir_all(&steam_dir)?;

    let manager = DownloadManager::new(steam_dir, cli.threads);
    let local_swarm = LocalTestSwarm::start().await?;

    let mut watched_dirs: Vec<PathBuf> = Vec::new();
    let mut all_inputs = cli.inputs.clone();
    if let Some(src) = &cli.src_dir {
        all_inputs.push(src.clone());
    }

    for raw_input in &all_inputs {
        let input_path = normalize_input_path(raw_input);
        if let Ok(torrent_paths) = resolve_torrent_paths(&input_path) {
            for torrent_path in torrent_paths {
                let _ = manager.add_torrent_file(&torrent_path).await;
            }
        }
        if input_path.is_dir() && !watched_dirs.contains(&input_path) {
            watched_dirs.push(input_path);
        }
    }

    if cli.demo {
        let sample_dir = std::env::current_dir()?.join("sample_torrents");
        let t1 = local_swarm
            .create_and_seed_sample_game(&sample_dir, "CyberQuest_SteamEdition", 2)
            .await?;
        let t2 = local_swarm
            .create_and_seed_sample_game(&sample_dir, "StarDrifter_Galactic", 2)
            .await?;
        manager.add_torrent_file(&t1).await?;
        manager.add_torrent_file(&t2).await?;
        if !watched_dirs.contains(&sample_dir) {
            watched_dirs.push(sample_dir);
        }
    }

    if cli.headless {
        run_headless_loop(manager).await?;
    } else {
        let app = TuiApp::new(manager, local_swarm, watched_dirs);
        run_tui(app).await?;
    }

    Ok(())
}

async fn run_headless_loop(manager: std::sync::Arc<DownloadManager>) -> Result<()> {
    println!(
        "Steam destination directory: {}",
        manager.steam_common_dir.read().await.display()
    );
    loop {
        let jobs = manager.jobs.read().await;
        if jobs.is_empty() {
            println!("No torrents queued. Pass a .torrent file, --src-dir, or --demo.");
            break;
        }

        let mut all_done = true;
        for job in jobs.iter() {
            let snap = job.snapshot(0).await;
            println!(
                "[#{}] {} | {} | {:>5.1}% ({}/{}) | {}/s | Threads: {}/{} {} | Steam Path: {}",
                snap.id,
                snap.name,
                snap.status.label(),
                snap.progress_ratio() * 100.0,
                format_bytes(snap.downloaded_bytes),
                format_bytes(snap.total_bytes),
                format_bytes(snap.speed_bps),
                snap.active_workers,
                snap.target_threads,
                thread_mode_label(snap.is_auto_threads),
                snap.game_install_dir.display()
            );
            if snap.status != DownloadStatus::Completed && snap.status != DownloadStatus::Error {
                all_done = false;
            }
        }
        drop(jobs);

        if all_done {
            println!("All game downloads completed and saved to Steam folder!");
            break;
        }

        tokio::time::sleep(Duration::from_millis(600)).await;
    }
    Ok(())
}
