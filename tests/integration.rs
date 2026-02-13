use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use tokio::net::UdpSocket;
use tokio::sync::watch;

// minimal protocol helpers needed for the test inline.

const BLOCK_SIZE: usize = 512;

fn build_rrq(filename: &str) -> Vec<u8> {
    let mut buf = Vec::new();
    buf.extend_from_slice(&1u16.to_be_bytes()); // opcode RRQ
    buf.extend_from_slice(filename.as_bytes());
    buf.push(0);
    buf.extend_from_slice(b"octet");
    buf.push(0);
    buf
}

fn build_wrq(filename: &str) -> Vec<u8> {
    let mut buf = Vec::new();
    buf.extend_from_slice(&2u16.to_be_bytes()); // opcode WRQ
    buf.extend_from_slice(filename.as_bytes());
    buf.push(0);
    buf.extend_from_slice(b"octet");
    buf.push(0);
    buf
}

fn build_ack(block: u16) -> Vec<u8> {
    let mut buf = Vec::new();
    buf.extend_from_slice(&4u16.to_be_bytes());
    buf.extend_from_slice(&block.to_be_bytes());
    buf
}

fn build_data(block: u16, data: &[u8]) -> Vec<u8> {
    let mut buf = Vec::new();
    buf.extend_from_slice(&3u16.to_be_bytes());
    buf.extend_from_slice(&block.to_be_bytes());
    buf.extend_from_slice(data);
    buf
}

fn parse_opcode(buf: &[u8]) -> u16 {
    u16::from_be_bytes([buf[0], buf[1]])
}

fn parse_block(buf: &[u8]) -> u16 {
    u16::from_be_bytes([buf[2], buf[3]])
}

/// Start the server on an OS-assigned port and return the address.
async fn start_server(dir: PathBuf) -> (SocketAddr, watch::Sender<bool>) {
    let (shutdown_tx, shutdown_rx) = watch::channel(false);

    // Bind to port 0 so the OS picks a free port.
    let sock = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let addr = sock.local_addr().unwrap();
    drop(sock);

    let dir2 = dir.clone();
    let mut shutdown_rx2 = shutdown_rx.clone();
    tokio::spawn(async move {
        let sock = UdpSocket::bind(addr).await.unwrap();
        let mut buf = vec![0u8; 4 + BLOCK_SIZE];

        loop {
            tokio::select! {
                result = sock.recv_from(&mut buf) => {
                    let (n, peer) = result.unwrap();
                    let opcode = u16::from_be_bytes([buf[0], buf[1]]);
                    let payload = &buf[2..n];

                    // Parse filename and mode from RRQ/WRQ.
                    let mut parts = payload.splitn(3, |&b| b == 0);
                    let filename = String::from_utf8(parts.next().unwrap().to_vec()).unwrap();
                    let _mode = String::from_utf8(parts.next().unwrap().to_vec()).unwrap();

                    let dir3 = dir2.clone();

                    match opcode {
                        1 => {
                            tokio::spawn(async move {
                                serve_rrq(peer, &filename, &dir3).await;
                            });
                        }
                        2 => {
                            tokio::spawn(async move {
                                serve_wrq(peer, &filename, &dir3).await;
                            });
                        }
                        _ => {}
                    }
                }
                _ = shutdown_rx2.changed() => break,
            }
        }
    });

    // Give the server a moment to bind.
    tokio::time::sleep(Duration::from_millis(50)).await;

    (addr, shutdown_tx)
}

async fn serve_rrq(peer: SocketAddr, filename: &str, dir: &Path) {
    let path = dir.join(filename);
    let data = tokio::fs::read(&path).await.unwrap();
    let sock = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    sock.connect(peer).await.unwrap();

    let chunks: Vec<&[u8]> = data.chunks(BLOCK_SIZE).collect();
    let needs_empty = !data.is_empty() && data.len() % BLOCK_SIZE == 0;
    let total = chunks.len().max(1) + if needs_empty { 1 } else { 0 };

    let mut recv_buf = vec![0u8; 516];
    for seq in 0..total {
        let block_num = (seq + 1) as u16;
        let payload: &[u8] = if seq < chunks.len() { chunks[seq] } else { &[] };
        let pkt = build_data(block_num, payload);

        sock.send(&pkt).await.unwrap();
        let n = sock.recv(&mut recv_buf).await.unwrap();
        assert_eq!(parse_opcode(&recv_buf[..n]), 4); // ACK
        assert_eq!(parse_block(&recv_buf[..n]), block_num);
    }
}

async fn serve_wrq(peer: SocketAddr, filename: &str, dir: &Path) {
    let path = dir.join(filename);
    let sock = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    sock.connect(peer).await.unwrap();

    // ACK 0
    sock.send(&build_ack(0)).await.unwrap();

    let mut file_data = Vec::new();
    let mut expected: u16 = 1;
    let mut recv_buf = vec![0u8; 516];

    loop {
        let n = sock.recv(&mut recv_buf).await.unwrap();
        assert_eq!(parse_opcode(&recv_buf[..n]), 3); // DATA
        let block = parse_block(&recv_buf[..n]);
        assert_eq!(block, expected);
        let payload = &recv_buf[4..n];
        file_data.extend_from_slice(payload);
        sock.send(&build_ack(block)).await.unwrap();
        if payload.len() < BLOCK_SIZE {
            break;
        }
        expected = expected.wrapping_add(1);
    }

    // Ensure parent directories exist for subdirectory uploads.
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await.unwrap();
    }

    tokio::fs::write(&path, &file_data).await.unwrap();
}

