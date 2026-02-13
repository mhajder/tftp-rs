use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::time::{Instant, SystemTime};

use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Gauge, List, ListItem, Paragraph};

use crate::server::{TransferInfo, TransferKind};

/// How often to refresh the interface IP list.
const IP_REFRESH_INTERVAL: std::time::Duration = std::time::Duration::from_secs(10);

/// Which panel currently has keyboard focus for scrolling.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FocusedPanel {
    Files,
    Transfers,
    Logs,
}

/// Top-level application state shared between the event loop and the renderer.
pub struct App {
    pub port: u16,
    pub http_port: Option<u16>,
    pub dir: PathBuf,
    pub online: bool,
    pub logs: Vec<String>,
    pub transfers: Vec<TransferInfo>,
    pub log_scroll: u16,
    pub files_scroll: u16,
    pub transfers_scroll: u16,
    pub focused_panel: FocusedPanel,
    pub show_quit_dialog: bool,
    /// true = "Yes" selected, false = "No" selected.
    pub quit_selection: bool,
    pub interface_ips: Vec<String>,
    last_ip_refresh: Instant,
    log_writer: Option<BufWriter<File>>,
}

impl App {
    pub fn new(
        port: u16,
        http_port: Option<u16>,
        dir: PathBuf,
        log_writer: Option<BufWriter<File>>,
    ) -> Self {
        let interface_ips = get_interface_ips();
        Self {
            port,
            http_port,
            dir,
            online: false,
            logs: Vec::new(),
            transfers: Vec::new(),
            log_scroll: 0,
            files_scroll: 0,
            transfers_scroll: 0,
            focused_panel: FocusedPanel::Logs,
            show_quit_dialog: false,
            quit_selection: false,
            interface_ips,
            last_ip_refresh: Instant::now(),
            log_writer,
        }
    }

    pub fn refresh_interfaces_if_needed(&mut self) {
        if self.last_ip_refresh.elapsed() >= IP_REFRESH_INTERVAL {
            self.interface_ips = get_interface_ips();
            self.last_ip_refresh = Instant::now();
        }
    }

    pub fn push_log(&mut self, msg: String) {
        let ts = timestamp_now();
        let line = format!("{ts} {msg}");

        if let Some(ref mut w) = self.log_writer {
            let _ = writeln!(w, "{line}");
            let _ = w.flush();
        }

        self.logs.push(line);
        // Auto-scroll to bottom.
        let visible = 10u16; // approximate
        let total = self.logs.len() as u16;
        self.log_scroll = total.saturating_sub(visible);
    }

    pub fn scroll_up(&mut self) {
        match self.focused_panel {
            FocusedPanel::Files => {
                self.files_scroll = self.files_scroll.saturating_sub(1);
            }
            FocusedPanel::Transfers => {
                self.transfers_scroll = self.transfers_scroll.saturating_sub(1);
            }
            FocusedPanel::Logs => {
                self.log_scroll = self.log_scroll.saturating_sub(1);
            }
        }
    }

    pub fn scroll_down(&mut self) {
        match self.focused_panel {
            FocusedPanel::Files => {
                self.files_scroll = self.files_scroll.saturating_add(1);
            }
            FocusedPanel::Transfers => {
                self.transfers_scroll = self.transfers_scroll.saturating_add(1);
            }
            FocusedPanel::Logs => {
                let max = (self.logs.len() as u16).saturating_sub(1);
                if self.log_scroll < max {
                    self.log_scroll += 1;
                }
            }
        }
    }

    pub fn cycle_focus(&mut self) {
        self.focused_panel = match self.focused_panel {
            FocusedPanel::Files => FocusedPanel::Transfers,
            FocusedPanel::Transfers => FocusedPanel::Logs,
            FocusedPanel::Logs => FocusedPanel::Files,
        };
    }
}

// ---------------------------------------------------------------------------
// Timestamp helper
// ---------------------------------------------------------------------------

fn timestamp_now() -> String {
    let dur = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default();
    let secs = dur.as_secs();
    let h = (secs / 3600) % 24;
    let m = (secs % 3600) / 60;
    let s = secs % 60;
    format!("[{h:02}:{m:02}:{s:02}]")
}

// ---------------------------------------------------------------------------
// Interface IP helper
// ---------------------------------------------------------------------------

fn get_interface_ips() -> Vec<String> {
    let mut ips: Vec<String> = if_addrs::get_if_addrs()
        .unwrap_or_default()
        .into_iter()
        .filter(|iface| !iface.is_loopback())
        .filter(|iface| iface.addr.ip().is_ipv4())
        .map(|iface| iface.addr.ip().to_string())
        .collect();
    ips.sort();
    ips.dedup();
    ips
}

