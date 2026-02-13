use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Result;
use axum::Router;
use axum::body::Body;
use axum::extract::{ConnectInfo, Request, State};
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Response};
use tokio::sync::{mpsc, watch};
use tokio_util::io::ReaderStream;

use crate::server::{ServerEvent, sanitize_path};

struct HttpState {
    dir: PathBuf,
    tx: mpsc::UnboundedSender<ServerEvent>,
}

pub async fn run(
    port: u16,
    dir: PathBuf,
    tx: mpsc::UnboundedSender<ServerEvent>,
    mut shutdown: watch::Receiver<bool>,
) -> Result<()> {
    let state = Arc::new(HttpState {
        dir,
        tx: tx.clone(),
    });

    let app = Router::new()
        .fallback(serve_path)
        .with_state(state)
        .into_make_service_with_connect_info::<SocketAddr>();

    let addr = format!("0.0.0.0:{port}");
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    tx.send(ServerEvent::Log(format!("HTTP server listening on {addr}")))?;

    axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            let _ = shutdown.changed().await;
        })
        .await?;

    Ok(())
}

async fn serve_path(
    State(state): State<Arc<HttpState>>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    request: Request,
) -> Response {
    let uri_path = percent_decode(request.uri().path());
    let stripped = uri_path.trim_start_matches('/');

    let _ = state
        .tx
        .send(ServerEvent::Log(format!("{addr}: HTTP GET /{stripped}")));

    // Root directory listing.
    if stripped.is_empty() {
        return match render_directory(&state.dir, "/") {
            Ok(html) => Html(html).into_response(),
            Err(_) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Failed to read directory",
            )
                .into_response(),
        };
    }

    // Try to resolve the path using the same sanitization as TFTP.
    let resolved = match sanitize_path(&state.dir, stripped) {
        Ok(p) => p,
        Err(_) => {
            return (StatusCode::NOT_FOUND, "Not found").into_response();
        }
    };

    if resolved.is_dir() {
        match render_directory(&resolved, &uri_path) {
            Ok(html) => Html(html).into_response(),
            Err(_) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Failed to read directory",
            )
                .into_response(),
        }
    } else if resolved.is_file() {
        let ct = content_type_for(&resolved);
        let filename = resolved
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();

        // Get file size for Content-Length header.
        let file_size = match tokio::fs::metadata(&resolved).await {
            Ok(m) => m.len(),
            Err(_) => {
                return (StatusCode::INTERNAL_SERVER_ERROR, "Failed to read file").into_response();
            }
        };

        // Stream the file instead of loading it all into memory.
        let file = match tokio::fs::File::open(&resolved).await {
            Ok(f) => f,
            Err(_) => {
                return (StatusCode::INTERNAL_SERVER_ERROR, "Failed to read file").into_response();
            }
        };
        let stream = ReaderStream::new(file);
        let body = Body::from_stream(stream);

        (
            [
                ("content-type", ct.to_string()),
                ("content-length", file_size.to_string()),
                (
                    "content-disposition",
                    format!("inline; filename=\"{filename}\""),
                ),
            ],
            body,
        )
            .into_response()
    } else {
        (StatusCode::NOT_FOUND, "Not found").into_response()
    }
}

fn render_directory(dir: &Path, display_path: &str) -> std::io::Result<String> {
    let mut entries: Vec<_> = std::fs::read_dir(dir)?.filter_map(|e| e.ok()).collect();

    // Sort: directories first, then alphabetical.
    entries.sort_by(|a, b| {
        let a_dir = a.file_type().map(|ft| ft.is_dir()).unwrap_or(false);
        let b_dir = b.file_type().map(|ft| ft.is_dir()).unwrap_or(false);
        b_dir.cmp(&a_dir).then_with(|| {
            a.file_name()
                .to_string_lossy()
                .to_lowercase()
                .cmp(&b.file_name().to_string_lossy().to_lowercase())
        })
    });

    let mut html = String::new();
    html.push_str("<!DOCTYPE html><html><head><meta charset=\"utf-8\">");
    html.push_str("<title>Index of ");
    html.push_str(&html_escape(display_path));
    html.push_str("</title>");
    html.push_str("<style>");
    html.push_str(
        "body { font-family: monospace; max-width: 900px; margin: 40px auto; padding: 0 20px; }",
    );
    html.push_str("h1 { border-bottom: 1px solid #ccc; padding-bottom: 10px; }");
    html.push_str("table { border-collapse: collapse; width: 100%; }");
    html.push_str("td, th { text-align: left; padding: 4px 12px; }");
    html.push_str("a { text-decoration: none; color: #0366d6; }");
    html.push_str("a:hover { text-decoration: underline; }");
    html.push_str(".size { color: #666; }");
    html.push_str("</style>");
    html.push_str("</head><body>");
    html.push_str("<h1>Index of ");
    html.push_str(&html_escape(display_path));
    html.push_str("</h1>");

    html.push_str("<table><tr><th>Name</th><th>Size</th></tr>");

    // Parent directory link.
    if display_path != "/" {
        let parent = display_path
            .trim_end_matches('/')
            .rsplit_once('/')
            .map(|(p, _)| if p.is_empty() { "/" } else { p })
            .unwrap_or("/");
        html.push_str("<tr><td><a href=\"");
        html.push_str(parent);
        html.push_str("\">..</a></td><td></td></tr>");
    }

    for entry in &entries {
        let name = entry.file_name().to_string_lossy().to_string();
        let is_dir = entry.file_type().map(|ft| ft.is_dir()).unwrap_or(false);

        let base = display_path.trim_end_matches('/');
        let href = if is_dir {
            format!("{base}/{name}/")
        } else {
            format!("{base}/{name}")
        };

        let display_name = if is_dir {
            format!("{name}/")
        } else {
            name.clone()
        };

        let size_str = if is_dir {
            "-".to_string()
        } else {
            entry
                .metadata()
                .ok()
                .map(|m| human_bytes(m.len()))
                .unwrap_or_default()
        };

        html.push_str("<tr><td><a href=\"");
        html.push_str(&html_escape(&href));
        html.push_str("\">");
        html.push_str(&html_escape(&display_name));
        html.push_str("</a></td><td class=\"size\">");
        html.push_str(&size_str);
        html.push_str("</td></tr>");
    }

    html.push_str("</table></body></html>");
    Ok(html)
}

fn content_type_for(path: &Path) -> &'static str {
    match path.extension().and_then(|e| e.to_str()) {
        Some("html" | "htm") => "text/html",
        Some("css") => "text/css",
        Some("js") => "application/javascript",
        Some("json") => "application/json",
        Some("xml") => "application/xml",
        Some("txt" | "cfg" | "log" | "conf" | "ini" | "yaml" | "yml" | "toml") => "text/plain",
        Some("png") => "image/png",
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("svg") => "image/svg+xml",
        Some("ico") => "image/x-icon",
        Some("pdf") => "application/pdf",
        Some("zip") => "application/zip",
        Some("gz" | "tgz") => "application/gzip",
        Some("tar") => "application/x-tar",
        Some("bin" | "img" | "iso") => "application/octet-stream",
        _ => "application/octet-stream",
    }
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

fn percent_decode(s: &str) -> String {
    let mut result = Vec::new();
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let Ok(val) = u8::from_str_radix(&String::from_utf8_lossy(&bytes[i + 1..i + 3]), 16)
        {
            result.push(val);
            i += 3;
            continue;
        }
        result.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&result).to_string()
}

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
