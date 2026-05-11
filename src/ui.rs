use chrono::{TimeZone, Utc};
use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap};

use crate::app::{
    AUTO_INTERVALS_SECS, App, ConfirmKind, ConnForm, Dialog, Focus, FormMode, SelKind, ToastKind,
};
use crate::backup;
use crate::quirks;
use crate::types::{Connection, DbKind, DetectedSource, human_bytes, human_duration};

const ACCENT: Color = Color::Rgb(110, 168, 254);
const ACCENT_DIM: Color = Color::Rgb(60, 90, 140);
const FG_DIM: Color = Color::Rgb(120, 120, 120);
const FG_MUTED: Color = Color::Rgb(180, 180, 180);
const FG_BRIGHT: Color = Color::Rgb(220, 220, 230);
const SUCCESS: Color = Color::Rgb(120, 200, 120);
const WARN: Color = Color::Rgb(245, 178, 73);
const ERROR: Color = Color::Rgb(235, 90, 90);
const BORDER_DIM: Color = Color::Rgb(60, 60, 60);
const HL_BG: Color = Color::Rgb(28, 36, 52);

pub fn draw(f: &mut Frame, app: &App) {
    let root = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(2),
            Constraint::Min(1),
            Constraint::Length(2),
        ])
        .split(f.area());

    draw_header(f, root[0], app);
    draw_body(f, root[1], app);
    draw_footer(f, root[2], app);

    if let Some(dialog) = &app.dialog {
        match dialog {
            Dialog::Form(form) => draw_form(f, form),
            Dialog::Auto(form) => draw_auto(f, form, app),
            Dialog::Confirm(kind) => draw_confirm(f, kind),
            Dialog::Progress { label } => draw_progress(f, label),
            Dialog::Help => draw_help(f),
        }
    }
}

fn draw_header(f: &mut Frame, area: Rect, app: &App) {
    let title = Line::from(vec![
        Span::styled(
            "  siphon ",
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        ),
        Span::styled("· dump any db, anytime", Style::default().fg(FG_DIM)),
    ]);

    let count_saved = app.conn_cache.len();
    let count_detected = app.detected.len();
    let scan_lbl = if app.scanning_detect {
        " scanning containers… ".to_string()
    } else {
        format!(" {} saved · {} detected ", count_saved, count_detected)
    };
    let row = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Min(1), Constraint::Length(40)])
        .split(area);
    f.render_widget(Paragraph::new(title), row[0]);
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(scan_lbl, Style::default().fg(FG_DIM))))
            .alignment(Alignment::Right),
        row[1],
    );

    let underline = Paragraph::new(Span::styled(
        "─".repeat(area.width as usize),
        Style::default().fg(BORDER_DIM),
    ));
    f.render_widget(
        underline,
        Rect {
            y: area.y + area.height - 1,
            height: 1,
            ..area
        },
    );
}

fn draw_body(f: &mut Frame, area: Rect, app: &App) {
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(38), Constraint::Percentage(62)])
        .split(area);
    draw_list(f, cols[0], app);
    draw_details(f, cols[1], app);
}

fn pane_block(title: impl Into<String>, count: Option<usize>, focused: bool) -> Block<'static> {
    let title = title.into();
    let title_line = match count {
        Some(n) => Line::from(vec![
            Span::raw(" "),
            Span::styled(
                title,
                Style::default()
                    .fg(if focused { ACCENT } else { FG_MUTED })
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(format!("  {n} "), Style::default().fg(FG_DIM)),
        ]),
        None => Line::from(vec![
            Span::raw(" "),
            Span::styled(
                title,
                Style::default()
                    .fg(if focused { ACCENT } else { FG_MUTED })
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(" "),
        ]),
    };
    Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(if focused { ACCENT } else { BORDER_DIM }))
        .title(title_line)
}