// ---------------------------------------------------------------------------
// Tree structures for shared files
// ---------------------------------------------------------------------------

struct TreeEntry {
    name: String,
    depth: usize,
    is_dir: bool,
    is_last: bool,
    size: Option<u64>,
    /// Whether each ancestor at depth 0..depth is the last child of its parent.
    ancestors_are_last: Vec<bool>,
}

fn build_tree(dir: &Path, depth: usize, ancestors_are_last: &[bool]) -> Vec<TreeEntry> {
    let mut entries = Vec::new();

    let mut children: Vec<_> = match std::fs::read_dir(dir) {
        Ok(rd) => rd.filter_map(|e| e.ok()).collect(),
        Err(_) => return entries,
    };

    // Sort: directories first, then alphabetical.
    children.sort_by(|a, b| {
        let a_dir = a.file_type().map(|ft| ft.is_dir()).unwrap_or(false);
        let b_dir = b.file_type().map(|ft| ft.is_dir()).unwrap_or(false);
        b_dir.cmp(&a_dir).then_with(|| {
            a.file_name()
                .to_string_lossy()
                .to_lowercase()
                .cmp(&b.file_name().to_string_lossy().to_lowercase())
        })
    });

    let count = children.len();
    for (i, entry) in children.into_iter().enumerate() {
        let is_last = i + 1 == count;
        let is_dir = entry.file_type().map(|ft| ft.is_dir()).unwrap_or(false);
        let name = entry.file_name().to_string_lossy().to_string();
        let size = if is_dir {
            None
        } else {
            entry.metadata().ok().map(|m| m.len())
        };

        entries.push(TreeEntry {
            name: name.clone(),
            depth,
            is_dir,
            is_last,
            size,
            ancestors_are_last: ancestors_are_last.to_vec(),
        });

        if is_dir {
            let mut child_ancestors = ancestors_are_last.to_vec();
            child_ancestors.push(is_last);
            let sub = build_tree(&dir.join(&name), depth + 1, &child_ancestors);
            entries.extend(sub);
        }
    }

    entries
}

fn format_tree_entry(entry: &TreeEntry) -> Line<'static> {
    let mut prefix = String::new();

    // Build indentation from ancestor information.
    for &ancestor_is_last in &entry.ancestors_are_last {
        if ancestor_is_last {
            prefix.push_str("    ");
        } else {
            prefix.push_str(" \u{2502}  "); // │
        }
    }

    // Add the branch connector.
    if entry.depth > 0 || !entry.ancestors_are_last.is_empty() {
        if entry.is_last {
            prefix.push_str(" \u{2514}\u{2500}\u{2500} "); // └──
        } else {
            prefix.push_str(" \u{251c}\u{2500}\u{2500} "); // ├──
        }
    }

    if entry.is_dir {
        Line::from(vec![
            Span::styled(format!(" {prefix}"), Style::default().fg(Color::DarkGray)),
            Span::styled(
                format!("{}/", entry.name),
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
        ])
    } else {
        let size_str = entry
            .size
            .map(|s| format!("  ({})", human_bytes(s)))
            .unwrap_or_default();
        Line::from(vec![
            Span::styled(format!(" {prefix}"), Style::default().fg(Color::DarkGray)),
            Span::raw(entry.name.clone()),
            Span::styled(size_str, Style::default().fg(Color::DarkGray)),
        ])
    }
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

pub fn draw(f: &mut Frame, app: &mut App) {
    // Overall vertical layout: Header | Middle | Logs
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),  // header
            Constraint::Min(10),    // middle (shared files + active transfers)
            Constraint::Length(12), // logs
        ])
        .split(f.area());

    draw_header(f, app, chunks[0]);
    draw_middle(f, app, chunks[1]);
    draw_logs(f, app, chunks[2]);

    if app.show_quit_dialog {
        draw_quit_dialog(f, app.quit_selection);
    }
}

// ---------------------------------------------------------------------------
// Header
// ---------------------------------------------------------------------------

