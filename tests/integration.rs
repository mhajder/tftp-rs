//! End-to-end transfers against the real server: whole downloads and uploads,
//! option negotiation, block-number rollover, and what happens when a client
//! goes quiet, sends an error, or asks for something the server refuses.

mod common;

use std::net::SocketAddr;
use std::path::Path;

use common::{
    ack, block, data, expect_ack, expect_log, next_event, oack_options, opcode, recv, rrq,
    rrq_with_options, start, wrq, wrq_with_options,
};
use tftp_rs::server::{ServerConfig, ServerEvent, TransferKind};
use tokio::net::UdpSocket;
use tokio::sync::mpsc::UnboundedReceiver;

async fn client() -> UdpSocket {
    UdpSocket::bind("127.0.0.1:0").await.expect("client socket")
}

/// Download a file in classic lock-step, ACKing every block, and return its
/// contents. With `negotiated`, the first reply must be an OACK, which is
/// acknowledged with ACK 0.
async fn download(
    client: &UdpSocket,
    server: SocketAddr,
    request: &[u8],
    blksize: usize,
    negotiated: bool,
) -> Vec<u8> {
    client.send_to(request, server).await.expect("RRQ");
    if negotiated {
        let (packet, from) = recv(client).await;
        assert_eq!(opcode(&packet), 6, "expected an OACK");
        client.send_to(&ack(0), from).await.expect("ACK 0");
    }
    let mut received = Vec::new();
    let mut expected: u16 = 1;
    loop {
        let (packet, from) = recv(client).await;
        assert_eq!(opcode(&packet), 3, "expected DATA");
        assert_eq!(block(&packet), expected, "block number");
        let payload = &packet[4..];
        received.extend_from_slice(payload);
        client.send_to(&ack(expected), from).await.expect("ACK");
        if payload.len() < blksize {
            return received;
        }
        expected = expected.wrapping_add(1);
    }
}

/// Upload `content` in blocks of `blksize` to a transfer that has already
/// been accepted at `transfer`, waiting for each ACK.
async fn upload(client: &UdpSocket, transfer: SocketAddr, content: &[u8], blksize: usize) {
    let mut block_num: u16 = 1;
    let mut offset = 0;
    loop {
        let end = (offset + blksize).min(content.len());
        client
            .send_to(&data(block_num, &content[offset..end]), transfer)
            .await
            .expect("DATA");
        expect_ack(client, block_num).await;
        if end - offset < blksize {
            return;
        }
        offset = end;
        block_num = block_num.wrapping_add(1);
    }
}

/// The id of the next transfer to start.
async fn started(events: &mut UnboundedReceiver<ServerEvent>) -> u64 {
    match next_event(events, |e| matches!(e, ServerEvent::TransferStarted(_))).await {
        ServerEvent::TransferStarted(info) => info.id,
        _ => unreachable!(),
    }
}

/// Wait for transfer `id` to complete.
async fn completed(events: &mut UnboundedReceiver<ServerEvent>, id: u64) {
    next_event(
        events,
        |e| matches!(e, ServerEvent::TransferComplete(done) if *done == id),
    )
    .await;
}

fn staging_files(dir: &Path) -> Vec<String> {
    std::fs::read_dir(dir)
        .expect("served directory")
        .map(|entry| {
            entry
                .expect("entry")
                .file_name()
                .to_string_lossy()
                .into_owned()
        })
        .filter(|name| name.ends_with(".part"))
        .collect()
}

// ---------------------------------------------------------------------------
// Whole transfers
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_download_runs_to_completion() {
    let dir = tempfile::tempdir().expect("temporary directory");
    let content = b"Hello, TFTP world! This is a download test.";
    tokio::fs::write(dir.path().join("test.txt"), content)
        .await
        .expect("test file");
    let (server, shutdown, _task, mut events) = start(dir.path(), ServerConfig::default()).await;

    let client = client().await;
    let received = download(&client, server, &rrq("test.txt"), 512, false).await;
    assert_eq!(received, content);

    let id = match next_event(&mut events, |e| {
        matches!(e, ServerEvent::TransferStarted(_))
    })
    .await
    {
        ServerEvent::TransferStarted(info) => {
            assert_eq!(info.kind, TransferKind::Download);
            assert_eq!(info.filename, "test.txt");
            assert_eq!(info.total_bytes, content.len() as u64);
            info.id
        }
        _ => unreachable!(),
    };
    completed(&mut events, id).await;
    expect_log(&mut events, "complete").await;

    let _ = shutdown.send(true);
}

#[tokio::test]
async fn an_upload_runs_to_completion() {
    let dir = tempfile::tempdir().expect("temporary directory");
    let (server, shutdown, _task, mut events) = start(dir.path(), ServerConfig::default()).await;
    let content = b"This file was uploaded via TFTP WRQ.";

    let client = client().await;
    client
        .send_to(&wrq("uploaded.txt"), server)
        .await
        .expect("WRQ");
    let (packet, transfer) = recv(&client).await;
    assert_eq!((opcode(&packet), block(&packet)), (4, 0), "expected ACK 0");
    upload(&client, transfer, content, 512).await;

    let id = started(&mut events).await;
    completed(&mut events, id).await;
    let written = tokio::fs::read(dir.path().join("uploaded.txt"))
        .await
        .expect("uploaded file");
    assert_eq!(written, content);
    assert!(staging_files(dir.path()).is_empty());

    let _ = shutdown.send(true);
}