fn draw_list(f: &mut Frame, area: Rect, app: &App) {
    let focused = app.focus == Focus::List;
    let total = app.conn_cache.len() + app.detected.len();
    let block = pane_block("Databases", Some(total), focused);

    if total == 0 {
        let msg = if app.scanning_detect {
            "scanning for local DB containers…"
        } else {
            "no saved connections.\n\npress  n  to add one, or wait for autodetect."
        };
        f.render_widget(
            Paragraph::new(msg)
                .style(Style::default().fg(FG_DIM))
                .block(block)
                .wrap(Wrap { trim: true }),
            area,
        );
        return;
    }

    let mut items: Vec<ListItem> = Vec::new();
    let mut state_idx: usize = 0;

    // Saved
    items.push(section_header("Saved"));
    let saved_section_offset = items.len();
    for (i, c) in app.conn_cache.iter().enumerate() {
        let selected = app.sel.kind == SelKind::Saved && app.sel.index == i;
        items.push(connection_row(c, selected, focused));
        if selected {
            state_idx = items.len() - 1;
        }
    }
    if app.conn_cache.is_empty() {
        items.push(ListItem::new(Line::from(vec![
            Span::raw("  "),
            Span::styled("(empty)", Style::default().fg(FG_DIM)),
        ])));
    }
    let _ = saved_section_offset;

    // Detected
    items.push(ListItem::new(Line::raw("")));
    items.push(section_header("Detected"));
    if app.detected.is_empty() {
        items.push(ListItem::new(Line::from(vec![
            Span::raw("  "),
            Span::styled(
                if app.scanning_detect {
                    "(scanning…)"
                } else {
                    "(none — run `docker ps` to verify)"
                },
                Style::default().fg(FG_DIM),
            ),
        ])));
    }
    for (i, d) in app.detected.iter().enumerate() {
        let selected = app.sel.kind == SelKind::Detected && app.sel.index == i;
        items.push(detected_row(d, &app.conn_cache, selected, focused));
        if selected {
            state_idx = items.len() - 1;
        }
    }

    let mut state = ListState::default();
    state.select(Some(state_idx));
    let list = List::new(items)
        .block(block)
        .highlight_style(Style::default().bg(HL_BG));
    f.render_stateful_widget(list, area, &mut state);
}

fn section_header(title: &str) -> ListItem<'_> {
    ListItem::new(Line::from(vec![
        Span::styled(
            format!("  {} ", title),
            Style::default().fg(FG_DIM).add_modifier(Modifier::BOLD),
        ),
    ]))
}

fn connection_row<'a>(c: &'a Connection, selected: bool, focused: bool) -> ListItem<'a> {
    let chevron = if selected { "▸ " } else { "  " };
    let cstyle = chevron_style(selected, focused);
    let kind_lbl = format!("[{}]", c.kind.label());
    let location = match c.container_name.as_deref() {
        Some(n) if !n.is_empty() => format!("docker · {n}"),
        _ => format!("{}:{}", c.host, c.port),
    };
    let auto = match &c.auto_backup {
        Some(ab) if ab.enabled => {
            let lbl = AUTO_INTERVALS_SECS
                .iter()
                .find(|(s, _)| *s == ab.interval_secs)
                .map(|(_, l)| *l)
                .unwrap_or("custom");
            format!(" · auto {}", lbl)
        }
        _ => String::new(),
    };
    let title = Line::from(vec![
        Span::styled(chevron, cstyle),
        Span::styled(
            c.name.clone(),
            Style::default()
                .fg(if selected { FG_BRIGHT } else { FG_MUTED })
                .add_modifier(if selected {
                    Modifier::BOLD
                } else {
                    Modifier::empty()
                }),
        ),
        Span::raw("  "),
        Span::styled(kind_lbl, Style::default().fg(ACCENT)),
    ]);
    let sub = Line::from(vec![
        Span::raw("  "),
        Span::styled(location, Style::default().fg(FG_DIM)),
        Span::styled(auto, Style::default().fg(SUCCESS)),
    ]);
    ListItem::new(vec![title, sub])
}

