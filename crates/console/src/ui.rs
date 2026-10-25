//! Renders the dashboard: header, torrent table, detail pane, footer, and
//! modal dialogs.

use crate::app::{AddField, App, DetailTab, Filter, LAYOUTS, LabelKind, Mode, Sort};
use crate::format;
use crate::input::PathInput;
use empirebt_app::{DhtState, Level, SeedStatus, Status, TorrentView as RowView};
use empirebt_backend::TorrentFormat;
use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Flex, Layout, Position, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Cell, Clear, Padding, Paragraph, Row, Table, Wrap};

const ACCENT: Color = Color::Cyan;
const MUTED: Color = Color::DarkGray;
const DOWN: Color = Color::LightBlue;
const UP: Color = Color::LightGreen;
const BAR_WIDTH: usize = 10;

pub fn draw(frame: &mut Frame, app: &mut App) {
    let [header, body, footer] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(0),
        Constraint::Length(1),
    ])
    .areas(frame.area());

    draw_header(frame, app, header);
    if app.total() == 0 {
        draw_welcome(frame, app, body);
    } else {
        let detail_height = if body.height >= 24 { 11 } else { 8 };
        let [list, detail] =
            Layout::vertical([Constraint::Min(5), Constraint::Length(detail_height)]).areas(body);
        draw_table(frame, app, list);
        draw_detail(frame, app, detail);
    }
    draw_footer(frame, app, footer);

    match &app.mode {
        Mode::Normal => {}
        Mode::Help => draw_help(frame, app),
        Mode::WebInfo => draw_web_info(frame, app),
        Mode::ConfirmRemove { .. } => draw_confirm_remove(frame, app),
        Mode::Add { .. } => draw_add(frame, app),
        Mode::Settings { .. } => draw_settings(frame, app),
        Mode::Label { .. } => draw_label(frame, app),
        Mode::Filter { .. } => draw_filter(frame, app),
        Mode::Sort { .. } => draw_sort(frame, app),
        Mode::PickFile { .. } => draw_pick_file(frame, app),
        Mode::Rename { .. } => draw_rename(frame, app),
    }
}

fn status_color(status: Status) -> Color {
    match status {
        Status::Downloading => DOWN,
        Status::Seeding => UP,
        Status::Checking | Status::Announcing | Status::Waiting => Color::Yellow,
        Status::Retrying | Status::Pausing => Color::LightYellow,
        Status::Queued => Color::Magenta,
        Status::Metadata => Color::LightMagenta,
        Status::Paused => Color::Gray,
        Status::Error => Color::LightRed,
    }
}

fn status_icon(status: Status) -> &'static str {
    match status {
        Status::Downloading => "↓",
        Status::Seeding => "↑",
        Status::Checking => "◌",
        Status::Queued => "·",
        Status::Metadata => "◇",
        Status::Announcing | Status::Waiting => "…",
        Status::Retrying => "↻",
        Status::Pausing | Status::Paused => "‖",
        Status::Error => "✕",
    }
}

fn format_label(format: TorrentFormat) -> &'static str {
    match format {
        TorrentFormat::V1 => "v1",
        TorrentFormat::V2 => "v2",
        TorrentFormat::Hybrid => "hybrid",
    }
}

fn rounded(title: impl Into<Line<'static>>) -> Block<'static> {
    Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(MUTED))
        .title(title)
}

/// A smooth bar using eighth-block glyphs, so slow transfers still move.
fn progress_bar(fraction: f64, width: usize, color: Color) -> Vec<Span<'static>> {
    const PARTIAL: [&str; 8] = ["", "▏", "▎", "▍", "▌", "▋", "▊", "▉"];
    let eighths = (fraction.clamp(0.0, 1.0) * (width * 8) as f64).round() as usize;
    let full = eighths / 8;
    let partial = PARTIAL[eighths % 8];
    let empty = width - full - usize::from(!partial.is_empty());
    vec![
        Span::styled("█".repeat(full) + partial, Style::new().fg(color)),
        Span::styled("░".repeat(empty), Style::new().fg(MUTED)),
    ]
}

fn draw_header(frame: &mut Frame, app: &App, area: Rect) {
    let (down, up) = app.totals();
    let (down_limit, up_limit) = app.rate_limits();
    let limit = |bytes_per_second: u64| {
        let text = match bytes_per_second {
            0 => String::new(),
            limit => format!(" / {}", format::rate(limit as f64)),
        };
        Span::styled(text, Style::new().fg(MUTED))
    };
    let left = Line::from(vec![
        Span::styled(
            " ◆ EmpireBT ",
            Style::new().fg(Color::Black).bg(ACCENT).bold(),
        ),
        Span::raw("  "),
        Span::styled("↓ ", Style::new().fg(DOWN).bold()),
        Span::raw(rate_or_zero(down)),
        limit(down_limit),
        Span::raw("   "),
        Span::styled("↑ ", Style::new().fg(UP).bold()),
        Span::raw(rate_or_zero(up)),
        limit(up_limit),
    ]);

    let port = app.listen_port();
    let mut extras = String::new();
    if app.utp_running() {
        extras.push_str(" · uTP");
    }
    if app.lsd_running() {
        extras.push_str(" · LSD");
    }
    if app.web.is_some() {
        extras.push_str(" · web");
    }
    let dht = match (app.dht_state(), app.dht_nodes()) {
        (DhtState::Running, Some(nodes)) => {
            let ipv6 = match app.dht_ipv6_nodes() {
                0 => String::new(),
                count => format!(" ({count} IPv6)"),
            };
            Some((format!("DHT {nodes} nodes{ipv6}{extras}   "), MUTED))
        }
        (DhtState::Failed(_), _) => Some((format!("DHT off{extras}   "), Color::LightRed)),
        _ if !extras.is_empty() => Some((format!("{}   ", &extras[" · ".len()..]), MUTED)),
        _ => None,
    };
    // The bound interface goes before the port; while it is down, that is
    // what the header says, since nothing can connect.
    let (interface, interface_up) = app.interface();
    let port = match interface {
        Some(name) => format!("{name}:{port}"),
        None => format!(":{port}"),
    };
    let (dot, listener, color) = match app.seed_status() {
        _ if !interface_up => (
            "✕",
            format!("{} is down · not connecting", interface.unwrap_or_default()),
            Color::LightRed,
        ),
        SeedStatus::Listening { torrents } => {
            let peers = match app.seeding_peers() {
                0 => String::new(),
                1 => " · 1 peer".to_owned(),
                count => format!(" · {count} peers"),
            };
            if torrents == 0 {
                ("●", format!("listening on {port}{peers}"), MUTED)
            } else {
                ("●", format!("seeding {torrents}{peers} on {port}"), UP)
            }
        }
        SeedStatus::Starting => ("◌", "preparing seeding".to_owned(), Color::Yellow),
        SeedStatus::Idle => ("○", format!("port {}", port.trim_start_matches(':')), MUTED),
        SeedStatus::Failed(error) => ("✕", error, Color::LightRed),
    };
    let counts = Span::styled(
        format!(
            "{} torrent{} · {} active · {} seeding   ",
            app.total(),
            if app.total() == 1 { "" } else { "s" },
            app.active_count(),
            app.seeding_count()
        ),
        Style::new().fg(MUTED),
    );
    let mut right = Line::from(vec![
        counts,
        dht.map(|(text, color)| Span::styled(text, Style::new().fg(color)))
            .unwrap_or_default(),
        Span::styled(format!("{dot} "), Style::new().fg(color)),
        Span::styled(listener, Style::new().fg(color)),
        Span::raw(" "),
    ])
    .alignment(Alignment::Right);
    // On narrow terminals the listener state matters more than the counts,
    // then the DHT node count.
    for _ in 0..2 {
        if left.width() + right.width() + 1 > area.width as usize {
            right.spans.remove(0);
        }
    }

    let [left_area, right_area] =
        Layout::horizontal([Constraint::Length(left.width() as u16), Constraint::Fill(1)])
            .areas(area);
    frame.render_widget(left, left_area);
    frame.render_widget(right, right_area);
}

