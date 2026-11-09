//! End-to-end transfers against the real server: whole downloads and uploads,
//! option negotiation, block-number rollover, and what happens when a client
//! goes quiet, sends an error, or asks for something the server refuses.

mod common;

use std::net::SocketAddr;
use std::path::Path;
use std::time::Duration;

use common::{
    ack, block, data, error, expect_ack, expect_log, next_event, nothing_arrives, oack_options,
    opcode, receive_blocks, recv, request, rrq, rrq_with_options, start, wrq, wrq_with_options,
};
use tftp_rs::server::{ServerConfig, ServerEvent, TransferKind};
use tokio::net::UdpSocket;
use tokio::sync::mpsc::UnboundedReceiver;

/// Settings that make a silent client time out within a fraction of a
/// second: 50 ms per wait, two resends.
fn impatient() -> ServerConfig {
    ServerConfig {
        timeout_ms: 50,
        max_retries: 2,
        ..ServerConfig::default()
    }
}

/// Settings that give up after two retries, at the default timeout: for tests
/// that count retries but must not depend on how fast the test runs.
fn few_retries() -> ServerConfig {
    ServerConfig {
        max_retries: 2,
        ..ServerConfig::default()
    }
}

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

/// Wait for transfer `id` to fail, and return the error.
async fn failed(events: &mut UnboundedReceiver<ServerEvent>, id: u64) -> String {
    match next_event(
        events,
        |e| matches!(e, ServerEvent::TransferFailed { id: failed, .. } if *failed == id),
    )
    .await
    {
        ServerEvent::TransferFailed { error, .. } => error,
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

// ---------------------------------------------------------------------------
// Retries and client errors
// ---------------------------------------------------------------------------

#[tokio::test]
async fn an_unacknowledged_block_is_resent_then_the_download_fails() {
    let dir = tempfile::tempdir().expect("temporary directory");
    tokio::fs::write(dir.path().join("f.bin"), b"AROS")
        .await
        .expect("test file");
    let (server, shutdown, _task, mut events) = start(dir.path(), impatient()).await;

    let client = client().await;
    client.send_to(&rrq("f.bin"), server).await.expect("RRQ");
    let id = started(&mut events).await;
    for _ in 0..3 {
        // The block, then one resend per retry. Never acknowledged.
        let (packet, _) = recv(&client).await;
        assert_eq!((opcode(&packet), block(&packet)), (3, 1));
    }

    let error = failed(&mut events, id).await;
    assert!(
        error.contains("no acknowledgment for block 1 after 2 retries"),
        "{error}"
    );

    let _ = shutdown.send(true);
}

#[tokio::test]
async fn a_silent_uploader_is_reacknowledged_then_the_upload_fails() {
    let dir = tempfile::tempdir().expect("temporary directory");
    let (server, shutdown, _task, mut events) = start(dir.path(), impatient()).await;

    let client = client().await;
    client
        .send_to(&wrq("quiet.bin"), server)
        .await
        .expect("WRQ");
    let id = started(&mut events).await;
    for _ in 0..3 {
        // ACK 0, then one repeat per retry. No DATA ever follows.
        let (packet, _) = recv(&client).await;
        assert_eq!((opcode(&packet), block(&packet)), (4, 0));
    }

    let error = failed(&mut events, id).await;
    assert!(
        error.contains("timeout waiting for DATA block 1"),
        "{error}"
    );
    assert!(staging_files(dir.path()).is_empty());

    let _ = shutdown.send(true);
}

#[tokio::test]
async fn a_client_error_ends_a_download() {
    let dir = tempfile::tempdir().expect("temporary directory");
    tokio::fs::write(dir.path().join("f.bin"), vec![7_u8; 2000])
        .await
        .expect("test file");
    let (server, shutdown, _task, mut events) = start(dir.path(), few_retries()).await;

    let client = client().await;
    client.send_to(&rrq("f.bin"), server).await.expect("RRQ");
    let id = started(&mut events).await;
    let (_, transfer) = recv(&client).await;
    client
        .send_to(&error(0, "client disk full"), transfer)
        .await
        .expect("ERROR");

    let error = failed(&mut events, id).await;
    assert!(
        error.contains("client error 0: client disk full"),
        "{error}"
    );
    assert!(
        nothing_arrives(&client, Duration::from_millis(200)).await,
        "the server must stop sending once the client gives up"
    );

    let _ = shutdown.send(true);
}

#[tokio::test]
async fn a_client_error_ends_an_upload() {
    let dir = tempfile::tempdir().expect("temporary directory");
    let (server, shutdown, _task, mut events) = start(dir.path(), few_retries()).await;

    let client = client().await;
    client.send_to(&wrq("up.bin"), server).await.expect("WRQ");
    let id = started(&mut events).await;
    let (_, transfer) = recv(&client).await;
    client
        .send_to(&error(0, "cancelled by user"), transfer)
        .await
        .expect("ERROR");

    let error = failed(&mut events, id).await;
    assert!(
        error.contains("client error 0: cancelled by user"),
        "{error}"
    );
    assert!(!dir.path().join("up.bin").exists());

    let _ = shutdown.send(true);
}

#[tokio::test]
async fn a_repeated_block_is_acknowledged_again() {
    let dir = tempfile::tempdir().expect("temporary directory");
    let (server, shutdown, _task, mut events) = start(dir.path(), few_retries()).await;

    let client = client().await;
    client.send_to(&wrq("dup.bin"), server).await.expect("WRQ");
    let (_, transfer) = recv(&client).await;
    let first = vec![1_u8; 512];
    client
        .send_to(&data(1, &first), transfer)
        .await
        .expect("DATA 1");
    expect_ack(&client, 1).await;
    // The client missed ACK 1 and sends block 1 again.
    client
        .send_to(&data(1, &first), transfer)
        .await
        .expect("DATA 1 again");
    expect_ack(&client, 1).await;
    client
        .send_to(&data(2, b"end"), transfer)
        .await
        .expect("DATA 2");
    expect_ack(&client, 2).await;

    let id = started(&mut events).await;
    completed(&mut events, id).await;
    let written = tokio::fs::read(dir.path().join("dup.bin"))
        .await
        .expect("uploaded file");
    assert_eq!(written.len(), 515);

    let _ = shutdown.send(true);
}

#[tokio::test]
async fn unexpected_packets_use_up_an_uploads_retries() {
    let dir = tempfile::tempdir().expect("temporary directory");
    let (server, shutdown, _task, mut events) = start(dir.path(), few_retries()).await;

    let client = client().await;
    client.send_to(&wrq("odd.bin"), server).await.expect("WRQ");
    let id = started(&mut events).await;
    let (_, transfer) = recv(&client).await;
    // An ACK is not something an uploading client sends.
    for _ in 0..3 {
        client.send_to(&ack(9), transfer).await.expect("stray ACK");
    }

    let error = failed(&mut events, id).await;
    assert!(
        error.contains("no usable data after 2 unexpected packets"),
        "{error}"
    );

    let _ = shutdown.send(true);
}

#[tokio::test]
async fn a_partial_window_ack_resends_only_the_rest_of_the_window() {
    let dir = tempfile::tempdir().expect("temporary directory");
    // Ten full 8-byte blocks and a short eleventh.
    tokio::fs::write(dir.path().join("w.bin"), vec![5_u8; 81])
        .await
        .expect("test file");
    let (server, shutdown, _task, _events) = start(
        dir.path(),
        ServerConfig {
            max_window_size: 4,
            ..few_retries()
        },
    )
    .await;

    let client = client().await;
    client
        .send_to(
            &rrq_with_options("w.bin", &[("blksize", "8"), ("windowsize", "4")]),
            server,
        )
        .await
        .expect("RRQ");
    let (packet, transfer) = recv(&client).await;
    assert_eq!(opcode(&packet), 6, "expected an OACK");
    client.send_to(&ack(0), transfer).await.expect("ACK 0");
    assert_eq!(receive_blocks(&client, 4).await, [1, 2, 3, 4]);

    // Blocks 3 and 4 were lost: acknowledge only up to 2.
    client.send_to(&ack(2), transfer).await.expect("ACK 2");
    assert_eq!(receive_blocks(&client, 2).await, [3, 4]);

    client.send_to(&ack(4), transfer).await.expect("ACK 4");
    assert_eq!(receive_blocks(&client, 4).await, [5, 6, 7, 8]);

    let _ = shutdown.send(true);
}

#[tokio::test]
async fn a_client_error_ends_a_windowed_download() {
    let dir = tempfile::tempdir().expect("temporary directory");
    tokio::fs::write(dir.path().join("w.bin"), vec![5_u8; 81])
        .await
        .expect("test file");
    let (server, shutdown, _task, mut events) = start(
        dir.path(),
        ServerConfig {
            max_window_size: 4,
            ..few_retries()
        },
    )
    .await;

    let client = client().await;
    client
        .send_to(
            &rrq_with_options("w.bin", &[("blksize", "8"), ("windowsize", "4")]),
            server,
        )
        .await
        .expect("RRQ");
    let id = started(&mut events).await;
    let (_, transfer) = recv(&client).await;
    client.send_to(&ack(0), transfer).await.expect("ACK 0");
    receive_blocks(&client, 4).await;
    client
        .send_to(&error(0, "stop"), transfer)
        .await
        .expect("ERROR");

    let error = failed(&mut events, id).await;
    assert!(error.contains("client error 0: stop"), "{error}");

    let _ = shutdown.send(true);
}

#[tokio::test]
async fn a_windowed_upload_that_goes_quiet_fails() {
    let dir = tempfile::tempdir().expect("temporary directory");
    let (server, shutdown, _task, mut events) = start(
        dir.path(),
        ServerConfig {
            max_window_size: 4,
            ..impatient()
        },
    )
    .await;

    let client = client().await;
    client
        .send_to(
            &wrq_with_options("w.bin", &[("blksize", "8"), ("windowsize", "4")]),
            server,
        )
        .await
        .expect("WRQ");
    let id = started(&mut events).await;
    let (packet, transfer) = recv(&client).await;
    assert_eq!(opcode(&packet), 6, "expected an OACK");
    client
        .send_to(&data(1, b"12345678"), transfer)
        .await
        .expect("DATA 1");
    // The partial window is acknowledged when the wait for the rest runs
    // out, and then again on each retry, until the server gives up.
    expect_ack(&client, 1).await;

    let error = failed(&mut events, id).await;
    assert!(
        error.contains("timeout waiting for DATA block 2"),
        "{error}"
    );

    let _ = shutdown.send(true);
}

#[tokio::test]
async fn unreadable_packets_use_up_a_windowed_uploads_retries() {
    let dir = tempfile::tempdir().expect("temporary directory");
    let (server, shutdown, _task, mut events) = start(
        dir.path(),
        ServerConfig {
            max_window_size: 4,
            ..few_retries()
        },
    )
    .await;

    let client = client().await;
    client
        .send_to(
            &wrq_with_options("w.bin", &[("blksize", "8"), ("windowsize", "4")]),
            server,
        )
        .await
        .expect("WRQ");
    let id = started(&mut events).await;
    let (_, transfer) = recv(&client).await;
    for _ in 0..3 {
        // Opcode 9 does not exist.
        client.send_to(&[0, 9, 0, 0], transfer).await.expect("junk");
    }

    let error = failed(&mut events, id).await;
    assert!(
        error.contains("no usable data after 2 unreadable packets"),
        "{error}"
    );

    let _ = shutdown.send(true);
}

// ---------------------------------------------------------------------------
// Overwrite protection
// ---------------------------------------------------------------------------

#[tokio::test]
async fn an_upload_without_overwrite_succeeds_when_the_name_is_free() {
    let dir = tempfile::tempdir().expect("temporary directory");
    let (server, shutdown, _task, mut events) = start(
        dir.path(),
        ServerConfig {
            allow_overwrite: false,
            ..ServerConfig::default()
        },
    )
    .await;

    let client = client().await;
    client.send_to(&wrq("new.bin"), server).await.expect("WRQ");
    let (_, transfer) = recv(&client).await;
    upload(&client, transfer, b"fresh", 512).await;

    let id = started(&mut events).await;
    completed(&mut events, id).await;
    let written = tokio::fs::read(dir.path().join("new.bin"))
        .await
        .expect("uploaded file");
    assert_eq!(written, b"fresh");
    assert!(staging_files(dir.path()).is_empty());

    let _ = shutdown.send(true);
}

#[tokio::test]
async fn a_file_that_appears_during_an_upload_is_not_overwritten() {
    let dir = tempfile::tempdir().expect("temporary directory");
    let (server, shutdown, _task, mut events) = start(
        dir.path(),
        ServerConfig {
            allow_overwrite: false,
            ..ServerConfig::default()
        },
    )
    .await;

    let client = client().await;
    client.send_to(&wrq("late.bin"), server).await.expect("WRQ");
    let id = started(&mut events).await;
    let (_, transfer) = recv(&client).await;

    // Someone else creates the file while the upload is running.
    tokio::fs::write(dir.path().join("late.bin"), b"theirs")
        .await
        .expect("competing file");
    client
        .send_to(&data(1, b"ours"), transfer)
        .await
        .expect("DATA 1");

    let (packet, _) = recv(&client).await;
    assert_eq!(
        (opcode(&packet), block(&packet)),
        (5, 6),
        "expected ERROR 6"
    );
    let error = failed(&mut events, id).await;
    assert!(error.contains("file already exists"), "{error}");
    let kept = tokio::fs::read(dir.path().join("late.bin"))
        .await
        .expect("competing file");
    assert_eq!(kept, b"theirs");
    assert!(staging_files(dir.path()).is_empty());

    let _ = shutdown.send(true);
}

// ---------------------------------------------------------------------------
// Requests the listener turns away
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_second_request_from_a_busy_client_is_ignored() {
    let dir = tempfile::tempdir().expect("temporary directory");
    tokio::fs::write(dir.path().join("f.bin"), b"AROS")
        .await
        .expect("test file");
    let (server, shutdown, _task, mut events) = start(dir.path(), ServerConfig::default()).await;

    let client = client().await;
    client.send_to(&rrq("f.bin"), server).await.expect("RRQ");
    let (packet, _) = recv(&client).await;
    assert_eq!((opcode(&packet), block(&packet)), (3, 1));

    // Same address and port, transfer still waiting for ACK 1.
    client
        .send_to(&rrq("f.bin"), server)
        .await
        .expect("RRQ again");
    expect_log(&mut events, "duplicate RRQ ignored").await;

    let _ = shutdown.send(true);
}

#[tokio::test]
async fn an_unexpected_packet_on_the_listener_is_logged_and_skipped() {
    let dir = tempfile::tempdir().expect("temporary directory");
    tokio::fs::write(dir.path().join("f.bin"), b"AROS")
        .await
        .expect("test file");
    let (server, shutdown, _task, mut events) = start(dir.path(), ServerConfig::default()).await;

    let client = client().await;
    client
        .send_to(&data(1, &[0_u8; 512]), server)
        .await
        .expect("stray DATA");
    let line = expect_log(&mut events, "unexpected DATA packet on listener").await;
    assert!(line.len() < 200, "only the packet kind is logged: {line}");

    // The listener keeps serving.
    let received = download(&client, server, &rrq("f.bin"), 512, false).await;
    assert_eq!(received, b"AROS");

    let _ = shutdown.send(true);
}

#[tokio::test]
async fn disabled_reads_and_writes_are_refused_with_an_access_error() {
    let dir = tempfile::tempdir().expect("temporary directory");
    tokio::fs::write(dir.path().join("f.bin"), b"AROS")
        .await
        .expect("test file");
    let (server, shutdown, _task, mut events) = start(
        dir.path(),
        ServerConfig {
            enable_read: false,
            enable_write: false,
            ..ServerConfig::default()
        },
    )
    .await;

    let client = client().await;
    for (packet, kind) in [
        (request(1, "f.bin", "octet", &[]), "RRQ"),
        (request(2, "g.bin", "octet", &[]), "WRQ"),
    ] {
        client.send_to(&packet, server).await.expect("request");
        let (reply, _) = recv(&client).await;
        assert_eq!(
            (opcode(&reply), block(&reply)),
            (5, 2),
            "{kind}: expected ERROR 2"
        );
        expect_log(&mut events, &format!("{kind} rejected")).await;
    }
    assert!(!dir.path().join("g.bin").exists());

    let _ = shutdown.send(true);
}