fn detected_row<'a>(
    d: &'a DetectedSource,
    saved: &'a [Connection],
    selected: bool,
    focused: bool,
) -> ListItem<'a> {
    let chevron = if selected { "▸ " } else { "  " };
    let cstyle = chevron_style(selected, focused);
    let already = saved
        .iter()
        .any(|c| c.container_id.as_deref() == Some(d.container_id.as_str()));
    let badge_text = if already { "saved" } else { "press i" };
    let badge_color = if already { SUCCESS } else { WARN };
    let title = Line::from(vec![
        Span::styled(chevron, cstyle),
        Span::styled(
            d.display_name(),
            Style::default()
                .fg(if selected { FG_BRIGHT } else { FG_MUTED })
                .add_modifier(if selected {
                    Modifier::BOLD
                } else {
                    Modifier::empty()
                }),
        ),
        Span::raw("  "),
        Span::styled(format!("[{}]", d.kind.label()), Style::default().fg(ACCENT)),
        Span::raw("  "),
        Span::styled(format!("· {badge_text}"), Style::default().fg(badge_color)),
    ]);
    let sub = Line::from(vec![
        Span::raw("  "),
        Span::styled(
            format!("{} · port {}", d.image, d.host_port),
            Style::default().fg(FG_DIM),
        ),
    ]);
    ListItem::new(vec![title, sub])
}

fn chevron_style(selected: bool, focused: bool) -> Style {
    if selected && focused {
        Style::default().fg(ACCENT)
    } else if selected {
        Style::default().fg(ACCENT_DIM)
    } else {
        Style::default().fg(BORDER_DIM)
    }
}

fn draw_details(f: &mut Frame, area: Rect, app: &App) {
    let focused = app.focus == Focus::Details;
    if let Some(c) = app.current_saved() {
        draw_saved_details(f, area, c, app, focused);
    } else if let Some(d) = app.current_detected() {
        draw_detected_details(f, area, d, focused);
    } else {
        let block = pane_block("Details", None, focused);
        let body = Paragraph::new("(select a database on the left)")
            .style(Style::default().fg(FG_DIM))
            .block(block);
        f.render_widget(body, area);
    }
}

fn draw_saved_details(
    f: &mut Frame,
    area: Rect,
    c: &Connection,
    app: &App,
    focused: bool,
) {
    let block = pane_block(c.name.clone(), None, focused);
    let inner = block.inner(area);
    f.render_widget(block, area);

    let split = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(11), Constraint::Min(1)])
        .split(inner);

    let mut lines: Vec<Line<'_>> = Vec::new();
    lines.push(kv("kind", c.kind.label()));
    if let Some(uri) = c.uri.as_deref().filter(|s| !s.is_empty()) {
        lines.push(kv("uri", uri));
    } else if matches!(c.kind, DbKind::Sqlite) {
        lines.push(kv(
            "file",
            c.database.as_deref().unwrap_or("(not set)"),
        ));
    } else {
        let host_line = format!("{}:{}", c.host, c.port);
        lines.push(kv("host", &host_line));
        if let Some(u) = &c.user {
            lines.push(kv("user", u));
        }
        if let Some(d) = c.database.as_deref().filter(|s| !s.is_empty()) {
            lines.push(kv("database", d));
        }
    }
    if let Some(name) = c.container_name.as_deref().filter(|s| !s.is_empty()) {
        lines.push(kv("container", name));
    }
    let auto_line = match &c.auto_backup {
        Some(ab) if ab.enabled => {
            let lbl = AUTO_INTERVALS_SECS
                .iter()
                .find(|(s, _)| *s == ab.interval_secs)
                .map(|(_, l)| *l)
                .unwrap_or("custom");
            format!("every {} · keep {}", lbl, ab.retention)
        }
        Some(_) => "configured (disabled)".to_string(),
        None => "off".to_string(),
    };
    lines.push(kv("auto", &auto_line));
    let last = match c.last_backup_at {
        Some(t) if t > 0 => {
            let now = Utc::now().timestamp();
            let age = (now - t).max(0) as u64;
            format!(
                "{} ago  ({})",
                human_duration(age),
                Utc.timestamp_opt(t, 0)
                    .single()
                    .map(|dt| dt.format("%Y-%m-%d %H:%M").to_string())
                    .unwrap_or_default()
            )
        }
        _ => "never".to_string(),
    };
    lines.push(kv("last", &last));

    // Quirks hint (Supabase/RDS/etc.)
    if let Some(note) = connection_quirks_note(c) {
        lines.push(Line::from(vec![
            Span::raw("  "),
            Span::styled(
                format!("{:<9}", "provider"),
                Style::default().fg(FG_DIM),
            ),
            Span::styled(note, Style::default().fg(SUCCESS)),
        ]));
    }
    // Runtime hint — how the dump will actually execute.
    let runtime_hint = runtime_hint_for(c);
    lines.push(Line::from(vec![
        Span::raw("  "),
        Span::styled(format!("{:<9}", "runtime"), Style::default().fg(FG_DIM)),
        Span::styled(runtime_hint.0, Style::default().fg(runtime_hint.1)),
    ]));

    if app.running.as_ref().map(|r| r.conn_id.as_str()) == Some(c.id.as_str()) {
        lines.push(Line::from(vec![
            Span::styled(
                "  • dumping…  ",
                Style::default().fg(WARN).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!(
                    "{}s",
                    app.running.as_ref().unwrap().started.elapsed().as_secs()
                ),
                Style::default().fg(FG_DIM),
            ),
        ]));
    }

    f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), split[0]);

    draw_backup_list(f, split[1], c, app);
}