#[tokio::test]
async fn a_download_from_a_subdirectory() {
    let dir = tempfile::tempdir().expect("temporary directory");
    let sub = dir.path().join("configs/switches");
    tokio::fs::create_dir_all(&sub).await.expect("subdirectory");
    let content = b"switch config data here";
    tokio::fs::write(sub.join("sw1.cfg"), content)
        .await
        .expect("test file");
    let (server, shutdown, _task, _events) = start(dir.path(), ServerConfig::default()).await;

    let client = client().await;
    let received = download(
        &client,
        server,
        &rrq("configs/switches/sw1.cfg"),
        512,
        false,
    )
    .await;
    assert_eq!(received, content);

    let _ = shutdown.send(true);
}

#[tokio::test]
async fn an_upload_creates_missing_subdirectories() {
    let dir = tempfile::tempdir().expect("temporary directory");
    let (server, shutdown, _task, mut events) = start(dir.path(), ServerConfig::default()).await;
    let content = b"uploaded into a new subdirectory";

    let client = client().await;
    client
        .send_to(&wrq("new_dir/sub/uploaded.txt"), server)
        .await
        .expect("WRQ");
    let (packet, transfer) = recv(&client).await;
    assert_eq!((opcode(&packet), block(&packet)), (4, 0), "expected ACK 0");
    upload(&client, transfer, content, 512).await;

    let id = started(&mut events).await;
    completed(&mut events, id).await;
    let written = tokio::fs::read(dir.path().join("new_dir/sub/uploaded.txt"))
        .await
        .expect("uploaded file");
    assert_eq!(written, content);

    let _ = shutdown.send(true);
}

// ---------------------------------------------------------------------------
// Option negotiation
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_download_with_a_negotiated_blksize() {
    let dir = tempfile::tempdir().expect("temporary directory");
    let content: Vec<u8> = (0..2048u32).map(|i| (i % 256) as u8).collect();
    tokio::fs::write(dir.path().join("large.bin"), &content)
        .await
        .expect("test file");
    let (server, shutdown, _task, _events) = start(dir.path(), ServerConfig::default()).await;

    let client = client().await;
    client
        .send_to(
            &rrq_with_options("large.bin", &[("blksize", "1024")]),
            server,
        )
        .await
        .expect("RRQ");
    let (packet, transfer) = recv(&client).await;
    assert_eq!(opcode(&packet), 6, "expected an OACK");
    assert_eq!(oack_options(&packet)["blksize"], "1024");
    client.send_to(&ack(0), transfer).await.expect("ACK 0");

    let mut received = Vec::new();
    for expected in 1..=3u16 {
        let (packet, _) = recv(&client).await;
        assert_eq!((opcode(&packet), block(&packet)), (3, expected));
        received.extend_from_slice(&packet[4..]);
        client.send_to(&ack(expected), transfer).await.expect("ACK");
    }
    // 2048 bytes in 1024-byte blocks: two full blocks and an empty third.
    assert_eq!(received, content);

    let _ = shutdown.send(true);
}

#[tokio::test]
async fn a_download_reports_its_tsize() {
    let dir = tempfile::tempdir().expect("temporary directory");
    let content = b"Transfer size test data - checking tsize option";
    tokio::fs::write(dir.path().join("tsize.txt"), content)
        .await
        .expect("test file");
    let (server, shutdown, _task, _events) = start(dir.path(), ServerConfig::default()).await;

    let client = client().await;
    client
        .send_to(&rrq_with_options("tsize.txt", &[("tsize", "0")]), server)
        .await
        .expect("RRQ");
    let (packet, transfer) = recv(&client).await;
    assert_eq!(opcode(&packet), 6, "expected an OACK");
    assert_eq!(oack_options(&packet)["tsize"], content.len().to_string());
    client.send_to(&ack(0), transfer).await.expect("ACK 0");

    let (packet, _) = recv(&client).await;
    assert_eq!((opcode(&packet), block(&packet)), (3, 1));
    assert_eq!(&packet[4..], content);
    client.send_to(&ack(1), transfer).await.expect("ACK 1");

    let _ = shutdown.send(true);
}

#[tokio::test]
async fn an_upload_with_a_negotiated_blksize() {
    let dir = tempfile::tempdir().expect("temporary directory");
    let (server, shutdown, _task, mut events) = start(dir.path(), ServerConfig::default()).await;
    let content: Vec<u8> = (0..2048u32).map(|i| (i % 256) as u8).collect();

    let client = client().await;
    client
        .send_to(
            &wrq_with_options("upload_large.bin", &[("blksize", "1024")]),
            server,
        )
        .await
        .expect("WRQ");
    let (packet, transfer) = recv(&client).await;
    assert_eq!(opcode(&packet), 6, "expected an OACK");
    assert_eq!(oack_options(&packet)["blksize"], "1024");
    // Two full blocks, then the empty block that ends an exact multiple.
    upload(&client, transfer, &content, 1024).await;

    let id = started(&mut events).await;
    completed(&mut events, id).await;
    let written = tokio::fs::read(dir.path().join("upload_large.bin"))
        .await
        .expect("uploaded file");
    assert_eq!(written, content);

    let _ = shutdown.send(true);
}

#[tokio::test]
async fn block_numbers_roll_over_past_65535() {
    let dir = tempfile::tempdir().expect("temporary directory");
    let blksize = 8usize;
    let blocks: usize = 65_537;
    let content: Vec<u8> = (0..blksize * blocks).map(|i| (i % 251) as u8).collect();
    tokio::fs::write(dir.path().join("rollover.bin"), &content)
        .await
        .expect("test file");
    let (server, shutdown, _task, _events) = start(dir.path(), ServerConfig::default()).await;

    let client = client().await;
    let received = download(
        &client,
        server,
        &rrq_with_options("rollover.bin", &[("blksize", "8")]),
        blksize,
        true,
    )
    .await;
    assert_eq!(received.len(), content.len());
    assert_eq!(received, content);

    let _ = shutdown.send(true);
}
