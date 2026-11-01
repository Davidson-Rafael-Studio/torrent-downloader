use super::app::{InputPrompt, TuiApp};
use crate::job::{DownloadStatus, PieceStatus, TorrentJobSnapshot};
use crate::peer::PeerConnectionStats;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, Borders, Cell, Clear, Gauge, List, ListItem, Paragraph, Row, Table, TableState, Wrap,
};
use ratatui::Frame;
use std::path::{Path, PathBuf};

fn bold(color: Color) -> Style {
    Style::default().fg(color).add_modifier(Modifier::BOLD)
}

fn piece_glyph(state: PieceStatus) -> Span<'static> {
    match state {
        PieceStatus::Completed => Span::styled("█", Style::default().fg(Color::Green)),
        PieceStatus::InProgress => Span::styled("▒", Style::default().fg(Color::Yellow)),
        PieceStatus::Pending => Span::styled("░", Style::default().fg(Color::DarkGray)),
    }
}

pub fn thread_mode_label(is_auto_threads: bool) -> &'static str {
    if is_auto_threads {
        "auto"
    } else {
        "manual"
    }
}

fn status_color(status: DownloadStatus) -> Color {
    match status {
        DownloadStatus::Downloading => Color::Green,
        DownloadStatus::Paused => Color::Yellow,
        DownloadStatus::Completed => Color::Cyan,
        DownloadStatus::ConnectingTrackers => Color::LightBlue,
        DownloadStatus::NoPeersFound | DownloadStatus::Error => Color::Red,
    }
}

pub(super) fn draw_ui(
    frame: &mut Frame,
    app: &TuiApp,
    snapshots: &[TorrentJobSnapshot],
    steam_common_dir: &Path,
) {
    let [header_area, queue_area, details_area, footer_area] = Layout::vertical([
        Constraint::Length(4),
        Constraint::Min(8),
        Constraint::Length(9),
        Constraint::Length(3),
    ])
    .areas(frame.area());

    draw_header(
        frame,
        header_area,
        snapshots,
        steam_common_dir,
        &app.watched_source_dirs,
    );
    draw_queue_table(frame, queue_area, snapshots, app.selected_index);
    match snapshots.get(app.selected_index) {
        Some(selected) => draw_job_details(
            frame,
            details_area,
            selected,
            app.executables_by_job_id.get(&selected.id),
        ),
        None => draw_getting_started(frame, details_area),
    }
    draw_footer(frame, footer_area, &app.status_banner);
    if let Some(prompt) = app.input_prompt {
        draw_input_popup(frame, prompt, &app.input_buffer);
    }
}

fn draw_header(
    frame: &mut Frame,
    area: Rect,
    snapshots: &[TorrentJobSnapshot],
    steam_common_dir: &Path,
    watched_dirs: &[PathBuf],
) {
    let total_speed_bps: u64 = snapshots.iter().map(|snapshot| snapshot.speed_bps).sum();
    let count_with_status = |status: DownloadStatus| {
        snapshots
            .iter()
            .filter(|snapshot| snapshot.status == status)
            .count()
    };

    let watched_dirs_text = if watched_dirs.is_empty() {
        "None (Press 'a' or drag & drop a folder/file)".to_string()
    } else {
        watched_dirs
            .iter()
            .map(|dir| dir.display().to_string())
            .collect::<Vec<_>>()
            .join(", ")
    };

    let header_lines = vec![
        Line::from(vec![
            Span::styled(" STEAM TORRENT DOWNLOADER ", bold(Color::Black).bg(Color::Cyan)),
            Span::raw("  "),
            Span::styled(
                format!("Active: {} ", count_with_status(DownloadStatus::Downloading)),
                bold(Color::Green),
            ),
            Span::styled(
                format!("| Completed: {} ", count_with_status(DownloadStatus::Completed)),
                Style::default().fg(Color::Cyan),
            ),
            Span::styled(
                format!("| Total Speed: {}/s", format_bytes(total_speed_bps)),
                bold(Color::Yellow),
            ),
        ]),
        Line::from(vec![
            Span::styled(" Steam Library Dest: ", Style::default().fg(Color::Gray)),
            Span::styled(steam_common_dir.display().to_string(), bold(Color::White)),
            Span::styled("   | Source Dir(s): ", Style::default().fg(Color::Gray)),
            Span::styled(watched_dirs_text, Style::default().fg(Color::LightBlue)),
        ]),
    ];

    let header = Paragraph::new(header_lines).block(
        Block::default()
            .borders(Borders::ALL)
            .title(" Overview & Steam Path "),
    );
    frame.render_widget(header, area);
}