fn draw_detected_details(f: &mut Frame, area: Rect, d: &DetectedSource, focused: bool) {
    let block = pane_block(d.display_name(), None, focused);
    let inner = block.inner(area);
    f.render_widget(block, area);

    let host_port = d.host_port.to_string();
    let lines = vec![
        kv("kind", d.kind.label()),
        kv("image", &d.image),
        kv("container", &d.container_name),
        kv("port", &host_port),
        kv("user", d.user.as_deref().unwrap_or("(none)")),
        kv(
            "password",
            if d.password.is_some() { "•••• (from env)" } else { "(none)" },
        ),
        kv("database", d.database.as_deref().unwrap_or("(default)")),
        Line::raw(""),
        Line::from(Span::styled(
            "  press  i  to import as a saved connection.",
            Style::default().fg(WARN),
        )),
        Line::from(Span::styled(
            "  press  d  to dump it right now.",
            Style::default().fg(FG_DIM),
        )),
    ];
    f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
}

fn connection_quirks_note(c: &Connection) -> Option<String> {
    let host = if let Some(uri) = c.uri.as_deref().filter(|s| !s.is_empty()) {
        url::Url::parse(uri).ok()?.host_str()?.to_string()
    } else {
        c.host.clone()
    };
    quirks::for_host(&host, c.kind).note()
}

fn runtime_hint_for(c: &Connection) -> (String, Color) {
    if matches!(c.kind, DbKind::Sqlite) {
        return if which::which("sqlite3").is_ok() {
            ("local sqlite3".to_string(), FG_BRIGHT)
        } else {
            ("missing — install sqlite".to_string(), ERROR)
        };
    }
    if let Some(name) = c.container_name.as_deref().filter(|s| !s.is_empty()) {
        return (format!("docker exec {name}"), FG_BRIGHT);
    }
    let tool = match c.kind {
        DbKind::Postgres => "pg_dump",
        DbKind::Mongo => "mongodump",
        DbKind::Mysql => "mysqldump",
        DbKind::Redis => "redis-cli",
        DbKind::Sqlite => "sqlite3",
    };
    if which::which(tool).is_ok() {
        (format!("local {tool}"), FG_BRIGHT)
    } else if which::which("docker").is_ok() {
        let image = match c.kind {
            DbKind::Postgres => "postgres:17",
            DbKind::Mongo => "mongo:6",
            DbKind::Mysql => "mysql:8",
            DbKind::Redis => "redis:7",
            DbKind::Sqlite => "alpine",
        };
        (format!("docker run {image}"), SUCCESS)
    } else {
        (format!("missing {tool} & docker"), ERROR)
    }
}