fn rate_or_zero(rate: f64) -> String {
    let text = format::rate(rate);
    if text.is_empty() {
        "0 B/s".to_owned()
    } else {
        text
    }
}

fn draw_welcome(frame: &mut Frame, app: &App, area: Rect) {
    let lines = vec![
        Line::from(Span::styled("◆ EmpireBT", Style::new().fg(ACCENT).bold())),
        Line::raw(""),
        Line::from("No torrents yet.".bold()),
        Line::raw(""),
        Line::from(vec![
            Span::raw("Press "),
            Span::styled(" a ", Style::new().fg(Color::Black).bg(ACCENT).bold()),
            Span::raw(
                " to add a .torrent file, URL, or magnet link, or paste one into this window.",
            ),
        ]),
        Line::raw(""),
        Line::from(Span::styled(
            format!("Downloads are saved to {}", app.download_dir().display()),
            Style::new().fg(MUTED),
        )),
    ];
    let height = lines.len() as u16 + 4;
    let block = rounded("").padding(Padding::uniform(1));
    let area = centered(area, 72, height);
    frame.render_widget(
        Paragraph::new(lines)
            .alignment(Alignment::Center)
            .wrap(Wrap { trim: true })
            .block(block),
        area,
    );
}

fn draw_table(frame: &mut Frame, app: &mut App, area: Rect) {
    // The extra columns only once names keep room to be read.
    let wide = area.width >= 130;
    let header_cells: Vec<&str> = if wide {
        vec![
            "", "Name", "Size", "Progress", "Down", "Up", "Peers", "ETA", "Status",
        ]
    } else {
        vec!["", "Name", "Size", "Progress", "Down", "Status"]
    };
    let header = Row::new(header_cells)
        .style(Style::new().fg(MUTED).add_modifier(Modifier::BOLD))
        .bottom_margin(0);

    let rows: Vec<Row> = app
        .rows()
        .map(|row| {
            let color = status_color(row.status);
            let progress = match row.fraction {
                Some(fraction) => {
                    let mut spans = progress_bar(fraction, BAR_WIDTH, color);
                    spans.push(Span::raw(format!(" {:>5.1}%", fraction * 100.0)));
                    Line::from(spans)
                }
                None => Line::from(Span::styled(
                    format!("{}  —", "░".repeat(BAR_WIDTH)),
                    Style::new().fg(MUTED),
                )),
            };
            let mut cells = vec![
                Cell::from(Span::styled(
                    status_icon(row.status),
                    Style::new().fg(color).bold(),
                )),
                Cell::from(name_with_labels(row)),
                Cell::from(
                    Line::from(row.size().map_or("—".to_owned(), format::bytes)).right_aligned(),
                ),
                Cell::from(progress),
                Cell::from(
                    Line::from(Span::styled(
                        format::rate(row.down_rate),
                        Style::new().fg(DOWN),
                    ))
                    .right_aligned(),
                ),
            ];
            if wide {
                cells.push(Cell::from(
                    Line::from(Span::styled(format::rate(row.up_rate), Style::new().fg(UP)))
                        .right_aligned(),
                ));
                cells.push(Cell::from(
                    Line::from(row.peers.map(|peers| peers.to_string()).unwrap_or_default())
                        .right_aligned(),
                ));
                cells.push(Cell::from(
                    Line::from(format::eta(row.remaining(), row.down_rate)).right_aligned(),
                ));
            }
            cells.push(Cell::from(Span::styled(
                row.status.label(),
                Style::new().fg(color),
            )));
            Row::new(cells)
        })
        .collect();

    let mut widths = vec![
        Constraint::Length(1),
        Constraint::Fill(1),
        Constraint::Length(9),
        Constraint::Length(BAR_WIDTH as u16 + 7),
        Constraint::Length(11),
    ];
    if wide {
        widths.extend([
            Constraint::Length(11),
            Constraint::Length(5),
            Constraint::Length(8),
        ]);
    }
    widths.push(Constraint::Length(11));

    let mut title = match &app.filter {
        Filter::All => Line::from(vec![
            Span::styled(" Torrents ", Style::new().fg(ACCENT).bold()),
            Span::styled(format!("{} ", app.len()), Style::new().fg(MUTED)),
        ]),
        filter => Line::from(vec![
            Span::styled(" Torrents ", Style::new().fg(ACCENT).bold()),
            Span::styled(
                format!("· {} ", filter.label()),
                Style::new().fg(Color::Yellow),
            ),
            Span::styled(
                format!("{} of {}  (Esc shows all) ", app.len(), app.total()),
                Style::new().fg(MUTED),
            ),
        ]),
    };
    if app.sort != Sort::Added {
        title.push_span(Span::styled(
            format!("· by {} ", app.sort.label().to_lowercase()),
            Style::new().fg(ACCENT),
        ));
    }
    let empty = rows.is_empty();
    let table = Table::new(rows, widths)
        .header(header)
        .column_spacing(2)
        .row_highlight_style(
            Style::new()
                .bg(Color::Indexed(236))
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol(Span::styled("▌", Style::new().fg(ACCENT)))
        .block(rounded(title).padding(Padding::horizontal(1)));
    frame.render_stateful_widget(table, area, &mut app.table);
    // Only a filter leaves the list empty; say so across the whole width,
    // under the header row.
    if empty {
        let [_, message] = Layout::vertical([Constraint::Length(2), Constraint::Min(0)])
            .areas(area.inner(ratatui::layout::Margin::new(2, 1)));
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "No torrents match this filter: press f to choose another, Esc to show all",
                Style::new().fg(MUTED),
            )))
            .wrap(Wrap { trim: true }),
            message,
        );
    }
}