fn draw_queue_table(
    frame: &mut Frame,
    area: Rect,
    snapshots: &[TorrentJobSnapshot],
    selected_index: usize,
) {
    let column_titles = [
        "#",
        "Game / Torrent Name",
        "Status",
        "Progress",
        "Downloaded / Total",
        "Speed",
        "Threads",
        "Peers",
        "Pieces",
    ];
    let table_header = Row::new(
        column_titles
            .iter()
            .map(|title| Cell::from(*title).style(bold(Color::Cyan))),
    )
    .bottom_margin(1);

    let rows = snapshots.iter().map(|snapshot| {
        Row::new(vec![
            snapshot.id.to_string(),
            snapshot.name.clone(),
            snapshot.status.label().to_string(),
            format!("{:>5.1}%", snapshot.progress_ratio() * 100.0),
            format!(
                "{} / {}",
                format_bytes(snapshot.downloaded_bytes),
                format_bytes(snapshot.total_bytes)
            ),
            format!("{}/s", format_bytes(snapshot.speed_bps)),
            format!(
                "{}/{} {}",
                snapshot.active_workers,
                snapshot.target_threads,
                thread_mode_label(snapshot.is_auto_threads)
            ),
            format!("{}/{}", snapshot.available_peer_count, snapshot.peer_count),
            format!("{}/{}", snapshot.completed_pieces, snapshot.total_pieces),
        ])
        .style(Style::default().fg(status_color(snapshot.status)))
    });

    let column_widths = [
        Constraint::Length(4),
        Constraint::Percentage(26),
        Constraint::Length(18),
        Constraint::Length(9),
        Constraint::Length(21),
        Constraint::Length(12),
        Constraint::Length(14),
        Constraint::Length(9),
        Constraint::Length(12),
    ];

    let table = Table::new(rows, column_widths)
        .header(table_header)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(" Multi-Download Queue (Drag & drop .torrent or folder here, or press 'g' for sample game) "),
        )
        .row_highlight_style(Style::default().bg(Color::DarkGray).add_modifier(Modifier::BOLD))
        .highlight_symbol("▶ ");

    let mut table_state = TableState::default();
    if !snapshots.is_empty() {
        table_state.select(Some(selected_index));
    }
    frame.render_stateful_widget(table, area, &mut table_state);
}

fn draw_job_details(
    frame: &mut Frame,
    area: Rect,
    selected: &TorrentJobSnapshot,
    executables_text: Option<&String>,
) {
    let [info_area, peers_area, piece_map_area] = Layout::horizontal([
        Constraint::Percentage(40),
        Constraint::Percentage(30),
        Constraint::Percentage(30),
    ])
    .areas(area);
    draw_connected_peers(frame, peers_area, &selected.connected_peers);
    let [gauge_area, details_list_area] =
        Layout::vertical([Constraint::Length(3), Constraint::Min(4)]).areas(info_area);

    let progress_ratio = selected.progress_ratio();
    let gauge = Gauge::default()
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(format!(" {} Progress ", selected.name)),
        )
        .gauge_style(Style::default().fg(Color::Green).bg(Color::Black))
        .ratio(progress_ratio)
        .label(format!(
            "{:.1}% ({} / {})",
            progress_ratio * 100.0,
            format_bytes(selected.downloaded_bytes),
            format_bytes(selected.total_bytes)
        ));
    frame.render_widget(gauge, gauge_area);

    let short_hash = &selected.info_hash_hex[..16.min(selected.info_hash_hex.len())];
    let details = List::new(vec![
        ListItem::new(format!(
            "Steam Game Path : {}",
            selected.game_install_dir.display()
        )),
        ListItem::new(format!(
            "Game Executables: {}   | Files: {}   | Hash: {}",
            executables_text.map_or("Available after download completes", String::as_str),
            selected.files_count,
            short_hash
        )),
        ListItem::new(format!("Worker Status   : {}", selected.status_message)),
    ])
    .block(
        Block::default()
            .borders(Borders::ALL)
            .title(" Steam Install & Worker Details "),
    );
    frame.render_widget(details, details_list_area);

    let piece_map = Paragraph::new(vec![
        Line::from(vec![
            piece_glyph(PieceStatus::Completed),
            Span::raw(" Verified  "),
            piece_glyph(PieceStatus::InProgress),
            Span::raw(" Active Thread  "),
            piece_glyph(PieceStatus::Pending),
            Span::raw(" Remaining"),
        ]),
        Line::from(
            selected
                .piece_states_preview
                .iter()
                .copied()
                .map(piece_glyph)
                .collect::<Vec<_>>(),
        ),
    ])
    .wrap(Wrap { trim: true })
    .block(Block::default().borders(Borders::ALL).title(format!(
        " Multi-Thread Piece Map ({} Threads) ",
        selected.target_threads
    )));
    frame.render_widget(piece_map, piece_map_area);
}