fn kv(k: &str, v: &str) -> Line<'static> {
    Line::from(vec![
        Span::raw("  "),
        Span::styled(format!("{:<9}", k), Style::default().fg(FG_DIM)),
        Span::styled(v.to_string(), Style::default().fg(FG_BRIGHT)),
    ])
}

fn draw_backup_list(f: &mut Frame, area: Rect, c: &Connection, app: &App) {
    let files = backup::list(&app.backup_root, c);
    let header = Line::from(vec![
        Span::raw("  "),
        Span::styled(
            "Recent backups",
            Style::default()
                .fg(FG_DIM)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!("  {}", files.len()),
            Style::default().fg(FG_DIM),
        ),
    ]);

    if files.is_empty() {
        let lines = vec![
            header,
            Line::raw(""),
            Line::from(vec![
                Span::raw("  "),
                Span::styled("(none yet — press  d  to dump now)", Style::default().fg(FG_DIM)),
            ]),
        ];
        f.render_widget(Paragraph::new(lines), area);
        return;
    }

    let mut lines: Vec<Line<'_>> = Vec::with_capacity(files.len() + 2);
    lines.push(header);
    lines.push(Line::raw(""));
    for (i, file) in files.iter().take(8).enumerate() {
        let when = Utc
            .timestamp_opt(file.created_at, 0)
            .single()
            .map(|dt| dt.format("%Y-%m-%d %H:%M").to_string())
            .unwrap_or_else(|| "?".into());
        let name = file
            .path
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("?");
        let bullet = if i == 0 { "▸" } else { "·" };
        lines.push(Line::from(vec![
            Span::styled(format!("  {bullet} "), Style::default().fg(if i == 0 { ACCENT } else { FG_DIM })),
            Span::styled(name.to_string(), Style::default().fg(FG_MUTED)),
            Span::styled(
                format!("  {}  ", human_bytes(file.bytes)),
                Style::default().fg(FG_DIM),
            ),
            Span::styled(when, Style::default().fg(FG_DIM)),
        ]));
    }
    f.render_widget(Paragraph::new(lines), area);
}

fn draw_footer(f: &mut Frame, area: Rect, app: &App) {
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Length(1)])
        .split(area);

    let mut spans: Vec<Span<'_>> = Vec::new();
    let push = |spans: &mut Vec<Span<'_>>, k: &'static str, label: &'static str| {
        spans.push(Span::styled(
            k,
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        ));
        spans.push(Span::styled(format!(" {label}   "), Style::default().fg(FG_DIM)));
    };
    push(&mut spans, "n", "new");
    push(&mut spans, "d", "dump");
    push(&mut spans, "a", "auto");
    push(&mut spans, "t", "test");
    push(&mut spans, "i", "import");
    push(&mut spans, "e", "edit");
    push(&mut spans, "D", "delete");
    push(&mut spans, "r", "rescan");
    push(&mut spans, "o", "open dir");
    push(&mut spans, "?", "help");
    push(&mut spans, "q", "quit");
    f.render_widget(Paragraph::new(Line::from(spans)), rows[0]);

    if let Some(t) = &app.toast {
        let color = match t.kind {
            ToastKind::Info => FG_MUTED,
            ToastKind::Success => SUCCESS,
            ToastKind::Error => ERROR,
        };
        let prefix = match t.kind {
            ToastKind::Info => "·",
            ToastKind::Success => "✓",
            ToastKind::Error => "✗",
        };
        f.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(
                    format!(" {prefix} "),
                    Style::default().fg(color).add_modifier(Modifier::BOLD),
                ),
                Span::styled(t.message.clone(), Style::default().fg(color)),
            ])),
            rows[1],
        );
    }
}

// ────────────────────────────────────────────────────────────────────────────
// Dialogs
// ────────────────────────────────────────────────────────────────────────────

