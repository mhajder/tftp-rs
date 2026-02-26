use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::Instant;

use anyhow::{Result, anyhow};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UdpSocket;
use tokio::sync::mpsc;
use tokio::time::{Duration, timeout};

use crate::tftp_protocol::{
    BLOCK_SIZE, DEFAULT_WINDOWSIZE, MAX_BLKSIZE, MAX_TIMEOUT, MIN_TIMEOUT, NetasciiDecoder,
    NetasciiEncoder, Packet,
};

/// Maximum UDP datagram size we ever expect (4-byte header + max blksize).
const MAX_PACKET: usize = 4 + MAX_BLKSIZE;

/// Default timeout before retransmitting (milliseconds).
const DEFAULT_TIMEOUT_MS: u64 = 500;

/// Maximum retransmission attempts before giving up.
const MAX_RETRIES: u32 = 10;

/// The largest TFTP blksize the OS will allow in a single UDP send.
/// Detected once at startup by probing the kernel.
static MAX_SENDABLE_BLKSIZE: OnceLock<usize> = OnceLock::new();

// ---------------------------------------------------------------------------
// Server configuration
// ---------------------------------------------------------------------------

/// Runtime-configurable server settings.
#[derive(Debug, Clone)]
pub struct ServerConfig {
    /// Reply timeout in milliseconds (default 500).
    /// Can be overridden by client via RFC 2349 timeout option.
    pub timeout_ms: u64,
    /// Maximum allowed blksize. Useful when serving behind a VPN.
    /// 0 means use OS-detected maximum.
    pub max_block_size: usize,
    /// Maximum allowed window size (RFC 7440). 0 or 1 disables windowing.
    pub max_window_size: u16,
    /// Whether to allow file overwrites on WRQ. When false, WRQ for
    /// existing files returns an error.
    pub allow_overwrite: bool,
    /// Maximum retransmission attempts before giving up.
    pub max_retries: u32,
    /// Whether to enable read (RRQ) requests.
    pub enable_read: bool,
    /// Whether to enable write (WRQ) requests.
    pub enable_write: bool,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            timeout_ms: DEFAULT_TIMEOUT_MS,
            max_block_size: 0,
            max_window_size: 1,
            allow_overwrite: true,
            max_retries: MAX_RETRIES,
            enable_read: true,
            enable_write: true,
        }
    }
}

/// Groups the per-transfer "infrastructure" parameters passed to RRQ/WRQ
/// handlers, keeping the argument count within the linter threshold.
struct TransferContext {
    id: u64,
    peer: SocketAddr,
    dir: Arc<PathBuf>,
    tx: mpsc::UnboundedSender<ServerEvent>,
    config: Arc<ServerConfig>,
}

/// Probe the kernel for the largest UDP datagram it will accept in a
/// single `sendto` call, then subtract the 4-byte TFTP header.
///
/// On macOS the limit is `net.inet.udp.maxdgram` (default 9216 bytes).
/// `SO_SNDBUF` only controls queue depth, not per-datagram size.
/// On Linux the practical limit is much higher (~65507).
fn detect_max_blksize() -> usize {
    let sock = match socket2::Socket::new(
        socket2::Domain::IPV4,
        socket2::Type::DGRAM,
        Some(socket2::Protocol::UDP),
    ) {
        Ok(s) => s,
        Err(_) => return BLOCK_SIZE,
    };

    let _ = sock.set_send_buffer_size(256 * 1024);

    if sock
        .bind(&"127.0.0.1:0".parse::<SocketAddr>().unwrap().into())
        .is_err()
    {
        return BLOCK_SIZE;
    }

    // Send to the discard port on loopback.  The kernel validates the
    // datagram size synchronously before queueing so this never leaves
    // the machine.
    let dest: socket2::SockAddr = "127.0.0.1:9".parse::<SocketAddr>().unwrap().into();

    // Try from largest to smallest.  The first size the kernel accepts
    // (or rejects for a non-size reason like ECONNREFUSED) is our cap.
    let candidates = [
        MAX_BLKSIZE + 4, // 65468
        32768,
        16384,
        9216,
        8192,
        4096,
        1024,
        516,
    ];

    let buf = vec![0u8; candidates[0]];
    for &size in &candidates {
        match sock.send_to(&buf[..size], &dest) {
            Ok(_) => return size.saturating_sub(4),
            Err(e) => {
                let raw = e.raw_os_error().unwrap_or(0);
                // EMSGSIZE (40 macOS, 90 Linux) or ENOBUFS (55 macOS)
                // → datagram is too large, try smaller.
                if raw == 40 || raw == 55 || raw == 90 {
                    continue;
                }
                // Any other error (e.g. ECONNREFUSED) means the kernel
                // accepted the datagram size — delivery just failed.
                return size.saturating_sub(4);
            }
        }
    }

    BLOCK_SIZE
}

/// Return the maximum blksize the OS can send in one UDP datagram.
fn max_blksize() -> usize {
    *MAX_SENDABLE_BLKSIZE.get_or_init(detect_max_blksize)
}

/// Maximum number of retries when the kernel returns ENOBUFS (macOS error 55)
/// because the outgoing buffer is temporarily full.
const ENOBUFS_MAX_RETRIES: u32 = 50;