/// One line per open connection, fastest first, so the user can see whether the limit is
/// choking peers, slow peers, or too few connections.
fn draw_connected_peers(frame: &mut Frame, area: Rect, connected_peers: &[PeerConnectionStats]) {
    let choked_count = connected_peers
        .iter()
        .filter(|peer| peer.is_choked)
        .count();
    let peer_lines: Vec<ListItem> = connected_peers
        .iter()
        .map(|peer| {
            let transfer = if peer.is_choked {
                Span::styled("CHOKED", Style::default().fg(Color::Yellow))
            } else {
                Span::styled(
                    format!("{}/s", format_bytes(peer.download_bps)),
                    Style::default().fg(Color::Green),
                )
            };
            ListItem::new(Line::from(vec![
                Span::raw(format!("{:<22}", peer.addr.to_string())),
                transfer,
                Span::styled(
                    format!("  q{}", peer.request_queue_limit),
                    Style::default().fg(Color::DarkGray),
                ),
            ]))
        })
        .collect();

    let peers = List::new(peer_lines).block(Block::default().borders(Borders::ALL).title(format!(
        " Peers: {} connected, {} choked ",
        connected_peers.len(),
        choked_count
    )));
    frame.render_widget(peers, area);
}

fn draw_getting_started(frame: &mut Frame, area: Rect) {
    let getting_started = Paragraph::new(
        "No torrents in queue yet.\n• Drag & drop a .torrent file or directory into this terminal window\n• Press 'a' to type/paste a .torrent file or directory path\n• Press 'g' to generate & download a sample Steam game torrent immediately!",
    )
    .block(
        Block::default()
            .borders(Borders::ALL)
            .title(" Getting Started "),
    );
    frame.render_widget(getting_started, area);
}

fn draw_footer(frame: &mut Frame, area: Rect, status_banner: &str) {
    let key_hints = [
        (" [a/Drop]", " Add File/Dir  ", Color::Yellow),
        ("[Space/p]", " Pause/Resume  ", Color::Yellow),
        ("[P/R]", " Pause/Resume All  ", Color::Yellow),
        ("[+/-]", " Threads  ", Color::Yellow),
        ("[t]", " Auto Threads  ", Color::Yellow),
        ("[g]", " Demo Game  ", Color::Green),
        ("[d]", " Steam Dir  ", Color::Yellow),
        ("[x]", " Remove  ", Color::Red),
        ("[q]", " Quit", Color::Red),
    ];
    let key_hint_spans: Vec<Span> = key_hints
        .into_iter()
        .flat_map(|(key, action, color)| [Span::styled(key, bold(color)), Span::raw(action)])
        .collect();

    let footer = Paragraph::new(vec![
        Line::from(vec![
            Span::styled(" Status: ", bold(Color::Cyan)),
            Span::styled(status_banner, Style::default().fg(Color::White)),
        ]),
        Line::from(key_hint_spans),
    ])
    .block(Block::default().borders(Borders::ALL));
    frame.render_widget(footer, area);
}

fn draw_input_popup(frame: &mut Frame, prompt: InputPrompt, input_buffer: &str) {
    let popup_area = centered_rect(75, 28, frame.area());
    frame.render_widget(Clear, popup_area);

    let popup_text = vec![
        Line::from(Span::styled(prompt.hint(), Style::default().fg(Color::Gray))),
        Line::from(""),
        Line::from(vec![
            Span::styled("> ", bold(Color::Cyan)),
            Span::styled(input_buffer, bold(Color::Yellow)),
            Span::styled("█", Style::default().fg(Color::Cyan)),
        ]),
    ];

    let popup = Paragraph::new(popup_text).wrap(Wrap { trim: false }).block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Cyan))
            .title(Span::styled(prompt.title(), bold(Color::Cyan))),
    );
    frame.render_widget(popup, popup_area);
}

fn centered_rect(percent_x: u16, percent_y: u16, area: Rect) -> Rect {
    let [_, middle_row, _] = Layout::vertical([
        Constraint::Percentage((100 - percent_y) / 2),
        Constraint::Percentage(percent_y),
        Constraint::Percentage((100 - percent_y) / 2),
    ])
    .areas(area);
    let [_, centered, _] = Layout::horizontal([
        Constraint::Percentage((100 - percent_x) / 2),
        Constraint::Percentage(percent_x),
        Constraint::Percentage((100 - percent_x) / 2),
    ])
    .areas(middle_row);
    centered
}

pub fn format_bytes(bytes: u64) -> String {
    const KIB: f64 = 1024.0;
    const MIB: f64 = KIB * 1024.0;
    const GIB: f64 = MIB * 1024.0;
    let bytes_f64 = bytes as f64;
    if bytes_f64 >= GIB {
        format!("{:.2} GB", bytes_f64 / GIB)
    } else if bytes_f64 >= MIB {
        format!("{:.2} MB", bytes_f64 / MIB)
    } else if bytes_f64 >= KIB {
        format!("{:.1} KB", bytes_f64 / KIB)
    } else {
        format!("{bytes} B")
    }
}