fn centered_rect(width: u16, height: u16, area: Rect) -> Rect {
    let w = width.min(area.width.saturating_sub(2));
    let h = height.min(area.height.saturating_sub(2));
    Rect {
        x: area.x + (area.width.saturating_sub(w)) / 2,
        y: area.y + (area.height.saturating_sub(h)) / 2,
        width: w,
        height: h,
    }
}

fn draw_form(f: &mut Frame, form: &ConnForm) {
    let area = centered_rect(64, 20, f.area());
    f.render_widget(Clear, area);
    let title = if form.editing_id.is_some() {
        "Edit connection"
    } else {
        "New connection"
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(ACCENT))
        .title(Line::from(Span::styled(
            format!(" {} ", title),
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        )));
    let inner = block.inner(area);
    f.render_widget(block, area);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(1),
            Constraint::Length(2),
        ])
        .split(inner);

    let mode_lbl = match form.mode {
        FormMode::Fields => "[fields]    uri  (ctrl-u to toggle)",
        FormMode::Uri => " fields    [uri]  (ctrl-u to toggle)",
    };
    f.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("  mode  ", Style::default().fg(FG_DIM)),
            Span::styled(
                mode_lbl,
                Style::default().fg(FG_MUTED).add_modifier(Modifier::BOLD),
            ),
        ])),
        chunks[0],
    );

    let mut lines: Vec<Line<'_>> = Vec::new();
    for i in 0..form.field_count() {
        let label = form.field_label(i);
        let focused = i == form.field;
        let prefix = if focused { "▸ " } else { "  " };
        let prefix_color = if focused { ACCENT } else { BORDER_DIM };
        let label_color = if focused { ACCENT } else { FG_DIM };
        let value: String = match label {
            "kind" => form.kind.label().to_string(),
            "password" => {
                if form.show_password || form.password.is_empty() {
                    form.password.clone()
                } else {
                    "•".repeat(form.password.chars().count().min(24))
                }
            }
            _ => form.field_text(i).to_string(),
        };
        let cursor = if focused && label != "kind" { "│" } else { "" };
        let value_color = if focused { FG_BRIGHT } else { FG_MUTED };
        lines.push(Line::from(vec![
            Span::styled(prefix, Style::default().fg(prefix_color)),
            Span::styled(format!("{:<9}", label), Style::default().fg(label_color)),
            Span::styled(value, Style::default().fg(value_color)),
            Span::styled(cursor, Style::default().fg(ACCENT)),
        ]));
        if label == "kind" && focused {
            lines.push(Line::from(vec![
                Span::raw("           "),
                Span::styled(
                    "(← → to cycle)",
                    Style::default().fg(FG_DIM).add_modifier(Modifier::ITALIC),
                ),
            ]));
        }
    }
    if let Some(err) = &form.error {
        lines.push(Line::raw(""));
        lines.push(Line::from(Span::styled(
            format!("  ! {err}"),
            Style::default().fg(ERROR),
        )));
    }

    f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), chunks[1]);

    let hint = Line::from(vec![
        key("tab"), sep(" next field   "),
        key("ctrl-u"), sep(" toggle mode   "),
        key("ctrl-p"), sep(" show pw   "),
        key("enter"), sep(" save   "),
        key("esc"), sep(" cancel"),
    ]);
    f.render_widget(Paragraph::new(hint), chunks[2]);
}