fn draw_detail(frame: &mut Frame, app: &App, area: Rect) {
    let mut tabs = vec![Span::raw(" ")];
    for (position, tab) in DetailTab::ALL.iter().enumerate() {
        if position > 0 {
            tabs.push(Span::styled(" │ ", Style::new().fg(MUTED)));
        }
        let style = if *tab == app.tab {
            Style::new().fg(ACCENT).bold().underlined()
        } else {
            Style::new().fg(Color::Gray)
        };
        tabs.push(Span::styled(tab.title(), style));
    }
    tabs.push(Span::raw(" "));
    let block = rounded(Line::from(tabs)).padding(Padding::horizontal(1));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let Some(row) = app.selected().and_then(|index| app.row(index)) else {
        return;
    };
    match app.tab {
        DetailTab::Overview => draw_overview(frame, row, inner),
        DetailTab::Files => draw_files(frame, row, inner),
        DetailTab::Peers => draw_peers(frame, app, inner),
        DetailTab::Trackers => draw_trackers(frame, app, row, inner),
    }
}

fn field(label: &str, value: impl Into<Line<'static>>) -> Line<'static> {
    let mut line = value.into();
    line.spans.insert(
        0,
        Span::styled(format!("{label:<10}"), Style::new().fg(MUTED)),
    );
    line
}

fn draw_overview(frame: &mut Frame, row: &RowView, area: Rect) {
    let color = status_color(row.status);
    let total_pieces = row
        .info
        .as_ref()
        .map_or(0, |info| info.piece_count().max(info.v2_piece_count()));
    let progress = match (row.info.as_ref(), row.fraction, row.verified_pieces) {
        (None, ..) => "metadata comes from peers once the magnet link is started".to_owned(),
        (Some(_), Some(fraction), Some(pieces)) => {
            format!(
                "{:.1}%  ·  {pieces} / {total_pieces} pieces",
                fraction * 100.0
            )
        }
        _ => "unknown until local data is checked".to_owned(),
    };
    let remaining = row.remaining();
    let size = match row.size() {
        None => "unknown until the metadata arrives".to_owned(),
        Some(total) if row.fraction.is_some() && remaining > 0 => format!(
            "{}  ({} left)",
            format::bytes(total),
            format::bytes(remaining)
        ),
        Some(total) => format::bytes(total),
    };
    let left = vec![
        field(
            "Status",
            Span::styled(row.status.label(), Style::new().fg(color).bold()),
        ),
        field("Progress", Line::raw(progress)),
        field("Size", Line::raw(size)),
        field(
            "Speed",
            Line::from(vec![
                Span::styled("↓ ", Style::new().fg(DOWN)),
                Span::raw(rate_or_zero(row.down_rate)),
                Span::raw("   "),
                Span::styled("↑ ", Style::new().fg(UP)),
                Span::raw(rate_or_zero(row.up_rate)),
                Span::styled(
                    row.announce
                        .as_ref()
                        .filter(|status| status.uploaded > 0)
                        .map(|status| format!("  ({} sent)", format::bytes(status.uploaded)))
                        .unwrap_or_default(),
                    Style::new().fg(MUTED),
                ),
            ]),
        ),
        field("Peers", Line::raw(peer_summary(row))),
        field(
            "ETA",
            Line::raw(match format::eta(remaining, row.down_rate) {
                eta if eta.is_empty() => "—".to_owned(),
                eta => eta,
            }),
        ),
    ];

    let mut hash = format::hex(&row.hash);
    if let Some(v2) = row.info.as_ref().and_then(|info| info.v2_info_hash()) {
        hash = format!("{hash}  (v2 {}…)", &format::hex(v2)[..16]);
    }
    let save_to = field(
        "Save to",
        Line::raw(row.save_dir.join(&row.name).display().to_string()),
    );
    let right = match &row.info {
        Some(info) => vec![
            field("Info hash", Line::raw(hash)),
            field(
                "Format",
                Line::raw(if info.is_private() {
                    format!(
                        "{}  ·  private (trackers only, no DHT)",
                        format_label(info.format())
                    )
                } else {
                    format_label(info.format()).to_owned()
                }),
            ),
            field(
                "Pieces",
                Line::raw(format!(
                    "{total_pieces} × {}",
                    format::bytes(u64::from(info.piece_length()))
                )),
            ),
            field(
                "Files",
                Line::raw(
                    info.files()
                        .iter()
                        .filter(|file| !file.is_padding())
                        .count()
                        .to_string(),
                ),
            ),
            save_to,
            labels_field(row),
        ],
        None => vec![
            field("Info hash", Line::raw(hash)),
            field("Source", Line::raw("magnet link")),
            save_to,
            labels_field(row),
        ],
    };

    let [columns, error_area] = Layout::vertical([
        Constraint::Min(0),
        Constraint::Length(u16::from(row.error.is_some())),
    ])
    .areas(area);
    let left_width = left.iter().map(Line::width).max().unwrap_or(0) as u16;
    let [left_area, right_area] =
        Layout::horizontal([Constraint::Length(left_width), Constraint::Fill(1)])
            .spacing(4)
            .areas(columns);
    frame.render_widget(Paragraph::new(left), left_area);
    frame.render_widget(Paragraph::new(right), right_area);
    if let Some(error) = &row.error {
        frame.render_widget(
            Line::from(vec![
                Span::styled("⚠ ", Style::new().fg(Color::LightRed).bold()),
                Span::styled(error.clone(), Style::new().fg(Color::LightRed)),
            ]),
            error_area,
        );
    }
}

fn draw_files(frame: &mut Frame, row: &RowView, area: Rect) {
    if row.info.is_none() {
        frame.render_widget(
            Line::from(Span::styled(
                "The file list appears once the metadata arrives from peers.",
                Style::new().fg(MUTED),
            )),
            area,
        );
        return;
    }
    let files = crate::app::file_entries(row);
    let visible = area.height as usize;
    let shown = if files.len() > visible {
        visible.saturating_sub(1)
    } else {
        files.len()
    };
    let mut rows: Vec<Row> = files
        .iter()
        .take(shown)
        .map(|file| {
            Row::new([
                Cell::from(file_line(file, false)),
                Cell::from(Line::from(format::bytes(file.size)).right_aligned()),
            ])
        })
        .collect();
    if shown < files.len() {
        rows.push(Row::new([Cell::from(Span::styled(
            format!("… and {} more", files.len() - shown),
            Style::new().fg(MUTED),
        ))]));
    }
    frame.render_widget(
        Table::new(rows, [Constraint::Fill(1), Constraint::Length(10)]).column_spacing(2),
        area,
    );
}

/// A file or folder, indented under the folder it is in.
fn file_line(file: &crate::app::FileEntry, chosen: bool) -> Line<'static> {
    let indent = "  ".repeat(file.depth);
    let mut spans = vec![Span::raw(indent)];
    if file.folder {
        spans.push(Span::styled(
            format!("{}/", file.name),
            Style::new().fg(Color::Yellow).bold(),
        ));
    } else {
        spans.push(Span::raw(file.name.clone()));
    }
    if file.renamed {
        spans.push(Span::styled("  renamed", Style::new().fg(MUTED)));
    }
    let line = Line::from(spans);
    if chosen {
        line.style(Style::new().bg(Color::Indexed(236)).bold())
    } else {
        line
    }
}

