mod app;
mod draw;

pub use app::TuiApp;
pub use draw::{format_bytes, thread_mode_label};

use anyhow::Result;
use crossterm::event::{self, DisableBracketedPaste, EnableBracketedPaste, Event, KeyEventKind};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use draw::draw_ui;
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;
use std::io;
use std::time::{Duration, Instant};

const DIR_RESCAN_INTERVAL: Duration = Duration::from_secs(3);
const INPUT_POLL_TIMEOUT: Duration = Duration::from_millis(80);

pub async fn run_tui(mut app: TuiApp) -> Result<()> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableBracketedPaste)?;
    let mut terminal = Terminal::new(CrosstermBackend::new(stdout))?;

    let result = run_event_loop(&mut terminal, &mut app).await;

    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        DisableBracketedPaste
    )?;
    terminal.show_cursor()?;

    result
}

async fn run_event_loop(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    app: &mut TuiApp,
) -> Result<()> {
    let mut last_dir_scan_at = Instant::now();

    loop {
        let snapshots = app.collect_snapshots().await;
        if !snapshots.is_empty() && app.selected_index >= snapshots.len() {
            app.selected_index = snapshots.len() - 1;
        }
        app.cache_selected_executables(&snapshots);

        {
            let steam_common_dir = app.manager.steam_common_dir.read().await;
            let app_view: &TuiApp = app;
            terminal.draw(|frame| draw_ui(frame, app_view, &snapshots, &steam_common_dir))?;
        }

        if last_dir_scan_at.elapsed() >= DIR_RESCAN_INTERVAL {
            app.rescan_watched_dirs().await;
            last_dir_scan_at = Instant::now();
        }

        // crossterm's poll blocks the thread; block_in_place lets tokio move the download
        // tasks queued on this worker elsewhere while we wait for input.
        if !tokio::task::block_in_place(|| event::poll(INPUT_POLL_TIMEOUT))? {
            continue;
        }
        let should_quit = match event::read()? {
            Event::Paste(pasted) => {
                app.handle_paste(&pasted).await;
                false
            }
            Event::Key(key) if key.kind == KeyEventKind::Press => {
                app.handle_key(key, &snapshots).await
            }
            _ => false,
        };
        if should_quit {
            return Ok(());
        }
    }
}