fn draw_auto(f: &mut Frame, form: &crate::app::AutoForm, app: &App) {
    let name = app
        .conn_cache
        .iter()
        .find(|c| c.id == form.conn_id)
        .map(|c| c.name.clone())
        .unwrap_or_default();

    let area = centered_rect(50, 12, f.area());
    f.render_widget(Clear, area);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(ACCENT))
        .title(Line::from(Span::styled(
            format!(" auto-backup · {} ", name),
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        )));
    let inner = block.inner(area);
    f.render_widget(block, area);

    let mut lines: Vec<Line<'_>> = Vec::new();
    let mk = |idx: usize, focused_idx: usize, lbl: &str, val: String| -> Line<'_> {
        let focused = idx == focused_idx;
        Line::from(vec![
            Span::styled(
                if focused { "▸ " } else { "  " },
                Style::default().fg(if focused { ACCENT } else { BORDER_DIM }),
            ),
            Span::styled(
                format!("{:<10}", lbl),
                Style::default().fg(if focused { ACCENT } else { FG_DIM }),
            ),
            Span::styled(
                val,
                Style::default()
                    .fg(if focused { FG_BRIGHT } else { FG_MUTED })
                    .add_modifier(if focused {
                        Modifier::BOLD
                    } else {
                        Modifier::empty()
                    }),
            ),
        ])
    };

    lines.push(mk(0, form.field, "enabled", if form.enabled { "on  (space to toggle)".into() } else { "off (space to toggle)".into() }));
    lines.push(mk(1, form.field, "interval", format!("{}  (← →)", form.interval_label())));
    lines.push(mk(2, form.field, "retention", format!("{}  files", form.retention)));
    lines.push(Line::raw(""));
    lines.push(Line::from(Span::styled(
        "  oldest backups are pruned automatically.",
        Style::default().fg(FG_DIM),
    )));
    if let Some(err) = &form.error {
        lines.push(Line::raw(""));
        lines.push(Line::from(Span::styled(
            format!("  ! {err}"),
            Style::default().fg(ERROR),
        )));
    }
    lines.push(Line::raw(""));
    lines.push(Line::from(vec![
        key("tab"), sep(" next   "),
        key("enter"), sep(" save   "),
        key("esc"), sep(" cancel"),
    ]));

    f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
}

fn draw_confirm(f: &mut Frame, kind: &ConfirmKind) {
    let (title, body, accent) = match kind {
        ConfirmKind::Delete { name, .. } => (
            " delete ",
            vec![
                Line::from(Span::styled(
                    format!("really delete '{}'?", name),
                    Style::default().fg(FG_BRIGHT),
                )),
                Line::from(Span::styled(
                    "(backups on disk are kept)",
                    Style::default().fg(FG_DIM),
                )),
            ],
            ERROR,
        ),
        ConfirmKind::Dump { name, .. } => (
            " dump now ",
            vec![Line::from(Span::styled(
                format!("dump '{}' now?", name),
                Style::default().fg(FG_BRIGHT),
            ))],
            ACCENT,
        ),
    };

    let area = centered_rect(50, 7, f.area());
    f.render_widget(Clear, area);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(accent))
        .title(Line::from(Span::styled(
            title,
            Style::default().fg(accent).add_modifier(Modifier::BOLD),
        )));
    let inner = block.inner(area);
    f.render_widget(block, area);

    let mut lines = body;
    lines.push(Line::raw(""));
    lines.push(Line::from(vec![
        Span::styled(" y ", Style::default().bg(accent).fg(Color::Black).add_modifier(Modifier::BOLD)),
        Span::styled(" confirm     ", Style::default().fg(FG_MUTED)),
        Span::styled(" n ", Style::default().bg(BORDER_DIM).fg(FG_BRIGHT)),
        Span::styled(" cancel", Style::default().fg(FG_MUTED)),
    ]));
    f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
}

fn draw_progress(f: &mut Frame, label: &str) {
    let area = centered_rect(50, 5, f.area());
    f.render_widget(Clear, area);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(WARN))
        .title(Line::from(Span::styled(
            " working ",
            Style::default().fg(WARN).add_modifier(Modifier::BOLD),
        )));
    let inner = block.inner(area);
    f.render_widget(block, area);
    let body = Paragraph::new(vec![
        Line::raw(""),
        Line::from(Span::styled(
            format!("  {}", label),
            Style::default().fg(FG_BRIGHT),
        )),
    ]);
    f.render_widget(body, inner);
}