fn draw_pick_file(frame: &mut Frame, app: &App) {
    let Mode::PickFile { selected, .. } = &app.mode else {
        return;
    };
    let entries = app.picked_files();
    let shown = entries.len().clamp(1, 16);
    let first = (*selected + 1).saturating_sub(shown);
    let height = shown as u16 + 5;
    let inner = dialog(frame, "Rename which file or folder?", 76, height);
    let [list, hint] =
        Layout::vertical([Constraint::Length(shown as u16), Constraint::Length(2)]).areas(inner);
    let lines: Vec<Line> = entries
        .iter()
        .enumerate()
        .skip(first)
        .take(shown)
        .map(|(index, entry)| {
            let chosen = index == *selected;
            let mut line = file_line(entry, chosen);
            line.spans.insert(
                0,
                Span::styled(if chosen { "▌ " } else { "  " }, Style::new().fg(ACCENT)),
            );
            line
        })
        .collect();
    frame.render_widget(Paragraph::new(lines), list);
    frame.render_widget(
        Paragraph::new(vec![
            Line::raw(""),
            key_hints(&[("↑↓", "select"), ("Enter", "rename"), ("Esc", "close")]),
        ]),
        hint,
    );
}

fn draw_rename(frame: &mut Frame, app: &App) {
    let Mode::Rename {
        torrent,
        path,
        input,
        error,
    } = &app.mode
    else {
        return;
    };
    let row = app.rows().find(|row| row.hash == *torrent);
    let (title, subject, about) = match (path, row) {
        (None, row) => (
            "Rename torrent",
            row.map_or(String::new(), |row| row.original_name.clone()),
            "Only the name shown changes; leave it empty for the torrent's own name.".to_owned(),
        ),
        (Some(path), _) => {
            let folder = row.is_some_and(|row| {
                row.files()
                    .iter()
                    .any(|file| file.path != *path && file.path.starts_with(path))
            });
            (
                if folder {
                    "Rename folder"
                } else {
                    "Rename file"
                },
                path.display().to_string(),
                "Renamed on disk, and it keeps seeding. A / moves it into a folder.".to_owned(),
            )
        }
    };
    let height = 10 + u16::from(error.is_some());
    let inner = dialog(frame, title, 76, height);
    let [label, field, error_area, hint] = Layout::vertical([
        Constraint::Length(2),
        Constraint::Length(3),
        Constraint::Length(u16::from(error.is_some())),
        Constraint::Length(2),
    ])
    .areas(inner);
    frame.render_widget(
        Paragraph::new(vec![
            Line::from(Span::styled(subject, Style::new().bold())),
            Line::from(Span::styled(about, Style::new().fg(Color::Gray))),
        ]),
        label,
    );
    let field_block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(MUTED));
    let field_inner = field_block.inner(field);
    frame.render_widget(field_block, field);
    let prompt = "› ";
    let room = (field_inner.width as usize).saturating_sub(prompt.len() + 1);
    let offset = input.cursor().saturating_sub(room);
    let visible: String = input.text().chars().skip(offset).take(room + 1).collect();
    frame.render_widget(
        Line::from(vec![
            Span::styled(prompt, Style::new().fg(ACCENT).bold()),
            Span::raw(visible),
        ]),
        field_inner,
    );
    frame.set_cursor_position(Position::new(
        field_inner.x + (prompt.chars().count() + input.cursor() - offset) as u16,
        field_inner.y,
    ));
    if let Some(error) = error {
        frame.render_widget(
            Line::from(Span::styled(
                format!("✕ {error}"),
                Style::new().fg(Color::LightRed),
            )),
            error_area,
        );
    }
    frame.render_widget(
        Paragraph::new(vec![
            Line::raw(""),
            key_hints(&[("Enter", "rename"), ("Esc", "cancel")]),
        ]),
        hint,
    );
}

fn draw_peers(frame: &mut Frame, app: &App, area: Rect) {
    let Some(peers) = app.peers() else {
        return;
    };
    if peers.is_empty() {
        frame.render_widget(
            Line::from(Span::styled("No peers connected.", Style::new().fg(MUTED))),
            area,
        );
        return;
    }
    // The last line credits the country database.
    let visible = (area.height as usize).saturating_sub(2);
    let shown = if peers.len() > visible {
        visible.saturating_sub(1)
    } else {
        peers.len()
    };
    // Sorted by what we upload to each, then what each sends us.
    let header = Row::new([
        "", "Address", "Client", "Flags", "Done", "Down", "Up ▼", "Sent",
    ])
    .style(Style::new().fg(MUTED));
    let mut rows: Vec<Row> = peers
        .iter()
        .take(shown)
        .map(|peer| {
            let info = &peer.info;
            // qBittorrent's flags, such as "D U I E P" (see `?`).
            let flags = info
                .flags()
                .iter()
                .map(|(flag, _)| flag.to_string())
                .collect::<Vec<_>>()
                .join(" ");
            let progress = info.progress.map_or(String::new(), |progress| {
                format!("{:.0}%", progress * 100.0)
            });
            let rate = |value: f64, color: Color| {
                let text = format::rate(value);
                Cell::from(Line::styled(text, Style::new().fg(color)).right_aligned())
            };
            let sent = if info.uploaded > 0 {
                format::bytes(info.uploaded)
            } else {
                String::new()
            };
            let row = Row::new([
                Cell::from(peer.country_code().unwrap_or("").to_owned()),
                Cell::from(info.address.to_string()),
                Cell::from(info.client.clone().unwrap_or_default()),
                Cell::from(Span::styled(flags, Style::new().fg(MUTED))),
                Cell::from(Line::from(progress).right_aligned()),
                rate(info.down_rate, DOWN),
                rate(info.up_rate, UP),
                Cell::from(Line::from(sent).right_aligned()),
            ]);
            // Peers we are sending to right now stand out.
            if info.up_rate >= 1.0 {
                row.style(Style::new().fg(UP))
            } else {
                row
            }
        })
        .collect();
    if shown < peers.len() {
        rows.push(Row::new([
            Cell::from(""),
            Cell::from(Span::styled(
                format!("… and {} more", peers.len() - shown),
                Style::new().fg(MUTED),
            )),
        ]));
    }
    let [table_area, credit_area] =
        Layout::vertical([Constraint::Fill(1), Constraint::Length(1)]).areas(area);
    frame.render_widget(
        Table::new(
            rows,
            [
                Constraint::Length(2),
                Constraint::Length(22),
                Constraint::Fill(1),
                Constraint::Length(14),
                Constraint::Length(4),
                Constraint::Length(10),
                Constraint::Length(10),
                Constraint::Length(10),
            ],
        )
        .header(header)
        .column_spacing(1),
        table_area,
    );
    if peers.iter().any(|peer| peer.country.is_some()) {
        frame.render_widget(
            Line::styled(empirebt_app::GEOIP_ATTRIBUTION, Style::new().fg(MUTED)).right_aligned(),
            credit_area,
        );
    }
}

