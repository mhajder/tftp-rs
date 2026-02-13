use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::Instant;

use anyhow::{Result, anyhow};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UdpSocket;
use tokio::sync::mpsc;
use tokio::time::{Duration, timeout};

use crate::tftp_protocol::{BLOCK_SIZE, MAX_BLKSIZE, Packet};

/// Maximum UDP datagram size we ever expect (4-byte header + max blksize).
const MAX_PACKET: usize = 4 + MAX_BLKSIZE;

/// How long to wait for an ACK / DATA before retransmitting.
const TIMEOUT: Duration = Duration::from_millis(500);

/// Maximum retransmission attempts before giving up.
const MAX_RETRIES: u32 = 10;

/// The largest TFTP blksize the OS will allow in a single UDP send.
/// Detected once at startup by probing the kernel.
static MAX_SENDABLE_BLKSIZE: OnceLock<usize> = OnceLock::new();

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

    // Need room for the 4-byte TFTP header plus the payload.
    let buf_size = (4 + blksize) * 2;
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

/// Negotiate blksize from client options. Returns (negotiated_blksize, oack_options).
/// If the client didn't request blksize, returns default 512 with empty options.
/// The negotiated blksize is capped at the OS maximum UDP datagram payload.
fn negotiate_options(client_options: &HashMap<String, String>) -> (usize, HashMap<String, String>) {
    let mut acked = HashMap::new();
    let mut blksize = BLOCK_SIZE;
    let os_max = max_blksize();

    if let Some(val) = client_options.get("blksize")
        && let Ok(requested) = val.parse::<usize>()
        && (8..=MAX_BLKSIZE).contains(&requested)
    {
        blksize = requested.min(os_max);
        acked.insert("blksize".to_string(), blksize.to_string());
    }

    // tsize option: report file size for downloads (set by caller).
    if client_options.contains_key("tsize") {
        // Signal that we should report tsize; the caller fills in the value.
        acked.insert("tsize".to_string(), "0".to_string());
    }

    (blksize, acked)
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
) -> Result<()> {
    let addr: SocketAddr = format!("0.0.0.0:{port}").parse()?;
    let sock = UdpSocket::bind(addr).await?;
    tx.send(ServerEvent::Log(format!("Listening on {addr}")))?;

    let detected_blksize = max_blksize();
    tx.send(ServerEvent::Log(format!(
        "Max negotiable blksize: {detected_blksize}"
    )))?;

    let dir = Arc::new(dir);
    let mut buf = vec![0u8; MAX_PACKET];
    let mut next_id: u64 = 1;

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
                        let id = next_id;
                        next_id += 1;
                        let tx2 = tx.clone();
                        let dir2 = Arc::clone(&dir);
                        tokio::spawn(async move {
                            if let Err(e) = handle_rrq(id, peer, &filename, &mode, &options, &dir2, tx2.clone()).await {
                                let _ = tx2.send(ServerEvent::TransferFailed { id, error: e.to_string() });
                                let _ = tx2.send(ServerEvent::Log(format!("{peer}: RRQ error: {e}")));
                            }
                        });
                    }
                    Packet::WRQ { filename, mode, options } => {
                        let id = next_id;
                        next_id += 1;
                        let tx2 = tx.clone();
                        let dir2 = Arc::clone(&dir);
                        tokio::spawn(async move {
                            if let Err(e) = handle_wrq(id, peer, &filename, &mode, &options, &dir2, tx2.clone()).await {
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
    id: u64,
    peer: SocketAddr,
    filename: &str,
    _mode: &str,
    options: &HashMap<String, String>,
    dir: &Path,
    tx: mpsc::UnboundedSender<ServerEvent>,
) -> Result<()> {
    let path = sanitize_path(dir, filename)?;
    let metadata = tokio::fs::metadata(&path)
        .await
        .map_err(|e| anyhow!("cannot read {}: {e}", path.display()))?;
    let total_bytes = metadata.len();

    // Negotiate options (blksize, tsize).
    let (blksize, mut oack_options) = negotiate_options(options);

    // Fill in tsize if the client requested it.
    if oack_options.contains_key("tsize") {
        oack_options.insert("tsize".to_string(), total_bytes.to_string());
    }

    let blksize_str = if blksize != BLOCK_SIZE {
        format!(" blksize={blksize}")
    } else {
        String::new()
    };
    tx.send(ServerEvent::Log(format!(
        "{peer}: RRQ \"{filename}\" ({total_bytes} bytes){blksize_str}"
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

    // Send OACK if we have negotiated options, then wait for ACK 0.
    if !oack_options.is_empty() {
        let oack_pkt = Packet::OACK {
            options: oack_options,
        };
        let oack_bytes = oack_pkt.to_bytes();

        let mut retries = 0u32;
        loop {
            sock.send(&oack_bytes).await?;
            match timeout(TIMEOUT, sock.recv(&mut recv_buf)).await {
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
                    if retries > MAX_RETRIES {
                        return Err(anyhow!("timeout waiting for OACK acknowledgment"));
                    }
                }
            }
        }
    }

    // Stream the file block-by-block instead of loading it all into memory.
    let mut file = tokio::fs::File::open(&path)
        .await
        .map_err(|e| anyhow!("cannot open {}: {e}", path.display()))?;
    let mut block_buf = vec![0u8; blksize];
    let mut block_num: u16 = 1;
    let mut transferred: u64 = 0;

    loop {
        let bytes_read = file.read(&mut block_buf).await?;
        let payload = &block_buf[..bytes_read];

        // Build DATA packet bytes directly to avoid extra Vec allocation.
        let mut pkt_bytes = Vec::with_capacity(4 + bytes_read);
        pkt_bytes.extend_from_slice(&3u16.to_be_bytes()); // OPCODE_DATA
        pkt_bytes.extend_from_slice(&block_num.to_be_bytes());
        pkt_bytes.extend_from_slice(payload);

        let mut retries = 0u32;
        loop {
            sock.send(&pkt_bytes).await?;
            match timeout(TIMEOUT, sock.recv(&mut recv_buf)).await {
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
                    if retries > MAX_RETRIES {
                        return Err(anyhow!("timeout after {MAX_RETRIES} retries"));
                    }
                }
            }
        }

        transferred += bytes_read as u64;
        tx.send(ServerEvent::TransferProgress {
            id,
            transferred,
            total_bytes,
        })?;

        // A block shorter than blksize signals end-of-transfer.
        if bytes_read < blksize {
            break;
        }
        block_num = block_num.wrapping_add(1);
    }

    tx.send(ServerEvent::TransferComplete(id))?;
    tx.send(ServerEvent::Log(format!(
        "{peer}: RRQ \"{filename}\" complete"
    )))?;
    Ok(())
}

// ---------------------------------------------------------------------------
// WRQ handler  (client uploads a file to us)
// ---------------------------------------------------------------------------

async fn handle_wrq(
    id: u64,
    peer: SocketAddr,
    filename: &str,
    _mode: &str,
    options: &HashMap<String, String>,
    dir: &Path,
    tx: mpsc::UnboundedSender<ServerEvent>,
) -> Result<()> {
    let path = sanitize_path(dir, filename)?;

    // Negotiate options (blksize).
    let (blksize, oack_options) = negotiate_options(options);

    let blksize_str = if blksize != BLOCK_SIZE {
        format!(" blksize={blksize}")
    } else {
        String::new()
    };
    tx.send(ServerEvent::Log(format!(
        "{peer}: WRQ \"{filename}\"{blksize_str}"
    )))?;
    tx.send(ServerEvent::TransferStarted(TransferInfo {
        id,
        peer,
        filename: filename.to_string(),
        kind: TransferKind::Upload,
        total_bytes: 0,
        transferred: 0,
        started: Instant::now(),
        size_known: false,
    }))?;

    let sock = bind_transfer_socket(peer, blksize).await?;
    let mut recv_buf = vec![0u8; MAX_PACKET];

    // Send OACK if we have negotiated options, then wait for first DATA.
    if !oack_options.is_empty() {
        let oack_pkt = Packet::OACK {
            options: oack_options,
        };
        sock.send(&oack_pkt.to_bytes()).await?;
    } else {
        // Send ACK 0 to acknowledge the WRQ.
        let ack0 = Packet::ACK { block_num: 0 };
        sock.send(&ack0.to_bytes()).await?;
    }

    // Ensure parent directories exist for subdirectory uploads.
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }

    // Stream directly to disk instead of accumulating in memory.
    let mut file = tokio::fs::File::create(&path).await?;
    let mut transferred: u64 = 0;
    let mut expected_block: u16 = 1;

    loop {
        let mut retries = 0u32;
        let data_payload;

        loop {
            match timeout(TIMEOUT, sock.recv(&mut recv_buf)).await {
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
                            sock.send(&ack.to_bytes()).await?;
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
                    if retries > MAX_RETRIES {
                        return Err(anyhow!("timeout waiting for DATA block {expected_block}"));
                    }
                    // Re-send previous ACK.
                    let prev = Packet::ACK {
                        block_num: expected_block.wrapping_sub(1),
                    };
                    sock.send(&prev.to_bytes()).await?;
                }
            }
        }

        let is_last = data_payload.len() < blksize;

        // Write directly to disk.
        file.write_all(&data_payload).await?;
        transferred += data_payload.len() as u64;

        // ACK this block.
        let ack = Packet::ACK {
            block_num: expected_block,
        };
        sock.send(&ack.to_bytes()).await?;

        tx.send(ServerEvent::TransferProgress {
            id,
            transferred,
            total_bytes: transferred, // grows with upload
        })?;

        if is_last {
            break;
        }
        expected_block = expected_block.wrapping_add(1);
    }

    file.flush().await?;

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