fn draw_help(f: &mut Frame) {
    let area = centered_rect(64, 22, f.area());
    f.render_widget(Clear, area);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(ACCENT))
        .title(Line::from(Span::styled(
            " help ",
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        )));
    let inner = block.inner(area);
    f.render_widget(block, area);

    let row = |k: &'static str, desc: &'static str| -> Line<'static> {
        Line::from(vec![
            Span::raw("  "),
            Span::styled(format!("{:<10}", k), Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)),
            Span::styled(desc, Style::default().fg(FG_MUTED)),
        ])
    };
    let lines = vec![
        Line::from(Span::styled(
            "  navigation",
            Style::default().fg(FG_DIM).add_modifier(Modifier::BOLD),
        )),
        row("↑↓ / jk", "move between databases"),
        row("tab", "switch focus (list ↔ details)"),
        Line::raw(""),
        Line::from(Span::styled(
            "  actions",
            Style::default().fg(FG_DIM).add_modifier(Modifier::BOLD),
        )),
        row("n", "new connection (form)"),
        row("d / enter", "dump selected db now"),
        row("a", "configure auto-backup"),
        row("t", "test connection"),
        row("i", "import detected docker db"),
        row("e", "edit selected connection"),
        row("D", "delete selected (shift+d)"),
        row("r", "rescan docker containers"),
        row("o", "reveal backup folder"),
        Line::raw(""),
        Line::from(Span::styled(
            "  ? to close, q to quit",
            Style::default().fg(FG_DIM),
        )),
    ];
    f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
}

fn key(label: &'static str) -> Span<'static> {
    Span::styled(label, Style::default().fg(ACCENT).add_modifier(Modifier::BOLD))
}
fn sep(label: &'static str) -> Span<'static> {
    Span::styled(label, Style::default().fg(FG_DIM))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::App;
    use crate::config::Config;
    use crate::types::{Connection, DbKind, DetectedSource};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use std::sync::Arc;
    use tokio::sync::Mutex;

    fn rendered(width: u16, height: u16, app: &App) -> String {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| draw(f, app)).unwrap();
        let buf = terminal.backend().buffer().clone();
        let mut out = String::new();
        for y in 0..buf.area.height {
            for x in 0..buf.area.width {
                out.push_str(buf[(x, y)].symbol());
            }
            out.push('\n');
        }
        out
    }

    fn loaded_app() -> App {
        let mut app = App::new(Arc::new(Mutex::new(Config::default())), std::env::temp_dir());
        app.conn_cache = vec![Connection {
            id: "abcdefghij".into(),
            name: "Local Postgres".into(),
            kind: DbKind::Postgres,
            host: "127.0.0.1".into(),
            port: 5432,
            user: Some("takshak".into()),
            database: Some("app".into()),
            ..Default::default()
        }];
        app.detected = vec![DetectedSource {
            kind: DbKind::Mongo,
            container_id: "deadbeef".into(),
            container_name: "app-mongo".into(),
            image: "mongo:7".into(),
            host_port: 27017,
            user: Some("root".into()),
            password: Some("x".into()),
            database: None,
        }];
        app.scanning_detect = false;
        app
    }

    #[test]
    fn empty_app_renders_without_panic() {
        let app = App::new(Arc::new(Mutex::new(Config::default())), std::env::temp_dir());
        let out = rendered(120, 30, &app);
        assert!(out.contains("siphon"));
        assert!(out.contains("scanning"));
    }

    #[test]
    fn loaded_app_renders_connection_and_detected() {
        let app = loaded_app();
        let out = rendered(140, 36, &app);
        assert!(out.contains("Local Postgres"));
        assert!(out.contains("app-mongo"));
        assert!(out.contains("takshak"));
        assert!(out.contains("press i"));
    }

    #[test]
    fn form_dialog_renders() {
        let mut app = loaded_app();
        app.dialog = Some(Dialog::Form(ConnForm::new_blank()));
        let out = rendered(140, 36, &app);
        assert!(out.contains("New connection"));
        assert!(out.contains("name"));
        assert!(out.contains("host"));
    }

    #[test]
    fn help_dialog_renders() {
        let mut app = loaded_app();
        app.dialog = Some(Dialog::Help);
        let out = rendered(140, 36, &app);
        assert!(out.contains("help"));
        assert!(out.contains("import"));
    }
}