fn peer_summary(row: &RowView) -> String {
    let connected = row.peers.map(|peers| format!("{peers} connected"));
    let swarm = match row.swarm {
        (None, None) => None,
        (seeders, leechers) => Some(format!(
            "swarm {} seeders, {} leechers",
            seeders.map_or("?".to_owned(), |count| count.to_string()),
            leechers.map_or("?".to_owned(), |count| count.to_string()),
        )),
    };
    match (connected, swarm) {
        (Some(connected), Some(swarm)) => format!("{connected}  ·  {swarm}"),
        (Some(text), None) | (None, Some(text)) => text,
        (None, None) => "—".to_owned(),
    }
}

/// Lines describing how the latest seeding announces went.
fn announce_lines(status: &empirebt_backend::AnnounceStatus) -> Vec<Line<'static>> {
    let ago = |at: std::time::Instant| format::duration(at.elapsed().as_secs());
    let mut lines = Vec::new();
    if status.trackers_total > 0 {
        let next = format::duration(
            status
                .next_announce
                .saturating_duration_since(std::time::Instant::now())
                .as_secs(),
        );
        let (icon, color) = match status.trackers_ok {
            0 => ("✕", Color::LightRed),
            ok if ok < status.trackers_total => ("◐", Color::Yellow),
            _ => ("✓", UP),
        };
        let mut spans = vec![
            Span::styled(format!("{icon} "), Style::new().fg(color).bold()),
            Span::raw(format!(
                "Seeding announced to {}/{} trackers  ·  next in {next}",
                status.trackers_ok, status.trackers_total
            )),
        ];
        if let Some(error) = &status.last_error {
            spans.push(Span::styled(
                format!("  ·  {error}"),
                Style::new().fg(MUTED),
            ));
        }
        lines.push(Line::from(spans));
    }
    if let Some(at) = status.dht_announced {
        lines.push(Line::from(vec![
            Span::styled("✓ ", Style::new().fg(UP).bold()),
            Span::raw(format!("Announced to the DHT {} ago", ago(at))),
        ]));
    }
    lines
}

fn draw_trackers(frame: &mut Frame, app: &App, row: &RowView, area: Rect) {
    let urls = &row.trackers;
    let dht_running = *app.dht_state() == DhtState::Running;
    let mut lines: Vec<Line> = if urls.is_empty() && dht_running {
        vec![Line::from(Span::styled(
            "No trackers: peers are found through the DHT.",
            Style::new().fg(MUTED),
        ))]
    } else if urls.is_empty() {
        vec![Line::from(Span::styled(
            "This torrent lists no trackers and the DHT is off, so EmpireBT cannot find peers for it.",
            Style::new().fg(Color::Yellow),
        ))]
    } else {
        urls.iter()
            .map(|url| {
                Line::from(vec![
                    Span::styled("• ", Style::new().fg(ACCENT)),
                    Span::raw(url.clone()),
                ])
            })
            .collect()
    };
    if let Some(status) = &row.announce {
        lines.splice(0..0, announce_lines(status));
    }
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: true }), area);
}

fn draw_footer(frame: &mut Frame, app: &App, area: Rect) {
    let mut hints: Vec<(&str, &str)> = if app.total() == 0 {
        vec![
            ("a", "Add"),
            ("c", "Settings"),
            ("?", "Help"),
            ("q", "Quit"),
        ]
    } else {
        vec![
            ("a", "Add"),
            ("s", "Start"),
            ("p", "Pause"),
            ("v", "Recheck"),
            ("x", "Remove"),
            ("f", "Filter"),
            ("S", "Sort"),
            ("c", "Settings"),
            ("⇥", "Details"),
            ("?", "Help"),
            ("q", "Quit"),
        ]
    };
    let render_hints = |hints: &[(&str, &str)]| {
        let mut spans = vec![Span::raw(" ")];
        for (key, label) in hints {
            spans.push(Span::styled(
                format!(" {key} "),
                Style::new().fg(Color::Black).bg(Color::Gray).bold(),
            ));
            spans.push(Span::styled(format!(" {label}  "), Style::new().fg(MUTED)));
        }
        Line::from(spans)
    };
    // Drop the least essential hints first; help and quit always stay.
    for optional in ["⇥", "S", "v", "f", "c", "x"] {
        if render_hints(&hints).width() <= area.width as usize {
            break;
        }
        hints.retain(|(key, _)| *key != optional);
    }
    let hints = render_hints(&hints);

    let notice = app.notice().map(|notice| {
        let (icon, color) = match notice.level {
            Level::Info => ("●", ACCENT),
            Level::Success => ("✓", UP),
            Level::Error => ("✕", Color::LightRed),
        };
        Line::from(vec![
            Span::styled(format!("{icon} "), Style::new().fg(color).bold()),
            Span::styled(notice.text.clone(), Style::new().fg(color)),
            Span::raw(" "),
        ])
        .right_aligned()
    });
    match notice {
        // An error matters more than the key hints when space is short.
        Some(notice) if notice.width() + hints.width() > area.width as usize => {
            frame.render_widget(notice.left_aligned(), area)
        }
        Some(notice) => {
            frame.render_widget(hints, area);
            frame.render_widget(notice, area);
        }
        None => frame.render_widget(hints, area),
    }
}

fn centered(area: Rect, width: u16, height: u16) -> Rect {
    let [area] = Layout::horizontal([Constraint::Length(width.min(area.width))])
        .flex(Flex::Center)
        .areas(area);
    let [area] = Layout::vertical([Constraint::Length(height.min(area.height))])
        .flex(Flex::Center)
        .areas(area);
    area
}

fn dialog(frame: &mut Frame, title: &str, width: u16, height: u16) -> Rect {
    let area = centered(frame.area(), width, height);
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(ACCENT))
        .title(Span::styled(
            format!(" {title} "),
            Style::new().fg(ACCENT).bold(),
        ))
        .padding(Padding::new(2, 2, 1, 0));
    let inner = block.inner(area);
    frame.render_widget(Clear, area);
    frame.render_widget(block, area);
    inner
}