#[tokio::test]
async fn test_rrq_download() {
    let dir = tempfile::tempdir().unwrap();
    let test_content = b"Hello, TFTP world! This is a download test.";
    tokio::fs::write(dir.path().join("test.txt"), test_content)
        .await
        .unwrap();

    let (server_addr, shutdown) = start_server(dir.path().to_path_buf()).await;

    // Act as a TFTP client: send RRQ and receive DATA.
    let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    client
        .send_to(&build_rrq("test.txt"), server_addr)
        .await
        .unwrap();

    let mut received = Vec::new();
    let mut recv_buf = vec![0u8; 516];

    loop {
        let (n, from) =
            tokio::time::timeout(Duration::from_secs(5), client.recv_from(&mut recv_buf))
                .await
                .unwrap()
                .unwrap();

        assert_eq!(parse_opcode(&recv_buf[..n]), 3); // DATA
        let block = parse_block(&recv_buf[..n]);
        let payload = &recv_buf[4..n];
        received.extend_from_slice(payload);

        // Send ACK.
        client.send_to(&build_ack(block), from).await.unwrap();

        if payload.len() < BLOCK_SIZE {
            break;
        }
    }

    assert_eq!(received, test_content);

    let _ = shutdown.send(true);
}

#[tokio::test]
async fn test_wrq_upload() {
    let dir = tempfile::tempdir().unwrap();
    let (server_addr, shutdown) = start_server(dir.path().to_path_buf()).await;

    let upload_content = b"This file was uploaded via TFTP WRQ.";

    let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    client
        .send_to(&build_wrq("uploaded.txt"), server_addr)
        .await
        .unwrap();

    let mut recv_buf = vec![0u8; 516];

    // Expect ACK 0.
    let (n, from) = tokio::time::timeout(Duration::from_secs(5), client.recv_from(&mut recv_buf))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(parse_opcode(&recv_buf[..n]), 4);
    assert_eq!(parse_block(&recv_buf[..n]), 0);

    // Send DATA block 1 (final, < 512 bytes).
    client
        .send_to(&build_data(1, upload_content), from)
        .await
        .unwrap();

    // Expect ACK 1.
    let (n, _) = tokio::time::timeout(Duration::from_secs(5), client.recv_from(&mut recv_buf))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(parse_opcode(&recv_buf[..n]), 4);
    assert_eq!(parse_block(&recv_buf[..n]), 1);

    // Verify file was written.
    tokio::time::sleep(Duration::from_millis(100)).await;
    let written = tokio::fs::read(dir.path().join("uploaded.txt"))
        .await
        .unwrap();
    assert_eq!(written, upload_content);

    let _ = shutdown.send(true);
}

#[tokio::test]
async fn test_rrq_subdirectory() {
    let dir = tempfile::tempdir().unwrap();

    // Create a file inside a subdirectory.
    let sub = dir.path().join("configs/switches");
    tokio::fs::create_dir_all(&sub).await.unwrap();
    let test_content = b"switch config data here";
    tokio::fs::write(sub.join("sw1.cfg"), test_content)
        .await
        .unwrap();

    let (server_addr, shutdown) = start_server(dir.path().to_path_buf()).await;

    let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    client
        .send_to(&build_rrq("configs/switches/sw1.cfg"), server_addr)
        .await
        .unwrap();

    let mut received = Vec::new();
    let mut recv_buf = vec![0u8; 516];

    loop {
        let (n, from) =
            tokio::time::timeout(Duration::from_secs(5), client.recv_from(&mut recv_buf))
                .await
                .unwrap()
                .unwrap();

        assert_eq!(parse_opcode(&recv_buf[..n]), 3);
        let block = parse_block(&recv_buf[..n]);
        let payload = &recv_buf[4..n];
        received.extend_from_slice(payload);

        client.send_to(&build_ack(block), from).await.unwrap();

        if payload.len() < BLOCK_SIZE {
            break;
        }
    }

    assert_eq!(received, test_content);

    let _ = shutdown.send(true);
}

#[tokio::test]
async fn test_wrq_creates_subdirectory() {
    let dir = tempfile::tempdir().unwrap();
    let (server_addr, shutdown) = start_server(dir.path().to_path_buf()).await;

    let upload_content = b"uploaded into a new subdirectory";

    let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    client
        .send_to(&build_wrq("new_dir/sub/uploaded.txt"), server_addr)
        .await
        .unwrap();

    let mut recv_buf = vec![0u8; 516];

    // Expect ACK 0.
    let (n, from) = tokio::time::timeout(Duration::from_secs(5), client.recv_from(&mut recv_buf))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(parse_opcode(&recv_buf[..n]), 4);
    assert_eq!(parse_block(&recv_buf[..n]), 0);

    // Send DATA block 1 (final, < 512 bytes).
    client
        .send_to(&build_data(1, upload_content), from)
        .await
        .unwrap();

    // Expect ACK 1.
    let (n, _) = tokio::time::timeout(Duration::from_secs(5), client.recv_from(&mut recv_buf))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(parse_opcode(&recv_buf[..n]), 4);
    assert_eq!(parse_block(&recv_buf[..n]), 1);

    // Verify file was written in the subdirectory.
    tokio::time::sleep(Duration::from_millis(100)).await;
    let written = tokio::fs::read(dir.path().join("new_dir/sub/uploaded.txt"))
        .await
        .unwrap();
    assert_eq!(written, upload_content);

    let _ = shutdown.send(true);
}