fn draw_header(f: &mut Frame, app: &App, area: Rect) {
    let status = if app.online { "ONLINE" } else { "OFFLINE" };
    let status_style = if app.online {
        Style::default()
            .fg(Color::Green)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)
    };

    let interfaces_str = if app.interface_ips.is_empty() {
        "none".to_string()
    } else {
        app.interface_ips.join(", ")
    };

    let mut spans = vec![
        Span::styled(" Status: ", Style::default().fg(Color::DarkGray)),
        Span::styled(status, status_style),
        Span::raw("  |  "),
        Span::styled("Interfaces: ", Style::default().fg(Color::DarkGray)),
        Span::styled(interfaces_str, Style::default().fg(Color::Cyan)),
        Span::raw("  |  "),
        Span::styled("Port: ", Style::default().fg(Color::DarkGray)),
        Span::styled(format!("{}", app.port), Style::default().fg(Color::Cyan)),
    ];

    if let Some(hp) = app.http_port {
        spans.push(Span::raw("  |  "));
        spans.push(Span::styled("HTTP: ", Style::default().fg(Color::DarkGray)));
        spans.push(Span::styled(
            format!("{hp}"),
            Style::default().fg(Color::Cyan),
        ));
    }

    spans.push(Span::raw("  |  "));
    spans.push(Span::styled(
        "Directory: ",
        Style::default().fg(Color::DarkGray),
    ));
    spans.push(Span::styled(
        app.dir.display().to_string(),
        Style::default().fg(Color::Cyan),
    ));

    let line = Line::from(spans);

    let block = Block::default().borders(Borders::ALL).title(" tftp-rs ");
    let para = Paragraph::new(line).block(block);
    f.render_widget(para, area);
}

// ---------------------------------------------------------------------------
// Middle: Shared Files (left) + Active Transfers (right)
// ---------------------------------------------------------------------------

fn draw_middle(f: &mut Frame, app: &mut App, area: Rect) {
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(40), Constraint::Percentage(60)])
        .split(area);

    draw_shared_files(f, app, cols[0]);
    draw_transfers(f, app, cols[1]);
}