fn draw_add(frame: &mut Frame, app: &App) {
    let Mode::Add {
        input,
        save,
        layout,
        field,
        error,
    } = &app.mode
    else {
        return;
    };
    let candidates = match field {
        AddField::Source => input.candidates(),
        AddField::Save => save.candidates(),
        AddField::Layout => &[],
    };
    let candidate_lines = candidates.len().min(5) as u16;
    let height = 16 + candidate_lines + u16::from(error.is_some());
    let inner = dialog(frame, "Add torrent", 76, height);

    let [
        source_label,
        source_field,
        save_label,
        save_field,
        layout_label,
        layout_field,
        list,
        error_area,
        hint,
    ] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(3),
        Constraint::Length(1),
        Constraint::Length(3),
        Constraint::Length(1),
        Constraint::Length(2),
        Constraint::Length(candidate_lines),
        Constraint::Length(u16::from(error.is_some())),
        Constraint::Length(2),
    ])
    .areas(inner);

    let label = |text: &'static str| Line::from(Span::styled(text, Style::new().fg(Color::Gray)));
    frame.render_widget(
        label("Path or URL of a .torrent file, or a magnet link"),
        source_label,
    );
    draw_input(frame, input, source_field, *field == AddField::Source);
    frame.render_widget(label("Save in"), save_label);
    draw_input(frame, save, save_field, *field == AddField::Save);
    frame.render_widget(label("Content layout"), layout_label);
    let on_layout = *field == AddField::Layout;
    let chosen = LAYOUTS
        .iter()
        .find(|(choice, _)| choice == layout)
        .map_or("", |(_, text)| *text);
    let arrows = Style::new()
        .fg(if on_layout { ACCENT } else { MUTED })
        .bold();
    let mut choice = vec![
        Span::styled("  ‹ ", arrows),
        Span::styled(
            chosen,
            if on_layout {
                Style::new().bold()
            } else {
                Style::new()
            },
        ),
        Span::styled(" ›", arrows),
    ];
    if on_layout {
        choice.push(Span::styled("  ←→ change", Style::new().fg(MUTED)));
    }
    frame.render_widget(Line::from(choice), layout_field);

    let mut shown: Vec<Line> = candidates
        .iter()
        .take(4)
        .map(|candidate| {
            Line::from(Span::styled(
                format!("  {candidate}"),
                Style::new().fg(MUTED),
            ))
        })
        .collect();
    if candidates.len() > 4 {
        shown.push(Line::from(Span::styled(
            format!("  … {} more", candidates.len() - 4),
            Style::new().fg(MUTED),
        )));
    }
    frame.render_widget(Paragraph::new(shown), list);

    if let Some(error) = error {
        frame.render_widget(
            Line::from(Span::styled(
                format!("✕ {error}"),
                Style::new().fg(Color::LightRed),
            )),
            error_area,
        );
    }
    frame.render_widget(
        Paragraph::new(vec![
            Line::raw(""),
            key_hints(&[
                ("Tab", "complete"),
                ("↑↓", "switch field"),
                ("Enter", "add & start"),
                ("Esc", "cancel"),
            ]),
        ]),
        hint,
    );
}

/// A one-line text field in a rounded box, with the cursor when focused.
fn draw_input(frame: &mut Frame, input: &PathInput, area: Rect, focused: bool) {
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(if focused { ACCENT } else { MUTED }));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let prompt = "› ";
    let room = (inner.width as usize).saturating_sub(prompt.len() + 1);
    let offset = input.cursor().saturating_sub(room);
    let visible: String = input.text().chars().skip(offset).take(room + 1).collect();
    let prompt_style = if focused {
        Style::new().fg(ACCENT).bold()
    } else {
        Style::new().fg(MUTED)
    };
    frame.render_widget(
        Line::from(vec![Span::styled(prompt, prompt_style), Span::raw(visible)]),
        inner,
    );
    if focused {
        frame.set_cursor_position(Position::new(
            inner.x + (prompt.chars().count() + input.cursor() - offset) as u16,
            inner.y,
        ));
    }
}

/// A torrent's name, followed by its category and tags.
fn name_with_labels(row: &RowView) -> Line<'static> {
    let mut spans = vec![Span::raw(row.name.clone())];
    if let Some(category) = &row.category {
        spans.push(Span::styled(
            format!("  {category}"),
            Style::new().fg(Color::Yellow),
        ));
    }
    if !row.tags.is_empty() {
        let tags: Vec<String> = row.tags.iter().map(|tag| format!("#{tag}")).collect();
        spans.push(Span::styled(
            format!("  {}", tags.join(" ")),
            Style::new().fg(Color::Magenta),
        ));
    }
    Line::from(spans)
}

fn labels_field(row: &RowView) -> Line<'static> {
    let category = row.category.as_deref().unwrap_or("none");
    let tags = if row.tags.is_empty() {
        "none".to_owned()
    } else {
        row.tags.join(", ")
    };
    field(
        "Labels",
        Line::from(vec![
            Span::styled(category.to_owned(), Style::new().fg(Color::Yellow)),
            Span::styled("  ·  tags ", Style::new().fg(MUTED)),
            Span::styled(tags, Style::new().fg(Color::Magenta)),
        ]),
    )
}

fn draw_label(frame: &mut Frame, app: &App) {
    let Mode::Label {
        torrent,
        kind,
        input,
        error,
    } = &app.mode
    else {
        return;
    };
    let name = app
        .rows()
        .find(|row| row.hash == *torrent)
        .map_or(String::new(), |row| row.name.clone());
    let (title, about) = match kind {
        LabelKind::Tags => ("Tags", "Tags, separated by commas; new ones are created"),
        LabelKind::Category => ("Category", "One category; leave empty for none"),
    };
    let known = app.known_labels(*kind);
    let height = 11 + u16::from(error.is_some());
    let inner = dialog(frame, title, 76, height);
    let [label, field, known_area, error_area, hint] = Layout::vertical([
        Constraint::Length(2),
        Constraint::Length(3),
        Constraint::Length(1),
        Constraint::Length(u16::from(error.is_some())),
        Constraint::Length(2),
    ])
    .areas(inner);
    frame.render_widget(
        Paragraph::new(vec![
            Line::from(Span::styled(name, Style::new().bold())),
            Line::from(Span::styled(about, Style::new().fg(Color::Gray))),
        ]),
        label,
    );

    let field_block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(MUTED));
    let field_inner = field_block.inner(field);
    frame.render_widget(field_block, field);
    let prompt = "› ";
    let room = (field_inner.width as usize).saturating_sub(prompt.len() + 1);
    let offset = input.cursor().saturating_sub(room);
    let visible: String = input.text().chars().skip(offset).take(room + 1).collect();
    frame.render_widget(
        Line::from(vec![
            Span::styled(prompt, Style::new().fg(ACCENT).bold()),
            Span::raw(visible),
        ]),
        field_inner,
    );
    frame.set_cursor_position(Position::new(
        field_inner.x + (prompt.chars().count() + input.cursor() - offset) as u16,
        field_inner.y,
    ));
    let known_text = if known.is_empty() {
        "None yet.".to_owned()
    } else {
        format!("Known: {}", known.join(", "))
    };
    frame.render_widget(
        Line::from(Span::styled(known_text, Style::new().fg(MUTED))),
        known_area,
    );
    if let Some(error) = error {
        frame.render_widget(
            Line::from(Span::styled(
                format!("✕ {error}"),
                Style::new().fg(Color::LightRed),
            )),
            error_area,
        );
    }
    frame.render_widget(
        Paragraph::new(vec![
            Line::raw(""),
            key_hints(&[("Tab", "complete"), ("Enter", "save"), ("Esc", "cancel")]),
        ]),
        hint,
    );
}