/// Send a datagram, retrying transparently on transient ENOBUFS (os error 55)
/// errors that macOS produces when the outgoing buffer is momentarily full.
async fn send_resilient(sock: &UdpSocket, buf: &[u8]) -> Result<()> {
    for attempt in 0..ENOBUFS_MAX_RETRIES {
        match sock.send(buf).await {
            Ok(_) => return Ok(()),
            Err(e) if e.raw_os_error() == Some(55) => {
                // ENOBUFS — back off briefly with exponential delay
                // (1 ms, 2 ms, 4 ms … capped at 50 ms).
                let delay_ms = (1u64 << attempt.min(5)).min(50);
                tokio::time::sleep(Duration::from_millis(delay_ms)).await;
            }
            Err(e) => return Err(e.into()),
        }
    }
    Err(anyhow!(
        "send failed: No buffer space available (os error 55) after {ENOBUFS_MAX_RETRIES} retries"
    ))
}

/// Create an ephemeral UDP socket with send/receive buffers sized for the
/// negotiated block size.  The OS default buffer (~9 KB on macOS) is too
/// small for blksize values above ~8 KB and causes "No buffer space
/// available" (ENOBUFS / os error 55).
async fn bind_transfer_socket(peer: SocketAddr, blksize: usize) -> Result<UdpSocket> {
    // Build the socket via socket2 so we can set buffer sizes before
    // handing it to tokio.
    let domain = if peer.is_ipv6() {
        socket2::Domain::IPV6
    } else {
        socket2::Domain::IPV4
    };
    let raw = socket2::Socket::new(domain, socket2::Type::DGRAM, Some(socket2::Protocol::UDP))?;

    // Need room for a comfortable number of packets in flight.
    let buf_size = ((4 + blksize) * 16).max(256 * 1024);
    let _ = raw.set_send_buffer_size(buf_size);
    let _ = raw.set_recv_buffer_size(buf_size);

    // Bind to an OS-assigned port.
    let bind_addr: SocketAddr = if peer.is_ipv6() {
        "[::]:0".parse().unwrap()
    } else {
        "0.0.0.0:0".parse().unwrap()
    };
    raw.bind(&bind_addr.into())?;
    raw.set_nonblocking(true)?;

    // Convert: socket2 -> std -> tokio.
    let std_sock: std::net::UdpSocket = raw.into();
    let sock = UdpSocket::from_std(std_sock)?;
    sock.connect(peer).await?;

    Ok(sock)
}

// ---------------------------------------------------------------------------
// Shared state exposed to the TUI
// ---------------------------------------------------------------------------

/// Direction of a transfer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransferKind {
    Download, // RRQ  (client is downloading from us)
    Upload,   // WRQ  (client is uploading to us)
}

/// A snapshot of a running transfer, suitable for display.
#[derive(Debug, Clone)]
pub struct TransferInfo {
    pub id: u64,
    pub peer: SocketAddr,
    pub filename: String,
    pub kind: TransferKind,
    /// Total file size in bytes (known for downloads only).
    pub total_bytes: u64,
    /// Bytes transferred so far.
    pub transferred: u64,
    /// Transfer start time.
    pub started: Instant,
    /// Whether the total file size is known (true for downloads, false for uploads).
    pub size_known: bool,
}

/// Events emitted by the server for the TUI.
#[derive(Debug, Clone)]
pub enum ServerEvent {
    Log(String),
    TransferStarted(TransferInfo),
    TransferProgress {
        id: u64,
        transferred: u64,
        total_bytes: u64,
    },
    TransferComplete(u64),
    TransferFailed {
        id: u64,
        error: String,
    },
}

// ---------------------------------------------------------------------------
// Option negotiation helpers
// ---------------------------------------------------------------------------

/// Negotiated transfer parameters.
#[derive(Debug, Clone)]
struct NegotiatedOptions {
    /// Block size for DATA payloads.
    blksize: usize,
    /// Transfer timeout in milliseconds.
    timeout_ms: u64,
    /// Window size (number of DATA packets sent before waiting for ACK).
    windowsize: u16,
    /// Options to send in OACK.
    oack: HashMap<String, String>,
}

/// Negotiate all RFC options from client request.
/// `config` provides the server-side limits.
fn negotiate_options(
    client_options: &HashMap<String, String>,
    config: &ServerConfig,
) -> NegotiatedOptions {
    let mut acked = HashMap::new();
    let mut blksize = BLOCK_SIZE;
    let mut timeout_ms = config.timeout_ms;
    let mut windowsize = DEFAULT_WINDOWSIZE;

    let os_max = max_blksize();
    let effective_max_blksize = if config.max_block_size > 0 {
        config.max_block_size.min(os_max)
    } else {
        os_max
    };

    // RFC 2348: blksize option.
    if let Some(val) = client_options.get("blksize")
        && let Ok(requested) = val.parse::<usize>()
        && (8..=MAX_BLKSIZE).contains(&requested)
    {
        blksize = requested.min(effective_max_blksize);
        acked.insert("blksize".to_string(), blksize.to_string());
    }

    // RFC 2349: timeout option (in seconds).
    if let Some(val) = client_options.get("timeout")
        && let Ok(requested) = val.parse::<u8>()
        && (MIN_TIMEOUT..=MAX_TIMEOUT).contains(&requested)
    {
        timeout_ms = (requested as u64) * 1000;
        acked.insert("timeout".to_string(), requested.to_string());
    }

    // RFC 7440: windowsize option.
    if let Some(val) = client_options.get("windowsize")
        && let Ok(requested) = val.parse::<u16>()
        && requested >= 1
    {
        let max_win = if config.max_window_size > 0 {
            config.max_window_size
        } else {
            1
        };
        windowsize = requested.min(max_win);
        if windowsize > 1 {
            acked.insert("windowsize".to_string(), windowsize.to_string());
        }
    }

    // tsize option: signal that we should report tsize; the caller fills in the value.
    if client_options.contains_key("tsize") {
        acked.insert("tsize".to_string(), "0".to_string());
    }

    NegotiatedOptions {
        blksize,
        timeout_ms,
        windowsize,
        oack: acked,
    }
}

