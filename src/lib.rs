//! Embeddable asynchronous TFTP server, and the library behind the `tftp-rs`
//! dashboard.
//!
//! The server implements TFTP from scratch on
//! [tokio](https://docs.rs/tokio): RFC 1350, the option extension
//! (RFC 2347), and the blksize (RFC 2348), timeout and tsize (RFC 2349) and
//! windowsize (RFC 7440) options. It serves downloads (RRQ) and uploads
//! (WRQ) from one directory and its subdirectories, in octet and netascii
//! mode. RFC 1350's obsolete mail mode is accepted and handled as octet.
//!
//! Everything lives in [`server`]:
//!
//! - [`run`](server::run) serves on one local address, and
//!   [`run_on_interface`](server::run_on_interface) also pins every socket to
//!   one network interface.
//! - [`ServerConfig`](server::ServerConfig) holds the settings: timeouts,
//!   block and window sizes, read and write access, overwrite protection, and
//!   limits on concurrent transfers and upload size.
//! - [`ServerEvent`](server::ServerEvent) reports log lines and transfer
//!   progress, for callers that want to show them.
//!
//! # Quick start
//!
//! ```no_run
//! use std::net::SocketAddr;
//!
//! use tftp_rs::server::{run, ServerConfig, ServerEvent};
//! use tokio::sync::{mpsc, watch};
//!
//! #[tokio::main]
//! async fn main() -> anyhow::Result<()> {
//!     let bind: SocketAddr = "192.0.2.1:69".parse()?;
//!     let (events, mut event_rx) = mpsc::unbounded_channel();
//!     // Send a value (conventionally `true`) to stop the server. Dropping the
//!     // sender stops it too, so keep it alive for as long as the server
//!     // should run.
//!     let (_shutdown_tx, shutdown_rx) = watch::channel(false);
//!
//!     // Print what the server logs. Leave this out, and drop `event_rx`,
//!     // if you do not need it: the server runs the same either way.
//!     tokio::spawn(async move {
//!         while let Some(event) = event_rx.recv().await {
//!             if let ServerEvent::Log(line) = event {
//!                 println!("{line}");
//!             }
//!         }
//!     });
//!
//!     let config = ServerConfig {
//!         max_window_size: 8,
//!         ..ServerConfig::default()
//!     };
//!     run(bind, "/srv/tftp".into(), events, shutdown_rx, config).await
//! }
//! ```
//!
//! # Feature flags
//!
//! - `tui` (default): builds the `tftp-rs` binary, with its terminal
//!   dashboard and HTTP file server. The library does not use it, so
//!   embedders can turn it off and skip those dependencies:
//!
//!   ```toml
//!   [dependencies]
//!   tftp-rs = { version = "0.4", default-features = false }
//!   ```
//!
//! # Platform support
//!
//! Linux, macOS and Windows. Binding to a named interface
//! ([`run_on_interface`](server::run_on_interface),
//! [`validate_interface`](server::validate_interface), and the `interface`
//! argument of [`probe_bind`](server::probe_bind) and
//! [`bind_tcp_listener`](server::bind_tcp_listener)) works on Linux and macOS
//! only, and returns an error elsewhere rather than binding to every
//! interface.
//!
//! On Linux the default TFTP port, 69, is privileged: serving it needs root
//! or `CAP_NET_BIND_SERVICE`. Other Unix systems may need root as well;
//! macOS and Windows let any user bind it.
//!
//! # Security
//!
//! TFTP has no authentication and no encryption. Anyone who can reach the
//! port can download every file in the served directory and, unless
//! [`enable_write`](server::ServerConfig::enable_write) is off, upload files.
//! Limit who can reach it with the bind address, the interface and a
//! firewall.
//!
//! What the server does enforce: every requested path stays inside the
//! served directory ([`sanitize_path`](server::sanitize_path)), an upload is
//! written under a staging name
//! ([`is_upload_staging_name`](server::is_upload_staging_name)) and appears
//! under its own name only once it is complete, and the limits in
//! [`ServerConfig`](server::ServerConfig) bound what clients can make it hold
//! open or write. Upload size is unlimited by default: set
//! [`max_upload_bytes`](server::ServerConfig::max_upload_bytes) to cap it.

#![warn(missing_docs)]

pub mod server;

mod tftp_protocol;