fn draw_filter(frame: &mut Frame, app: &App) {
    let Mode::Filter { selected } = &app.mode else {
        return;
    };
    let choices = app.filter_choices();
    let shown = choices.len().min(16);
    // Keep the selected choice in view in a long list.
    let first = (*selected + 1).saturating_sub(shown);
    let height = shown as u16 + 5;
    let inner = dialog(frame, "Show", 52, height);
    let [list, hint] =
        Layout::vertical([Constraint::Length(shown as u16), Constraint::Length(2)]).areas(inner);
    let lines: Vec<Line> = choices
        .iter()
        .enumerate()
        .skip(first)
        .take(shown)
        .map(|(index, choice)| {
            let chosen = index == *selected;
            let color = match choice {
                Filter::Category(_) => Color::Yellow,
                Filter::Tag(_) => Color::Magenta,
                _ => Color::Gray,
            };
            let mut spans = vec![
                Span::styled(if chosen { "▌ " } else { "  " }, Style::new().fg(ACCENT)),
                Span::styled(choice.label(), Style::new().fg(color)),
            ];
            if *choice == app.filter {
                spans.push(Span::styled("  (showing)", Style::new().fg(MUTED)));
            }
            let line = Line::from(spans);
            if chosen {
                line.style(Style::new().bg(Color::Indexed(236)).bold())
            } else {
                line
            }
        })
        .collect();
    frame.render_widget(Paragraph::new(lines), list);
    frame.render_widget(
        Paragraph::new(vec![
            Line::raw(""),
            key_hints(&[("↑↓", "select"), ("Enter", "show"), ("Esc", "close")]),
        ]),
        hint,
    );
}

fn draw_sort(frame: &mut Frame, app: &App) {
    let Mode::Sort { selected } = &app.mode else {
        return;
    };
    let height = Sort::ALL.len() as u16 + 5;
    let inner = dialog(frame, "Sort by", 40, height);
    let [list, hint] = Layout::vertical([
        Constraint::Length(Sort::ALL.len() as u16),
        Constraint::Length(2),
    ])
    .areas(inner);
    let lines: Vec<Line> = Sort::ALL
        .iter()
        .enumerate()
        .map(|(index, sort)| {
            let chosen = index == *selected;
            let mut spans = vec![
                Span::styled(if chosen { "▌ " } else { "  " }, Style::new().fg(ACCENT)),
                Span::styled(sort.label(), Style::new().fg(Color::Gray)),
            ];
            if *sort == app.sort {
                spans.push(Span::styled("  (now)", Style::new().fg(MUTED)));
            }
            let line = Line::from(spans);
            if chosen {
                line.style(Style::new().bg(Color::Indexed(236)).bold())
            } else {
                line
            }
        })
        .collect();
    frame.render_widget(Paragraph::new(lines), list);
    frame.render_widget(
        Paragraph::new(vec![
            Line::raw(""),
            key_hints(&[("↑↓", "select"), ("Enter", "sort"), ("Esc", "close")]),
        ]),
        hint,
    );
}

fn key_hints(hints: &[(&str, &str)]) -> Line<'static> {
    let mut spans = Vec::new();
    for (index, (key, label)) in hints.iter().enumerate() {
        if index > 0 {
            spans.push(Span::styled("  ·  ", Style::new().fg(MUTED)));
        }
        spans.push(Span::styled(
            key.to_string(),
            Style::new().fg(ACCENT).bold(),
        ));
        spans.push(Span::styled(
            format!(" {label}"),
            Style::new().fg(Color::Gray),
        ));
    }
    Line::from(spans)
}

fn draw_confirm_remove(frame: &mut Frame, app: &App) {
    let Mode::ConfirmRemove {
        torrent,
        delete_files,
    } = &app.mode
    else {
        return;
    };
    let row = app.rows().find(|row| row.hash == *torrent);
    let name = row.map(|row| row.name.to_owned()).unwrap_or_default();
    let folder = row
        .map(|row| row.save_dir.display().to_string())
        .unwrap_or_default();
    let (mark, about, action) = if *delete_files {
        (
            Span::styled("[x]", Style::new().fg(Color::LightRed).bold()),
            Span::styled(
                format!(
                    "Its files in {folder} are deleted for good. Other files there, and files \
                     another torrent uses, stay."
                ),
                Style::new().fg(Color::LightRed),
            ),
            "remove and delete files",
        )
    } else {
        (
            Span::styled("[ ]", Style::new().fg(MUTED)),
            Span::styled(
                format!("Its downloaded files stay in {folder}."),
                Style::new().fg(Color::Gray),
            ),
            "remove",
        )
    };
    let inner = dialog(frame, "Remove torrent", 72, 11);
    frame.render_widget(
        Paragraph::new(vec![
            Line::from(vec![
                Span::raw("Remove "),
                Span::raw(name).bold(),
                Span::raw(" from EmpireBT?"),
            ]),
            Line::raw(""),
            Line::from(vec![mark, Span::raw(" Also delete the downloaded files")]),
            Line::from(about),
            Line::raw(""),
            key_hints(&[("d", "delete files too"), ("y", action), ("n", "keep")]),
        ])
        .wrap(Wrap { trim: true }),
        inner,
    );
}

