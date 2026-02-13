mod http_server;
mod server;
mod tftp_protocol;
mod ui;

use std::fs::OpenOptions;
use std::io::{self, BufWriter};
use std::path::PathBuf;
use std::time::Duration;

use anyhow::Result;
use clap::Parser;
use crossterm::ExecutableCommand;
use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use tokio::sync::{mpsc, watch};

use server::ServerEvent;
use ui::App;

/// A high-performance TFTP server with a TUI dashboard.
#[derive(Parser, Debug)]
#[command(name = "tftp-rs", version, about)]
struct Cli {
    /// UDP port to listen on.
    #[arg(short, long, default_value_t = 69)]
    port: u16,

    /// Directory to serve / receive files.
    #[arg(short, long, default_value = ".")]
    dir: PathBuf,

    /// Optional file path to write logs to.
    #[arg(short, long)]
    log_file: Option<PathBuf>,

    /// Enable HTTP file server on the specified port. Shares the same directory as TFTP.
    #[arg(long)]
    http_port: Option<u16>,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    let dir = std::fs::canonicalize(&cli.dir)?;

    let log_writer = match cli.log_file {
        Some(ref path) => {
            let file = OpenOptions::new().create(true).append(true).open(path)?;
            Some(BufWriter::new(file))
        }
        None => None,
    };

    // Channel: server -> TUI event loop.
    let (ev_tx, mut ev_rx) = mpsc::unbounded_channel::<ServerEvent>();

    // Shutdown signal: TUI -> server.
    let (shutdown_tx, shutdown_rx) = watch::channel(false);

    // Clone shutdown receiver for HTTP server before TFTP server consumes it.
    let http_shutdown_rx = shutdown_rx.clone();

    // Spawn the TFTP server in the background.
    let server_handle = {
        let dir = dir.clone();
        let tx = ev_tx.clone();
        tokio::spawn(async move {
            if let Err(e) = server::run(cli.port, dir, tx.clone(), shutdown_rx).await {
                let _ = tx.send(ServerEvent::Log(format!("Server fatal: {e}")));
            }
        })
    };

    // Optionally spawn the HTTP file server.
    if let Some(http_port) = cli.http_port {
        let dir = dir.clone();
        let tx = ev_tx.clone();
        tokio::spawn(async move {
            if let Err(e) = http_server::run(http_port, dir, tx.clone(), http_shutdown_rx).await {
                let _ = tx.send(ServerEvent::Log(format!("HTTP server fatal: {e}")));
            }
        });
    }

    // ---------- TUI setup ----------
    enable_raw_mode()?;
    io::stdout().execute(EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = Terminal::new(backend)?;

    let mut app = App::new(cli.port, cli.http_port, dir, log_writer);
    app.online = true;
    app.push_log("Starting tftp-rs...".into());

    let result = run_tui(&mut terminal, &mut app, &mut ev_rx).await;

    // Log shutdown before cleanup.
    app.push_log("Shutting down...".into());

    // ---------- Cleanup ----------
    let _ = shutdown_tx.send(true);
    disable_raw_mode()?;
    io::stdout().execute(LeaveAlternateScreen)?;

    // Give the server a moment to shut down cleanly.
    let _ = tokio::time::timeout(Duration::from_millis(200), server_handle).await;

    result
}

/// Main TUI event loop.
async fn run_tui(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    app: &mut App,
    ev_rx: &mut mpsc::UnboundedReceiver<ServerEvent>,
) -> Result<()> {
    loop {
        // Draw.
        terminal.draw(|f| ui::draw(f, app))?;

        // Poll for server events (drain all pending).
        while let Ok(ev) = ev_rx.try_recv() {
            handle_server_event(app, ev);
        }

        // Periodically refresh interface IPs.
        app.refresh_interfaces_if_needed();

        // Poll for terminal / keyboard events with a short timeout so we
        // keep refreshing the screen.
        if event::poll(Duration::from_millis(100))?
            && let Event::Key(key) = event::read()?
            && key.kind == KeyEventKind::Press
        {
            if app.show_quit_dialog {
                match key.code {
                    KeyCode::Char('y') | KeyCode::Char('Y') => return Ok(()),
                    KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                        app.show_quit_dialog = false;
                    }
                    KeyCode::Left | KeyCode::Right | KeyCode::Tab => {
                        app.quit_selection = !app.quit_selection;
                    }
                    KeyCode::Enter => {
                        if app.quit_selection {
                            return Ok(());
                        } else {
                            app.show_quit_dialog = false;
                        }
                    }
                    _ => {}
                }
            } else {
                match key.code {
                    KeyCode::Char('q') | KeyCode::Esc => {
                        app.show_quit_dialog = true;
                        app.quit_selection = false;
                    }
                    KeyCode::Tab => app.cycle_focus(),
                    KeyCode::Up => app.scroll_up(),
                    KeyCode::Down => app.scroll_down(),
                    _ => {}
                }
            }
        }
    }
}

fn handle_server_event(app: &mut App, ev: ServerEvent) {
    match ev {
        ServerEvent::Log(msg) => app.push_log(msg),
        ServerEvent::TransferStarted(info) => {
            app.push_log(format!(
                "Transfer #{} started: {} {} ({})",
                info.id,
                match info.kind {
                    server::TransferKind::Download => "DL",
                    server::TransferKind::Upload => "UL",
                },
                info.filename,
                info.peer,
            ));
            app.transfers.push(info);
        }
        ServerEvent::TransferProgress {
            id,
            transferred,
            total_bytes,
        } => {
            if let Some(tf) = app.transfers.iter_mut().find(|t| t.id == id) {
                tf.transferred = transferred;
                tf.total_bytes = total_bytes;
            }
        }
        ServerEvent::TransferComplete(id) => {
            app.transfers.retain(|t| t.id != id);
            app.push_log(format!("Transfer #{id} complete"));
        }
        ServerEvent::TransferFailed { id, error } => {
            app.transfers.retain(|t| t.id != id);
            app.push_log(format!("Transfer #{id} failed: {error}"));
        }
    }
}