// ---------------------------------------------------------------------------
// Server entry-point
// ---------------------------------------------------------------------------

/// Run the TFTP server. Returns when `shutdown` is dropped.
pub async fn run(
    port: u16,
    dir: PathBuf,
    tx: mpsc::UnboundedSender<ServerEvent>,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
    config: ServerConfig,
) -> Result<()> {
    let addr: SocketAddr = format!("0.0.0.0:{port}").parse()?;
    let sock = UdpSocket::bind(addr).await?;
    tx.send(ServerEvent::Log(format!("Listening on {addr}")))?;

    let detected_blksize = max_blksize();
    let effective_max_blksize = if config.max_block_size > 0 {
        config.max_block_size.min(detected_blksize)
    } else {
        detected_blksize
    };
    tx.send(ServerEvent::Log(format!(
        "Max negotiable blksize: {effective_max_blksize}"
    )))?;
    if config.max_window_size > 1 {
        tx.send(ServerEvent::Log(format!(
            "Max window size: {}",
            config.max_window_size
        )))?;
    }
    tx.send(ServerEvent::Log(format!(
        "Default timeout: {}ms",
        config.timeout_ms
    )))?;

    let dir = Arc::new(dir);
    let config = Arc::new(config);
    let mut buf = vec![0u8; MAX_PACKET];
    let mut next_id: u64 = 1;

    // Track in-progress transfers to reject duplicate requests from the same peer.
    let reqs_in_progress: Arc<tokio::sync::Mutex<HashSet<SocketAddr>>> =
        Arc::new(tokio::sync::Mutex::new(HashSet::new()));

    loop {
        tokio::select! {
            result = sock.recv_from(&mut buf) => {
                let (n, peer) = result?;
                let pkt = match Packet::from_bytes(&buf[..n]) {
                    Ok(p) => p,
                    Err(e) => {
                        let _ = tx.send(ServerEvent::Log(format!("{peer}: bad packet: {e}")));
                        continue;
                    }
                };

                match pkt {
                    Packet::RRQ { filename, mode, options } => {
                        if !config.enable_read {
                            let _ = tx.send(ServerEvent::Log(format!("{peer}: RRQ rejected (reads disabled)")));
                            // Send error on a temporary socket.
                            if let Ok(tmp) = UdpSocket::bind("0.0.0.0:0").await {
                                let err = Packet::ERROR { code: 2, msg: "Read access denied".into() };
                                let _ = tmp.send_to(&err.to_bytes(), peer).await;
                            }
                            continue;
                        }

                        // Reject duplicate request from same peer.
                        {
                            let mut in_progress = reqs_in_progress.lock().await;
                            if !in_progress.insert(peer) {
                                let _ = tx.send(ServerEvent::Log(format!("{peer}: duplicate RRQ ignored (transfer in progress)")));
                                continue;
                            }
                        }

                        let id = next_id;
                        next_id += 1;
                        let tx2 = tx.clone();
                        let dir2 = Arc::clone(&dir);
                        let cfg = Arc::clone(&config);
                        let rip = Arc::clone(&reqs_in_progress);
                        tokio::spawn(async move {
                            let result = handle_rrq(TransferContext { id, peer, dir: dir2, tx: tx2.clone(), config: cfg }, &filename, &mode, &options).await;
                            rip.lock().await.remove(&peer);
                            if let Err(e) = result {
                                let _ = tx2.send(ServerEvent::TransferFailed { id, error: e.to_string() });
                                let _ = tx2.send(ServerEvent::Log(format!("{peer}: RRQ error: {e}")));
                            }
                        });
                    }
                    Packet::WRQ { filename, mode, options } => {
                        if !config.enable_write {
                            let _ = tx.send(ServerEvent::Log(format!("{peer}: WRQ rejected (writes disabled)")));
                            if let Ok(tmp) = UdpSocket::bind("0.0.0.0:0").await {
                                let err = Packet::ERROR { code: 2, msg: "Write access denied".into() };
                                let _ = tmp.send_to(&err.to_bytes(), peer).await;
                            }
                            continue;
                        }

                        // Reject duplicate request from same peer.
                        {
                            let mut in_progress = reqs_in_progress.lock().await;
                            if !in_progress.insert(peer) {
                                let _ = tx.send(ServerEvent::Log(format!("{peer}: duplicate WRQ ignored (transfer in progress)")));
                                continue;
                            }
                        }

                        let id = next_id;
                        next_id += 1;
                        let tx2 = tx.clone();
                        let dir2 = Arc::clone(&dir);
                        let cfg = Arc::clone(&config);
                        let rip = Arc::clone(&reqs_in_progress);
                        tokio::spawn(async move {
                            let result = handle_wrq(TransferContext { id, peer, dir: dir2.clone(), tx: tx2.clone(), config: cfg }, &filename, &mode, &options).await;
                            rip.lock().await.remove(&peer);
                            if let Err(e) = result {
                                // Clean up the incomplete .part file.
                                if let Ok(final_path) = sanitize_path(&dir2, &filename) {
                                    let mut part = final_path.into_os_string();
                                    part.push(".part");
                                    let _ = tokio::fs::remove_file(PathBuf::from(part)).await;
                                }
                                let _ = tx2.send(ServerEvent::TransferFailed { id, error: e.to_string() });
                                let _ = tx2.send(ServerEvent::Log(format!("{peer}: WRQ error: {e}")));
                            }
                        });
                    }
                    other => {
                        let _ = tx.send(ServerEvent::Log(format!(
                            "{peer}: unexpected packet on listener: {other:?}"
                        )));
                    }
                }
            }
            _ = shutdown.changed() => {
                tx.send(ServerEvent::Log("Shutting down".into()))?;
                break;
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// RRQ handler  (client downloads a file from us)
// ---------------------------------------------------------------------------

async fn handle_rrq(
    ctx: TransferContext,
    filename: &str,
    mode: &str,
    options: &HashMap<String, String>,
) -> Result<()> {
    let TransferContext {
        id,
        peer,
        dir,
        tx,
        config,
    } = ctx;
    let dir = dir.as_path();
    let config = config.as_ref();
    let path = sanitize_path(dir, filename)?;
    let metadata = tokio::fs::metadata(&path)
        .await
        .map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => {
                anyhow!("file not found: {}", path.display())
            }
            std::io::ErrorKind::PermissionDenied => {
                anyhow!("permission denied: {}", path.display())
            }
            _ => anyhow!("cannot read {}: {e}", path.display()),
        })?;
    let total_bytes = metadata.len();

    let is_netascii = mode == "netascii";

    // Negotiate options (blksize, timeout, windowsize, tsize).
    let negotiated = negotiate_options(options, config);
    let blksize = negotiated.blksize;
    let timeout_dur = Duration::from_millis(negotiated.timeout_ms);
    let windowsize = negotiated.windowsize;
    let mut oack_options = negotiated.oack;

    // Fill in tsize if the client requested it.
    if oack_options.contains_key("tsize") {
        oack_options.insert("tsize".to_string(), total_bytes.to_string());
    }

    let mut detail_parts = Vec::new();
    if blksize != BLOCK_SIZE {
        detail_parts.push(format!("blksize={blksize}"));
    }
    if windowsize > 1 {
        detail_parts.push(format!("windowsize={windowsize}"));
    }
    if negotiated.timeout_ms != config.timeout_ms {
        detail_parts.push(format!("timeout={}ms", negotiated.timeout_ms));
    }
    if is_netascii {
        detail_parts.push("netascii".to_string());
    }
    let detail_str = if detail_parts.is_empty() {
        String::new()
    } else {
        format!(" [{}]", detail_parts.join(", "))
    };

    tx.send(ServerEvent::Log(format!(
        "{peer}: RRQ \"{filename}\" ({total_bytes} bytes){detail_str}"
    )))?;
    tx.send(ServerEvent::TransferStarted(TransferInfo {
        id,
        peer,
        filename: filename.to_string(),
        kind: TransferKind::Download,
        total_bytes,
        transferred: 0,
        started: Instant::now(),
        size_known: true,
    }))?;

    // Bind an ephemeral socket for this transfer with appropriately sized buffers.
    let sock = bind_transfer_socket(peer, blksize).await?;
    let mut recv_buf = vec![0u8; MAX_PACKET];
    let max_retries = config.max_retries;

    // Send OACK if we have negotiated options, then wait for ACK 0.
    if !oack_options.is_empty() {
        let oack_pkt = Packet::OACK {
            options: oack_options,
        };
        let oack_bytes = oack_pkt.to_bytes();

        let mut retries = 0u32;
        loop {
            send_resilient(&sock, &oack_bytes).await?;
            match timeout(timeout_dur, sock.recv(&mut recv_buf)).await {
                Ok(Ok(n)) => {
                    let ack = Packet::from_bytes(&recv_buf[..n])?;
                    match ack {
                        Packet::ACK { block_num: 0 } => break,
                        Packet::ERROR { code, msg } => {
                            return Err(anyhow!("client error {code}: {msg}"));
                        }
                        _ => { /* retry */ }
                    }
                }
                Ok(Err(e)) => return Err(e.into()),
                Err(_) => {
                    retries += 1;
                    if retries > max_retries {
                        return Err(anyhow!("timeout waiting for OACK acknowledgment"));
                    }
                }
            }
        }
    }

    // Stream the file.
    let mut file = tokio::fs::File::open(&path)
        .await
        .map_err(|e| anyhow!("cannot open {}: {e}", path.display()))?;
    let mut block_buf = vec![0u8; blksize];
    let mut block_num: u16 = 1;
    let mut transferred: u64 = 0;
    let mut encoder = if is_netascii {
        Some(NetasciiEncoder::new())
    } else {
        None
    };

    // --- Windowed transfer (RFC 7440) ---
    if windowsize > 1 {
        // Read and send `windowsize` DATA blocks, then wait for ACK.
        // The ACK may acknowledge any block in the window.
        loop {
            let mut window: Vec<(u16, Vec<u8>)> = Vec::with_capacity(windowsize as usize);
            let mut last_block = false;

            // Fill the window.
            for _ in 0..windowsize {
                let payload =
                    read_next_block(&mut file, &mut block_buf, blksize, &mut encoder).await?;
                let is_last = payload.len() < blksize && !has_encoder_overflow(&encoder);
                window.push((block_num, payload));
                if is_last {
                    last_block = true;
                    break;
                }
                block_num = block_num.wrapping_add(1);
            }

            let window_end = window.last().map(|(bn, _)| *bn).unwrap_or(block_num);

            // Send all blocks in the window.
            let mut retries = 0u32;
            loop {
                for (bn, payload) in &window {
                    let mut pkt_bytes = Vec::with_capacity(4 + payload.len());
                    pkt_bytes.extend_from_slice(&3u16.to_be_bytes());
                    pkt_bytes.extend_from_slice(&bn.to_be_bytes());
                    pkt_bytes.extend_from_slice(payload);
                    send_resilient(&sock, &pkt_bytes).await?;
                }

                // Wait for ACK for any block in the window.
                match timeout(timeout_dur, sock.recv(&mut recv_buf)).await {
                    Ok(Ok(n)) => {
                        let ack = Packet::from_bytes(&recv_buf[..n])?;
                        match ack {
                            Packet::ACK { block_num: bn } => {
                                // Check if this ACK is for the end of our window.
                                if bn == window_end {
                                    // Full window acknowledged.
                                    let total_payload: u64 =
                                        window.iter().map(|(_, p)| p.len() as u64).sum();
                                    transferred += total_payload;
                                    break;
                                } else if is_in_window(bn, &window) {
                                    // Partial ACK — remove acknowledged blocks and resend rest.
                                    let mut acked_bytes: u64 = 0;
                                    while !window.is_empty() && window[0].0 != bn.wrapping_add(1) {
                                        let (_, p) = window.remove(0);
                                        acked_bytes += p.len() as u64;
                                        if window.is_empty() {
                                            break;
                                        }
                                    }
                                    transferred += acked_bytes;
                                    // Continue loop to resend remaining blocks.
                                    retries = 0;
                                    continue;
                                }
                                // ACK for wrong block, retry.
                            }
                            Packet::ERROR { code, msg } => {
                                return Err(anyhow!("client error {code}: {msg}"));
                            }
                            _ => {}
                        }
                    }
                    Ok(Err(e)) => return Err(e.into()),
                    Err(_) => {
                        retries += 1;
                        if retries > max_retries {
                            return Err(anyhow!("timeout after {max_retries} retries"));
                        }
                        // Resend the entire window.
                        continue;
                    }
                }
            }

            tx.send(ServerEvent::TransferProgress {
                id,
                transferred,
                total_bytes,
            })?;

            if last_block {
                break;
            }
            block_num = block_num.wrapping_add(1);
        }
    } else {
        // --- Classic single-block transfer ---
        loop {
            let payload = read_next_block(&mut file, &mut block_buf, blksize, &mut encoder).await?;
            let is_last = payload.len() < blksize && !has_encoder_overflow(&encoder);

            let mut pkt_bytes = Vec::with_capacity(4 + payload.len());
            pkt_bytes.extend_from_slice(&3u16.to_be_bytes()); // OPCODE_DATA
            pkt_bytes.extend_from_slice(&block_num.to_be_bytes());
            pkt_bytes.extend_from_slice(&payload);

            let mut retries = 0u32;
            loop {
                send_resilient(&sock, &pkt_bytes).await?;
                match timeout(timeout_dur, sock.recv(&mut recv_buf)).await {
                    Ok(Ok(n)) => {
                        let ack = Packet::from_bytes(&recv_buf[..n])?;
                        match ack {
                            Packet::ACK { block_num: bn } if bn == block_num => break,
                            Packet::ERROR { code, msg } => {
                                return Err(anyhow!("client error {code}: {msg}"));
                            }
                            _ => { /* duplicate / wrong block – resend */ }
                        }
                    }
                    Ok(Err(e)) => return Err(e.into()),
                    Err(_) => {
                        retries += 1;
                        if retries > max_retries {
                            return Err(anyhow!("timeout after {max_retries} retries"));
                        }
                    }
                }
            }

            transferred += payload.len() as u64;
            tx.send(ServerEvent::TransferProgress {
                id,
                transferred,
                total_bytes,
            })?;

            if is_last {
                break;
            }
            block_num = block_num.wrapping_add(1);
        }
    }

    tx.send(ServerEvent::TransferComplete(id))?;
    tx.send(ServerEvent::Log(format!(
        "{peer}: RRQ \"{filename}\" complete ({transferred} bytes transferred)"
    )))?;
    Ok(())
}

/// Read the next block from a file, applying netascii encoding if needed.
async fn read_next_block(
    file: &mut tokio::fs::File,
    buf: &mut [u8],
    blksize: usize,
    encoder: &mut Option<NetasciiEncoder>,
) -> Result<Vec<u8>> {
    if let Some(enc) = encoder {
        // If there's overflow from a previous encode, drain it first.
        if enc.has_overflow() {
            let data = enc.drain_overflow(blksize);
            if data.len() >= blksize {
                return Ok(data);
            }
            // Not enough overflow to fill a block — read more from file.
            let bytes_read = file.read(buf).await?;
            if bytes_read == 0 {
                return Ok(data);
            }
            let raw = &buf[..bytes_read];
            let mut encoded = enc.encode(raw, blksize - data.len());
            let mut combined = data;
            combined.append(&mut encoded);
            return Ok(combined);
        }

        let bytes_read = file.read(buf).await?;
        if bytes_read == 0 {
            return Ok(Vec::new());
        }
        let raw = &buf[..bytes_read];
        Ok(enc.encode(raw, blksize))
    } else {
        let bytes_read = file.read(buf).await?;
        Ok(buf[..bytes_read].to_vec())
    }
}

/// Check if an encoder has pending overflow data.
fn has_encoder_overflow(encoder: &Option<NetasciiEncoder>) -> bool {
    encoder.as_ref().is_some_and(|e| e.has_overflow())
}

/// Check if a block number is within the current window.
fn is_in_window(bn: u16, window: &[(u16, Vec<u8>)]) -> bool {
    window.iter().any(|(b, _)| *b == bn)
}

// ---------------------------------------------------------------------------
// WRQ handler  (client uploads a file to us)
// ---------------------------------------------------------------------------

async fn handle_wrq(
    ctx: TransferContext,
    filename: &str,
    mode: &str,
    options: &HashMap<String, String>,
) -> Result<()> {
    let TransferContext {
        id,
        peer,
        dir,
        tx,
        config,
    } = ctx;
    let dir = dir.as_path();
    let config = config.as_ref();
    let path = sanitize_path(dir, filename)?;

    // Overwrite protection.
    if !config.allow_overwrite && path.exists() {
        // Send error to client on a temporary socket.
        if let Ok(tmp) = UdpSocket::bind("0.0.0.0:0").await {
            let err = Packet::ERROR {
                code: 6,
                msg: "File already exists".into(),
            };
            let _ = tmp.send_to(&err.to_bytes(), peer).await;
        }
        return Err(anyhow!("file already exists: {}", path.display()));
    }

    let is_netascii = mode == "netascii";

    // Negotiate options (blksize, timeout, windowsize).
    let negotiated = negotiate_options(options, config);
    let blksize = negotiated.blksize;
    let timeout_dur = Duration::from_millis(negotiated.timeout_ms);
    let windowsize = negotiated.windowsize;
    let mut oack_options = negotiated.oack;

    // RFC 2349: For WRQ, echo back the client's tsize value.
    if let Some(tsize_val) = options.get("tsize") {
        oack_options.insert("tsize".to_string(), tsize_val.clone());
    }

    let mut detail_parts = Vec::new();
    if blksize != BLOCK_SIZE {
        detail_parts.push(format!("blksize={blksize}"));
    }
    if windowsize > 1 {
        detail_parts.push(format!("windowsize={windowsize}"));
    }
    if negotiated.timeout_ms != config.timeout_ms {
        detail_parts.push(format!("timeout={}ms", negotiated.timeout_ms));
    }
    if is_netascii {
        detail_parts.push("netascii".to_string());
    }
    let detail_str = if detail_parts.is_empty() {
        String::new()
    } else {
        format!(" [{}]", detail_parts.join(", "))
    };

    tx.send(ServerEvent::Log(format!(
        "{peer}: WRQ \"{filename}\"{detail_str}"
    )))?;

    // Try to determine expected size from tsize option.
    let expected_size = options
        .get("tsize")
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(0);

    tx.send(ServerEvent::TransferStarted(TransferInfo {
        id,
        peer,
        filename: filename.to_string(),
        kind: TransferKind::Upload,
        total_bytes: expected_size,
        transferred: 0,
        started: Instant::now(),
        size_known: expected_size > 0,
    }))?;

    let sock = bind_transfer_socket(peer, blksize).await?;
    let mut recv_buf = vec![0u8; MAX_PACKET];
    let max_retries = config.max_retries;

    // Send OACK if we have negotiated options, then wait for first DATA.
    if !oack_options.is_empty() {
        let oack_pkt = Packet::OACK {
            options: oack_options,
        };
        send_resilient(&sock, &oack_pkt.to_bytes()).await?;
    } else {
        // Send ACK 0 to acknowledge the WRQ.
        let ack0 = Packet::ACK { block_num: 0 };
        send_resilient(&sock, &ack0.to_bytes()).await?;
    }

    // Ensure parent directories exist for subdirectory uploads.
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }

    // Write to a temporary ".part" file so that incomplete uploads are
    // never mistaken for valid files.  On success we rename to the real
    // path; on failure the .part file is cleaned up.
    let part_path = {
        let mut p = path.as_os_str().to_owned();
        p.push(".part");
        PathBuf::from(p)
    };

    let mut file = if config.allow_overwrite {
        tokio::fs::File::create(&part_path).await?
    } else {
        tokio::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&part_path)
            .await
            .map_err(|e| {
                if e.kind() == std::io::ErrorKind::AlreadyExists {
                    anyhow!("file already exists: {}", path.display())
                } else {
                    anyhow!("cannot create {}: {e}", path.display())
                }
            })?
    };

    let mut transferred: u64 = 0;
    let mut expected_block: u16 = 1;
    let mut decoder = if is_netascii {
        Some(NetasciiDecoder::new())
    } else {
        None
    };

    if windowsize > 1 {
        // --- Windowed WRQ ---
        loop {
            let mut window_data: Vec<(u16, Vec<u8>)> = Vec::new();
            let mut last_block = false;
            let mut retries = 0u32;

            // Receive up to `windowsize` DATA blocks.
            loop {
                match timeout(timeout_dur, sock.recv(&mut recv_buf)).await {
                    Ok(Ok(n)) => {
                        let pkt = Packet::from_bytes(&recv_buf[..n])?;
                        match pkt {
                            Packet::DATA { block_num, data } if block_num == expected_block => {
                                let is_last = data.len() < blksize;
                                window_data.push((block_num, data));
                                expected_block = expected_block.wrapping_add(1);
                                if is_last {
                                    last_block = true;
                                    break;
                                }
                                if window_data.len() >= windowsize as usize {
                                    break;
                                }
                            }
                            Packet::DATA { block_num, .. }
                                if block_num == expected_block.wrapping_sub(1)
                                    && window_data.is_empty() =>
                            {
                                // Duplicate of previous block — re-ACK.
                                let ack = Packet::ACK { block_num };
                                send_resilient(&sock, &ack.to_bytes()).await?;
                            }
                            Packet::ERROR { code, msg } => {
                                return Err(anyhow!("client error {code}: {msg}"));
                            }
                            _ => { /* ignore unexpected */ }
                        }
                    }
                    Ok(Err(e)) => return Err(e.into()),
                    Err(_) => {
                        if !window_data.is_empty() {
                            // Got partial window, process what we have.
                            break;
                        }
                        retries += 1;
                        if retries > max_retries {
                            return Err(anyhow!(
                                "timeout waiting for DATA block {}",
                                expected_block
                            ));
                        }
                        // Re-send previous ACK.
                        let prev = Packet::ACK {
                            block_num: expected_block.wrapping_sub(1),
                        };
                        send_resilient(&sock, &prev.to_bytes()).await?;
                    }
                }
            }

            // Write all received data to disk.
            for (_, data) in &window_data {
                let to_write = if let Some(ref mut dec) = decoder {
                    dec.decode(data)
                } else {
                    data.clone()
                };
                file.write_all(&to_write).await?;
                transferred += to_write.len() as u64;
            }

            // ACK the last block we received.
            if let Some((last_bn, _)) = window_data.last() {
                let ack = Packet::ACK {
                    block_num: *last_bn,
                };
                send_resilient(&sock, &ack.to_bytes()).await?;
            }

            let report_total = if expected_size > 0 {
                expected_size
            } else {
                transferred
            };
            tx.send(ServerEvent::TransferProgress {
                id,
                transferred,
                total_bytes: report_total,
            })?;

            if last_block {
                break;
            }
        }
    } else {
        // --- Classic single-block WRQ ---
        loop {
            let mut retries = 0u32;
            let data_payload;

            loop {
                match timeout(timeout_dur, sock.recv(&mut recv_buf)).await {
                    Ok(Ok(n)) => {
                        let pkt = Packet::from_bytes(&recv_buf[..n])?;
                        match pkt {
                            Packet::DATA { block_num, data } if block_num == expected_block => {
                                data_payload = data;
                                break;
                            }
                            // Duplicate of previous block – re-ACK it.
                            Packet::DATA { block_num, .. }
                                if block_num == expected_block.wrapping_sub(1) =>
                            {
                                let ack = Packet::ACK { block_num };
                                send_resilient(&sock, &ack.to_bytes()).await?;
                            }
                            Packet::ERROR { code, msg } => {
                                return Err(anyhow!("client error {code}: {msg}"));
                            }
                            _ => { /* ignore unexpected */ }
                        }
                    }
                    Ok(Err(e)) => return Err(e.into()),
                    Err(_) => {
                        retries += 1;
                        if retries > max_retries {
                            return Err(anyhow!("timeout waiting for DATA block {expected_block}"));
                        }
                        // Re-send previous ACK.
                        let prev = Packet::ACK {
                            block_num: expected_block.wrapping_sub(1),
                        };
                        send_resilient(&sock, &prev.to_bytes()).await?;
                    }
                }
            }

            let is_last = data_payload.len() < blksize;

            // Apply netascii decoding if needed.
            let to_write = if let Some(ref mut dec) = decoder {
                dec.decode(&data_payload)
            } else {
                data_payload
            };

            // Write directly to disk.
            file.write_all(&to_write).await?;
            transferred += to_write.len() as u64;

            // ACK this block.
            let ack = Packet::ACK {
                block_num: expected_block,
            };
            send_resilient(&sock, &ack.to_bytes()).await?;

            let report_total = if expected_size > 0 {
                expected_size
            } else {
                transferred
            };
            tx.send(ServerEvent::TransferProgress {
                id,
                transferred,
                total_bytes: report_total,
            })?;

            if is_last {
                break;
            }
            expected_block = expected_block.wrapping_add(1);
        }
    }

    file.flush().await?;
    drop(file);

    // Atomically promote the completed .part file to its final name.
    tokio::fs::rename(&part_path, &path).await.map_err(|e| {
        anyhow!(
            "failed to rename {} -> {}: {e}",
            part_path.display(),
            path.display()
        )
    })?;

    tx.send(ServerEvent::TransferComplete(id))?;
    tx.send(ServerEvent::Log(format!(
        "{peer}: WRQ \"{filename}\" complete ({transferred} bytes)"
    )))?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Ensure the requested filename stays inside the served directory.
/// Supports subdirectory paths (e.g. `ios/config/router.cfg`) while
/// rejecting any traversal attempt (`..`) or absolute paths.
pub(crate) fn sanitize_path(dir: &Path, filename: &str) -> Result<PathBuf> {
    let normalized = filename.replace('\\', "/");

    // Reject absolute paths.
    if normalized.starts_with('/') {
        return Err(anyhow!("absolute paths are not allowed"));
    }

    // Reject any component that is `..`.
    for component in normalized.split('/') {
        if component == ".." {
            return Err(anyhow!("path traversal is not allowed"));
        }
    }

    // Filter out empty segments and `.` components.
    let clean: PathBuf = normalized
        .split('/')
        .filter(|c| !c.is_empty() && *c != ".")
        .collect();

    if clean.as_os_str().is_empty() {
        return Err(anyhow!("invalid filename"));
    }

    let candidate = dir.join(&clean);

    // For existing files, canonicalize and verify containment.
    // For new files (WRQ), verify the deepest existing ancestor is within dir.
    let canonical_dir = dir
        .canonicalize()
        .map_err(|e| anyhow!("cannot canonicalize served directory: {e}"))?;

    if candidate.exists() {
        let canonical = candidate
            .canonicalize()
            .map_err(|e| anyhow!("cannot canonicalize path: {e}"))?;
        if !canonical.starts_with(&canonical_dir) {
            return Err(anyhow!("path escapes served directory"));
        }
        Ok(canonical)
    } else {
        // Walk up until we find an existing ancestor.
        let mut ancestor = candidate.parent();
        while let Some(a) = ancestor {
            if a.exists() {
                let canonical_ancestor = a
                    .canonicalize()
                    .map_err(|e| anyhow!("cannot canonicalize ancestor: {e}"))?;
                if !canonical_ancestor.starts_with(&canonical_dir) {
                    return Err(anyhow!("path escapes served directory"));
                }
                return Ok(candidate);
            }
            ancestor = a.parent();
        }
        Err(anyhow!("path escapes served directory"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_simple_file() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("hello.txt"), b"test").unwrap();
        let result = sanitize_path(dir.path(), "hello.txt").unwrap();
        assert!(result.ends_with("hello.txt"));
    }

    #[test]
    fn sanitize_subdirectory() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("sub/deep")).unwrap();
        std::fs::write(dir.path().join("sub/deep/file.cfg"), b"data").unwrap();
        let result = sanitize_path(dir.path(), "sub/deep/file.cfg").unwrap();
        assert!(result.ends_with("sub/deep/file.cfg"));
    }

    #[test]
    fn sanitize_rejects_dotdot() {
        let dir = tempfile::tempdir().unwrap();
        assert!(sanitize_path(dir.path(), "../etc/passwd").is_err());
        assert!(sanitize_path(dir.path(), "sub/../../etc/passwd").is_err());
    }

    #[test]
    fn sanitize_rejects_absolute() {
        let dir = tempfile::tempdir().unwrap();
        assert!(sanitize_path(dir.path(), "/etc/passwd").is_err());
    }

    #[test]
    fn sanitize_normalizes_backslashes() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("ios")).unwrap();
        std::fs::write(dir.path().join("ios/config.cfg"), b"data").unwrap();
        let result = sanitize_path(dir.path(), "ios\\config.cfg").unwrap();
        assert!(result.ends_with("ios/config.cfg"));
    }

    #[test]
    fn sanitize_nonexistent_path_within_dir() {
        let dir = tempfile::tempdir().unwrap();
        // New file in a non-existent subdirectory (for WRQ).
        let result = sanitize_path(dir.path(), "new_dir/file.bin").unwrap();
        assert!(result.ends_with("new_dir/file.bin"));
    }

    #[test]
    fn sanitize_rejects_empty() {
        let dir = tempfile::tempdir().unwrap();
        assert!(sanitize_path(dir.path(), "").is_err());
        assert!(sanitize_path(dir.path(), ".").is_err());
        assert!(sanitize_path(dir.path(), "..").is_err());
    }
}