fn draw_settings(frame: &mut Frame, app: &App) {
    let Mode::Settings {
        selected,
        editing,
        error,
    } = &app.mode
    else {
        return;
    };
    let settings = app.settings();
    let setting = settings[*selected].setting;
    let field_height = if editing.is_some() { 3 } else { 0 };
    let height = settings.len() as u16 + 10 + field_height + u16::from(error.is_some());
    let inner = dialog(frame, "Settings", 92, height);
    let [list, _, about, field, error_area, hint] = Layout::vertical([
        Constraint::Length(settings.len() as u16),
        Constraint::Length(1),
        Constraint::Length(2),
        Constraint::Length(field_height),
        Constraint::Length(u16::from(error.is_some())),
        Constraint::Length(3),
    ])
    .areas(inner);

    let width = settings
        .iter()
        .map(|item| item.setting.key.len())
        .max()
        .unwrap_or(0);
    let rows: Vec<Line> = settings
        .iter()
        .enumerate()
        .map(|(index, item)| {
            let chosen = index == *selected;
            let mut spans = vec![
                Span::styled(if chosen { "▌ " } else { "  " }, Style::new().fg(ACCENT)),
                Span::styled(
                    format!("{:<width$}  ", item.setting.key),
                    if chosen {
                        Style::new().bold()
                    } else {
                        Style::new().fg(Color::Gray)
                    },
                ),
                Span::styled(
                    item.value.clone(),
                    Style::new().fg(if chosen { ACCENT } else { Color::White }),
                ),
            ];
            if item.overridden {
                spans.push(Span::styled(
                    "  (flag overrides this run)",
                    Style::new().fg(Color::Yellow),
                ));
            }
            let line = Line::from(spans);
            if chosen {
                line.style(Style::new().bg(Color::Indexed(236)))
            } else {
                line
            }
        })
        .collect();
    frame.render_widget(Paragraph::new(rows), list);

    let mut description = vec![Span::raw(setting.description)];
    if setting.needs_restart {
        description.push(Span::styled(
            "  Applies after restart.",
            Style::new().fg(Color::Yellow),
        ));
    }
    frame.render_widget(
        Paragraph::new(Line::from(description))
            .style(Style::new().fg(Color::Gray))
            .wrap(Wrap { trim: true }),
        about,
    );

    if let Some(input) = editing {
        let field_block = Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(Style::new().fg(ACCENT));
        let field_inner = field_block.inner(field);
        frame.render_widget(field_block, field);
        let room = (field_inner.width as usize).saturating_sub(3);
        let offset = input.cursor().saturating_sub(room);
        let visible: String = input.text().chars().skip(offset).take(room + 1).collect();
        frame.render_widget(
            Line::from(vec![
                Span::styled("› ", Style::new().fg(ACCENT).bold()),
                Span::raw(visible),
            ]),
            field_inner,
        );
        frame.set_cursor_position(Position::new(
            field_inner.x + (2 + input.cursor() - offset) as u16,
            field_inner.y,
        ));
    }
    if let Some(error) = error {
        frame.render_widget(
            Line::from(Span::styled(
                format!("✕ {error}"),
                Style::new().fg(Color::LightRed),
            )),
            error_area,
        );
    }
    let keys = if editing.is_some() {
        key_hints(&[("Enter", "save"), ("Esc", "cancel")])
    } else {
        key_hints(&[("↑↓", "select"), ("Enter", "change"), ("Esc", "close")])
    };
    frame.render_widget(
        Paragraph::new(vec![
            keys,
            Line::from(Span::styled(
                format!("Saved to {}", app.config_path().display()),
                Style::new().fg(MUTED),
            )),
        ]),
        hint,
    );
}

fn draw_web_info(frame: &mut Frame, app: &App) {
    let Some(web) = &app.web else {
        return;
    };
    let label = |text: &str| Line::from(Span::styled(text.to_owned(), Style::new().fg(MUTED)));
    let mut lines = vec![
        label("Open in a browser:"),
        Line::from(Span::styled(
            web.url.clone(),
            Style::new().fg(ACCENT).bold(),
        )),
        Line::raw(""),
        label("Or sign in at once with this one-time link (valid for 10 minutes):"),
        Line::raw(web.link.clone()),
    ];
    if let Some(password) = &web.password {
        lines.push(Line::raw(""));
        lines.push(label("First start: the web UI password is"));
        lines.push(Line::from(Span::styled(
            password.clone(),
            Style::new().bold(),
        )));
        lines.push(label("Change it later with `empirebt web password`."));
    }
    if let Some(fingerprint) = &web.fingerprint {
        lines.push(Line::raw(""));
        lines.push(label(
            "HTTPS certificate fingerprint (SHA-256), as the browser shows it:",
        ));
        lines.push(Line::raw(fingerprint.clone()));
    }
    lines.push(Line::raw(""));
    lines.push(Line::from(Span::styled(
        "Press any key to close (the address stays in the help, ?)",
        Style::new().fg(MUTED),
    )));
    let width = 104;
    // Long links wrap; leave room for them.
    let wrapped: u16 = lines
        .iter()
        .map(|line| (line.width() as u16).div_ceil(width - 6).max(1))
        .sum();
    let inner = dialog(frame, "Web UI", width, wrapped + 3);
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
}

fn draw_help(frame: &mut Frame, app: &App) {
    let sections: [(&str, &[(&str, &str)]); 5] = [
        (
            "Torrents",
            &[
                (
                    "a  o",
                    "add a .torrent file, URL, or magnet link (or paste one)",
                ),
                ("s  ⏎", "start / resume downloading"),
                ("p", "pause downloading or seeding"),
                ("space", "toggle start / pause"),
                ("v", "recheck local data"),
                ("x  del", "remove from the list (keeps files)"),
            ],
        ),
        (
            "Organize",
            &[
                ("t", "edit tags (comma-separated)"),
                ("C", "set category"),
                ("R", "rename the torrent (the name shown)"),
                ("r", "rename a file or folder on disk"),
                ("f", "filter by category or tag"),
                ("S", "sort: by upload speed, size, progress…"),
                ("esc", "show all torrents again"),
            ],
        ),
        (
            "Peer flags (as in qBittorrent)",
            &[
                ("D  d", "downloading from it / it is choking us"),
                ("U  u", "uploading to it / we are choking it"),
                ("K  ?", "unchoked by it / by us, nothing wanted"),
                ("O  S", "optimistic unchoke / snubbed"),
                ("I", "it connected to us"),
                ("H X L", "from the DHT / peer exchange / LAN"),
                ("E e P", "encrypted / encrypted handshake / uTP"),
            ],
        ),
        (
            "Navigate",
            &[
                ("↑ ↓  j k", "move selection"),
                ("g G", "first / last torrent"),
                ("⇥ ⇤  1 2 3 4", "switch detail tab"),
            ],
        ),
        (
            "App",
            &[
                ("c", "settings"),
                ("?", "show this help"),
                ("q  ctrl-c", "quit (progress is saved)"),
            ],
        ),
    ];
    let mut lines = Vec::new();
    for (index, (title, keys)) in sections.iter().enumerate() {
        if index > 0 {
            lines.push(Line::raw(""));
        }
        lines.push(Line::from(Span::styled(
            *title,
            Style::new().fg(ACCENT).bold(),
        )));
        for (key, description) in *keys {
            lines.push(Line::from(vec![
                Span::styled(format!("  {key:<12}"), Style::new().bold()),
                Span::styled(*description, Style::new().fg(Color::Gray)),
            ]));
        }
    }
    if let Some(web) = &app.web {
        lines.push(Line::raw(""));
        lines.push(Line::from(vec![
            Span::styled("Web UI  ", Style::new().fg(ACCENT).bold()),
            Span::raw(web.url.clone()),
        ]));
    }
    lines.push(Line::raw(""));
    lines.push(Line::from(Span::styled(
        "Press any key to close",
        Style::new().fg(MUTED),
    )));
    let height = lines.len() as u16 + 3;
    let inner = dialog(frame, "Keyboard shortcuts", 60, height);
    frame.render_widget(Paragraph::new(lines), inner);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bar_text(fraction: f64) -> String {
        progress_bar(fraction, 4, Color::Reset)
            .iter()
            .map(|span| span.content.as_ref())
            .collect()
    }

    #[test]
    fn progress_bar_keeps_a_fixed_width() {
        for step in 0..=32 {
            assert_eq!(bar_text(f64::from(step) / 32.0).chars().count(), 4);
        }
        assert_eq!(bar_text(0.0), "░░░░");
        assert_eq!(bar_text(0.5), "██░░");
        assert_eq!(bar_text(1.0), "████");
    }
}