fn draw_shared_files(f: &mut Frame, app: &mut App, area: Rect) {
    let tree = build_tree(&app.dir, 0, &[]);
    let items: Vec<ListItem> = if tree.is_empty() {
        vec![ListItem::new(" (empty directory)")]
    } else {
        tree.iter()
            .map(|e| ListItem::new(format_tree_entry(e)))
            .collect()
    };

    let focused = app.focused_panel == FocusedPanel::Files;
    let border_style = if focused {
        Style::default().fg(Color::Cyan)
    } else {
        Style::default()
    };
    let title = if focused {
        " Shared Files (focused) "
    } else {
        " Shared Files "
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .title(title)
        .border_style(border_style);

    let inner_height = area.height.saturating_sub(2) as usize;
    let max_scroll = items.len().saturating_sub(inner_height) as u16;
    app.files_scroll = app.files_scroll.min(max_scroll);

    let visible: Vec<ListItem> = items
        .into_iter()
        .skip(app.files_scroll as usize)
        .take(inner_height)
        .collect();

    let list = List::new(visible)
        .block(block)
        .style(Style::default().fg(Color::White));
    f.render_widget(list, area);
}

fn draw_transfers(f: &mut Frame, app: &mut App, area: Rect) {
    let focused = app.focused_panel == FocusedPanel::Transfers;
    let border_style = if focused {
        Style::default().fg(Color::Cyan)
    } else {
        Style::default()
    };
    let title = if focused {
        " Active Transfers (focused) "
    } else {
        " Active Transfers "
    };

    if app.transfers.is_empty() {
        let block = Block::default()
            .borders(Borders::ALL)
            .title(title)
            .border_style(border_style);
        let para = Paragraph::new(" No active transfers")
            .style(Style::default().fg(Color::DarkGray))
            .block(block);
        f.render_widget(para, area);
        return;
    }

    let block = Block::default()
        .borders(Borders::ALL)
        .title(title)
        .border_style(border_style);
    let inner = block.inner(area);
    f.render_widget(block, area);

    let inner_height = inner.height as usize;
    // Each transfer uses 2 rows (info + gauge).
    let rows_per_transfer = 2usize;
    let visible_count = inner_height / rows_per_transfer;
    let max_scroll = app.transfers.len().saturating_sub(visible_count) as u16;
    app.transfers_scroll = app.transfers_scroll.min(max_scroll);

    let visible_transfers: Vec<&TransferInfo> = app
        .transfers
        .iter()
        .skip(app.transfers_scroll as usize)
        .take(visible_count)
        .collect();

    // Build constraints for visible transfers only.
    let constraints: Vec<Constraint> = visible_transfers
        .iter()
        .flat_map(|_| [Constraint::Length(1), Constraint::Length(1)])
        .chain(std::iter::once(Constraint::Min(0)))
        .collect();

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints(constraints)
        .split(inner);

    for (i, tf) in visible_transfers.iter().enumerate() {
        let idx = i * 2;
        if idx + 1 >= rows.len() {
            break;
        }

        let kind_str = match tf.kind {
            TransferKind::Download => "DL",
            TransferKind::Upload => "UL",
        };
        let kind_color = match tf.kind {
            TransferKind::Download => Color::Cyan,
            TransferKind::Upload => Color::Yellow,
        };

        let elapsed = tf.started.elapsed().as_secs_f64().max(0.001);
        let speed = tf.transferred as f64 / elapsed;
        let speed_str = format!("{}/s", human_bytes(speed as u64));

        let info_line = Line::from(vec![
            Span::styled(
                format!(" [{kind_str}] "),
                Style::default().fg(kind_color).add_modifier(Modifier::BOLD),
            ),
            Span::raw(format!("{} ", tf.filename)),
            Span::styled(
                format!("({}) ", tf.peer),
                Style::default().fg(Color::DarkGray),
            ),
            Span::styled(speed_str, Style::default().fg(Color::Green)),
        ]);
        f.render_widget(Paragraph::new(info_line), rows[idx]);

        // Progress gauge — different for downloads (known size) vs uploads.
        if tf.size_known {
            let ratio = if tf.total_bytes > 0 {
                (tf.transferred as f64 / tf.total_bytes as f64).min(1.0)
            } else {
                0.0
            };
            let label = format!(
                "{} / {}  ({:.0}%)",
                human_bytes(tf.transferred),
                human_bytes(tf.total_bytes),
                ratio * 100.0,
            );
            let gauge = Gauge::default()
                .gauge_style(Style::default().fg(kind_color).bg(Color::DarkGray))
                .label(label)
                .ratio(ratio);
            f.render_widget(gauge, rows[idx + 1]);
        } else {
            // Upload: total is unknown, show transferred bytes only.
            let label = format!("{} uploaded", human_bytes(tf.transferred));
            let gauge = Gauge::default()
                .gauge_style(Style::default().fg(kind_color).bg(Color::DarkGray))
                .label(label)
                .ratio(0.0);
            f.render_widget(gauge, rows[idx + 1]);
        }
    }
}

// ---------------------------------------------------------------------------
// Logs
// ---------------------------------------------------------------------------

fn draw_logs(f: &mut Frame, app: &mut App, area: Rect) {
    let focused = app.focused_panel == FocusedPanel::Logs;
    let border_style = if focused {
        Style::default().fg(Color::Cyan)
    } else {
        Style::default()
    };
    let title = if focused {
        " Logs (focused, Up/Down to scroll) "
    } else {
        " Logs (Tab to focus, Up/Down to scroll) "
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .title(title)
        .border_style(border_style);
    let inner_height = area.height.saturating_sub(2) as usize;
    let max_scroll = (app.logs.len() as u16).saturating_sub(inner_height as u16);
    app.log_scroll = app.log_scroll.min(max_scroll);

    let start = app.log_scroll as usize;
    let visible: Vec<ListItem> = app
        .logs
        .iter()
        .skip(start)
        .take(inner_height)
        .map(|l| ListItem::new(format!(" {l}")))
        .collect();

    let list = List::new(visible)
        .block(block)
        .style(Style::default().fg(Color::White));
    f.render_widget(list, area);
}

// ---------------------------------------------------------------------------
// Quit confirmation dialog
// ---------------------------------------------------------------------------

fn centered_rect(percent_x: u16, height: u16, area: Rect) -> Rect {
    let popup_layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length((area.height.saturating_sub(height)) / 2),
            Constraint::Length(height),
            Constraint::Min(0),
        ])
        .split(area);

    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(popup_layout[1])[1]
}

fn draw_quit_dialog(f: &mut Frame, selected_yes: bool) {
    let area = centered_rect(40, 5, f.area());
    f.render_widget(Clear, area);

    let block = Block::default()
        .borders(Borders::ALL)
        .title(" Quit ")
        .border_style(Style::default().fg(Color::Yellow));

    let yes_style = if selected_yes {
        Style::default()
            .fg(Color::Black)
            .bg(Color::Green)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::Green)
    };

    let no_style = if !selected_yes {
        Style::default()
            .fg(Color::Black)
            .bg(Color::Red)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::Red)
    };

    let text = Paragraph::new(Line::from(vec![
        Span::raw(" Are you sure?  "),
        Span::styled(" Yes ", yes_style),
        Span::raw("  "),
        Span::styled(" No ", no_style),
    ]))
    .block(block)
    .centered();

    f.render_widget(text, area);
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn human_bytes(b: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = 1024 * KB;
    const GB: u64 = 1024 * MB;
    if b >= GB {
        format!("{:.1} GB", b as f64 / GB as f64)
    } else if b >= MB {
        format!("{:.1} MB", b as f64 / MB as f64)
    } else if b >= KB {
        format!("{:.1} KB", b as f64 / KB as f64)
    } else {
        format!("{b} B")
    }
}
