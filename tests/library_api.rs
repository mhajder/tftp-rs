use std::net::SocketAddr;
use std::time::Duration;

#[cfg(any(target_os = "linux", target_os = "macos"))]
use tftp_rs::server::run_on_interface;
use tftp_rs::server::{ServerConfig, ServerEvent, probe_bind, run, validate_bind_addr};
use tokio::net::UdpSocket;
use tokio::sync::{mpsc, watch};

#[test]
fn validate_bind_addr_rejects_wildcards_and_accepts_explicit_addresses() {
    let error = validate_bind_addr(SocketAddr::from(([0, 0, 0, 0], 69)))
        .expect_err("wildcard binds must be rejected");
    assert!(error.to_string().contains("wildcard bind address"));

    validate_bind_addr(SocketAddr::from(([0, 0, 0, 0, 0, 0, 0, 0], 69)))
        .expect_err("the IPv6 wildcard must be rejected too");

    validate_bind_addr(SocketAddr::from(([127, 0, 0, 1], 69)))
        .expect("an explicit address is accepted");
}

#[tokio::test]
async fn serves_from_a_wildcard_listener() {
    let dir = tempfile::tempdir().expect("temporary directory");
    tokio::fs::write(dir.path().join("hello.txt"), b"AROS")
        .await
        .expect("test file");

    let reservation = UdpSocket::bind("0.0.0.0:0")
        .await
        .expect("reserve test port");
    let port = reservation.local_addr().expect("listener address").port();
    drop(reservation);

    let (events, mut event_rx) = mpsc::unbounded_channel();
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let server_dir = dir.path().to_path_buf();
    let server = tokio::spawn(async move {
        run(
            SocketAddr::from(([0, 0, 0, 0], port)),
            server_dir,
            events,
            shutdown_rx,
            ServerConfig::default(),
        )
        .await
    });

    wait_until_listening(&mut event_rx).await;

    let client = UdpSocket::bind("127.0.0.1:0").await.expect("client socket");
    let server_address = SocketAddr::from(([127, 0, 0, 1], port));
    client
        .send_to(&rrq("hello.txt"), server_address)
        .await
        .expect("RRQ");

    let mut buffer = [0_u8; 516];
    let (length, transfer_address) =
        tokio::time::timeout(Duration::from_secs(2), client.recv_from(&mut buffer))
            .await
            .expect("TFTP response timeout")
            .expect("TFTP response");

    assert_eq!(&buffer[..4], &[0, 3, 0, 1]);
    assert_eq!(&buffer[4..length], b"AROS");

    client
        .send_to(&[0, 4, 0, 1], transfer_address)
        .await
        .expect("ACK");

    shutdown_tx.send(true).expect("shutdown signal");
    server.await.expect("server task").expect("server result");
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[tokio::test]
async fn interface_bound_run_accepts_a_wildcard_listener() {
    let dir = tempfile::tempdir().expect("temporary directory");
    let (events, _event_rx) = mpsc::unbounded_channel();
    let (_shutdown_tx, shutdown_rx) = watch::channel(false);

    // The interface binding scopes the service, so a wildcard address is fine.
    // Any failure here must come from interface resolution, not the address.
    let error = run_on_interface(
        SocketAddr::from(([0, 0, 0, 0], 0)),
        "tftp-no-such0",
        dir.path().to_path_buf(),
        events,
        shutdown_rx,
        ServerConfig::default(),
    )
    .await
    .expect_err("the bogus interface must still be rejected");

    assert!(!error.to_string().contains("wildcard bind address"));
    assert!(error.to_string().contains("tftp-no-such0"));
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[tokio::test]
async fn interface_bound_run_rejects_a_missing_interface() {
    let dir = tempfile::tempdir().expect("temporary directory");
    let (events, _event_rx) = mpsc::unbounded_channel();
    let (_shutdown_tx, shutdown_rx) = watch::channel(false);

    let error = run_on_interface(
        "127.0.0.1:0".parse().expect("socket address"),
        "tftp-no-such0",
        dir.path().to_path_buf(),
        events,
        shutdown_rx,
        ServerConfig::default(),
    )
    .await
    .expect_err("missing interface must never fall back to an unbound socket");

    assert!(error.to_string().contains("was not found"));
}

#[tokio::test]
async fn serves_from_the_explicit_listener_address() {
    let dir = tempfile::tempdir().expect("temporary directory");
    tokio::fs::write(dir.path().join("hello.txt"), b"AROS")
        .await
        .expect("test file");

    let reservation = UdpSocket::bind("127.0.0.1:0")
        .await
        .expect("reserve test port");
    let listener_address = reservation.local_addr().expect("listener address");
    drop(reservation);

    let (events, mut event_rx) = mpsc::unbounded_channel();
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let server_dir = dir.path().to_path_buf();
    let server = tokio::spawn(async move {
        run(
            listener_address,
            server_dir,
            events,
            shutdown_rx,
            ServerConfig::default(),
        )
        .await
    });

    wait_until_listening(&mut event_rx).await;

    let client = UdpSocket::bind("127.0.0.1:0").await.expect("client socket");
    client
        .send_to(&rrq("hello.txt"), listener_address)
        .await
        .expect("RRQ");

    let mut buffer = [0_u8; 516];
    let (length, transfer_address) =
        tokio::time::timeout(Duration::from_secs(2), client.recv_from(&mut buffer))
            .await
            .expect("TFTP response timeout")
            .expect("TFTP response");

    assert_eq!(transfer_address.ip(), listener_address.ip());
    assert_eq!(&buffer[..4], &[0, 3, 0, 1]);
    assert_eq!(&buffer[4..length], b"AROS");

    client
        .send_to(&[0, 4, 0, 1], transfer_address)
        .await
        .expect("ACK");

    shutdown_tx.send(true).expect("shutdown signal");
    server.await.expect("server task").expect("server result");
}

/// The loopback interface, which is the only device every host is sure to have.
#[cfg(any(target_os = "linux", target_os = "macos"))]
const LOOPBACK_INTERFACE: &str = if cfg!(target_os = "macos") {
    "lo0"
} else {
    "lo"
};

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[tokio::test]
async fn binds_listener_and_transfer_sockets_to_named_interface() {
    let dir = tempfile::tempdir().expect("temporary directory");
    tokio::fs::write(dir.path().join("hello.txt"), b"AROS")
        .await
        .expect("test file");

    let reservation = UdpSocket::bind("127.0.0.1:0")
        .await
        .expect("reserve test port");
    let listener_address = reservation.local_addr().expect("listener address");
    drop(reservation);

    let (events, mut event_rx) = mpsc::unbounded_channel();
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let server_dir = dir.path().to_path_buf();
    let server = tokio::spawn(async move {
        run_on_interface(
            listener_address,
            LOOPBACK_INTERFACE,
            server_dir,
            events,
            shutdown_rx,
            ServerConfig::default(),
        )
        .await
    });

    wait_until_listening(&mut event_rx).await;

    let client = UdpSocket::bind("127.0.0.1:0").await.expect("client socket");
    client
        .send_to(&rrq("hello.txt"), listener_address)
        .await
        .expect("RRQ");

    let mut buffer = [0_u8; 516];
    let (length, transfer_address) =
        tokio::time::timeout(Duration::from_secs(2), client.recv_from(&mut buffer))
            .await
            .expect("TFTP response timeout")
            .expect("TFTP response");

    assert_eq!(transfer_address.ip(), listener_address.ip());
    assert_eq!(&buffer[..4], &[0, 3, 0, 1]);
    assert_eq!(&buffer[4..length], b"AROS");

    client
        .send_to(&[0, 4, 0, 1], transfer_address)
        .await
        .expect("ACK");

    shutdown_tx.send(true).expect("shutdown signal");
    server.await.expect("server task").expect("server result");
}

async fn wait_until_listening(events: &mut mpsc::UnboundedReceiver<ServerEvent>) {
    loop {
        let event = tokio::time::timeout(Duration::from_secs(2), events.recv())
            .await
            .expect("server startup timeout")
            .expect("server event channel");
        if matches!(event, ServerEvent::Log(message) if message.starts_with("Listening on ")) {
            return;
        }
    }
}

fn rrq(filename: &str) -> Vec<u8> {
    let mut request = Vec::new();
    request.extend_from_slice(&1_u16.to_be_bytes());
    request.extend_from_slice(filename.as_bytes());
    request.push(0);
    request.extend_from_slice(b"octet");
    request.push(0);
    request
}

#[tokio::test]
async fn probe_bind_reports_an_address_already_in_use() {
    let holder = UdpSocket::bind("127.0.0.1:0").await.expect("held socket");
    let taken = holder.local_addr().expect("held address");

    let error = probe_bind(taken, None).expect_err("a held address must be rejected");
    assert!(
        error.to_string().contains("in use"),
        "unexpected error: {error}"
    );

    drop(holder);
    probe_bind(taken, None).expect("the address is free once released");
}

#[tokio::test]
async fn probe_bind_accepts_a_wildcard_the_host_can_bind() {
    // Unlike validate_bind_addr, probing asks the OS rather than refusing
    // wildcards on principle.
    probe_bind(SocketAddr::from(([0, 0, 0, 0], 0)), None).expect("wildcard bind");
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[tokio::test]
async fn probe_bind_rejects_a_missing_interface() {
    let error = probe_bind(
        SocketAddr::from(([127, 0, 0, 1], 0)),
        Some("tftp-rs-no-such-if"),
    )
    .expect_err("an unknown interface must be rejected");
    assert!(
        error.to_string().contains("was not found"),
        "unexpected error: {error}"
    );
}

#[tokio::test]
async fn windowed_download_numbers_blocks_consecutively() {
    const BLOCKS: usize = 5;
    const WINDOW: u16 = 2;

    let dir = tempfile::tempdir().expect("temporary directory");
    // Four full blocks plus a short one, so the transfer spans several windows
    // and still ends on a block the server marks as final.
    let body: Vec<u8> = (0..(512 * (BLOCKS - 1) + 100))
        .map(|i| (i % 251) as u8)
        .collect();
    tokio::fs::write(dir.path().join("big.bin"), &body)
        .await
        .expect("test file");

    let reservation = UdpSocket::bind("127.0.0.1:0")
        .await
        .expect("reserve test port");
    let listener_address = reservation.local_addr().expect("listener address");
    drop(reservation);

    let (events, mut event_rx) = mpsc::unbounded_channel();
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let server_dir = dir.path().to_path_buf();
    let server = tokio::spawn(async move {
        run(
            listener_address,
            server_dir,
            events,
            shutdown_rx,
            ServerConfig {
                max_window_size: WINDOW,
                ..ServerConfig::default()
            },
        )
        .await
    });

    wait_until_listening(&mut event_rx).await;

    let client = UdpSocket::bind("127.0.0.1:0").await.expect("client socket");
    client
        .send_to(
            &rrq_with_options("big.bin", &[("windowsize", &WINDOW.to_string())]),
            listener_address,
        )
        .await
        .expect("RRQ");

    let mut buffer = vec![0_u8; 1024];
    let mut transfer_address = None;
    let mut seen_blocks = Vec::new();
    let mut received = Vec::new();

    loop {
        let (length, from) = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            client.recv_from(&mut buffer),
        )
        .await
        .expect("server went quiet mid-transfer")
        .expect("datagram");
        let from = *transfer_address.get_or_insert(from);

        match u16::from_be_bytes([buffer[0], buffer[1]]) {
            // OACK: accept the negotiated options.
            6 => {
                client.send_to(&[0, 4, 0, 0], from).await.expect("ACK 0");
            }
            // DATA: record the block number, acknowledge the end of a window.
            3 => {
                let block = u16::from_be_bytes([buffer[2], buffer[3]]);
                seen_blocks.push(block);
                received.extend_from_slice(&buffer[4..length]);
                let short = length - 4 < 512;
                if short || seen_blocks.len() % WINDOW as usize == 0 {
                    client
                        .send_to(&[0, 4, buffer[2], buffer[3]], from)
                        .await
                        .expect("ACK");
                }
                if short {
                    break;
                }
            }
            opcode => panic!("unexpected opcode {opcode}"),
        }
    }

    let expected: Vec<u16> = (1..=BLOCKS as u16).collect();
    assert_eq!(
        seen_blocks, expected,
        "windowed transfers must number blocks consecutively"
    );
    assert_eq!(received, body, "the file must arrive intact");

    shutdown_tx.send(true).expect("shutdown signal");
    server.await.expect("server task").expect("server result");
}

fn rrq_with_options(filename: &str, options: &[(&str, &str)]) -> Vec<u8> {
    let mut request = rrq(filename);
    for (key, value) in options {
        request.extend_from_slice(key.as_bytes());
        request.push(0);
        request.extend_from_slice(value.as_bytes());
        request.push(0);
    }
    request
}

#[tokio::test]
async fn a_request_for_a_missing_file_is_answered_with_an_error() {
    let dir = tempfile::tempdir().expect("temporary directory");

    let reservation = UdpSocket::bind("127.0.0.1:0")
        .await
        .expect("reserve test port");
    let listener_address = reservation.local_addr().expect("listener address");
    drop(reservation);

    let (events, mut event_rx) = mpsc::unbounded_channel();
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let server_dir = dir.path().to_path_buf();
    let server = tokio::spawn(async move {
        run(
            listener_address,
            server_dir,
            events,
            shutdown_rx,
            ServerConfig::default(),
        )
        .await
    });

    wait_until_listening(&mut event_rx).await;

    let client = UdpSocket::bind("127.0.0.1:0").await.expect("client socket");
    client
        .send_to(&rrq("absent.txt"), listener_address)
        .await
        .expect("RRQ");

    let mut buffer = [0_u8; 512];
    let (length, _) = tokio::time::timeout(Duration::from_secs(2), client.recv_from(&mut buffer))
        .await
        .expect("the client must not be left waiting for its own timeout")
        .expect("datagram");

    // ERROR, code 1 (file not found), then a NUL-terminated message.
    assert_eq!(&buffer[..4], &[0, 5, 0, 1]);
    assert_eq!(buffer[length - 1], 0);

    shutdown_tx.send(true).expect("shutdown signal");
    server.await.expect("server task").expect("server result");
}

#[tokio::test]
async fn transfers_survive_a_caller_that_stops_reading_events() {
    let dir = tempfile::tempdir().expect("temporary directory");
    tokio::fs::write(dir.path().join("hello.txt"), b"AROS")
        .await
        .expect("test file");

    let reservation = UdpSocket::bind("127.0.0.1:0")
        .await
        .expect("reserve test port");
    let listener_address = reservation.local_addr().expect("listener address");
    drop(reservation);

    let (events, mut event_rx) = mpsc::unbounded_channel();
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let server_dir = dir.path().to_path_buf();
    let server = tokio::spawn(async move {
        run(
            listener_address,
            server_dir,
            events,
            shutdown_rx,
            ServerConfig::default(),
        )
        .await
    });

    wait_until_listening(&mut event_rx).await;

    // An embedder that only wanted a file server, and never a dashboard, has
    // no reason to keep draining events. That must cost it the events alone.
    drop(event_rx);

    let client = UdpSocket::bind("127.0.0.1:0").await.expect("client socket");
    client
        .send_to(&rrq("hello.txt"), listener_address)
        .await
        .expect("RRQ");

    let mut buffer = [0_u8; 516];
    let (length, transfer) =
        tokio::time::timeout(Duration::from_secs(2), client.recv_from(&mut buffer))
            .await
            .expect("the transfer must run without anyone listening to the events")
            .expect("datagram");

    assert_eq!(&buffer[..4], &[0, 3, 0, 1]);
    assert_eq!(&buffer[4..length], b"AROS");

    client.send_to(&[0, 4, 0, 1], transfer).await.expect("ACK");

    shutdown_tx.send(true).expect("shutdown signal");
    server
        .await
        .expect("server task")
        .expect("a shutdown with no event receiver is still a clean shutdown");
}

#[tokio::test]
async fn a_netascii_download_does_not_acknowledge_tsize() {
    let dir = tempfile::tempdir().expect("temporary directory");
    // Six bytes on disk, nine on the wire: each LF is sent as CR LF.
    tokio::fs::write(dir.path().join("lines.txt"), b"a\nb\nc\n")
        .await
        .expect("test file");

    let reservation = UdpSocket::bind("127.0.0.1:0")
        .await
        .expect("reserve test port");
    let listener_address = reservation.local_addr().expect("listener address");
    drop(reservation);

    let (events, mut event_rx) = mpsc::unbounded_channel();
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let server_dir = dir.path().to_path_buf();
    let server = tokio::spawn(async move {
        run(
            listener_address,
            server_dir,
            events,
            shutdown_rx,
            ServerConfig::default(),
        )
        .await
    });

    wait_until_listening(&mut event_rx).await;

    let client = UdpSocket::bind("127.0.0.1:0").await.expect("client socket");
    let mut request = Vec::from(&1_u16.to_be_bytes()[..]);
    for field in [
        &b"lines.txt"[..],
        b"netascii",
        b"blksize",
        b"512",
        b"tsize",
        b"0",
    ] {
        request.extend_from_slice(field);
        request.push(0);
    }
    client
        .send_to(&request, listener_address)
        .await
        .expect("RRQ");

    let mut buffer = [0_u8; 616];
    let (length, transfer) =
        tokio::time::timeout(Duration::from_secs(2), client.recv_from(&mut buffer))
            .await
            .expect("OACK timeout")
            .expect("datagram");

    assert_eq!(&buffer[..2], &[0, 6], "expected an OACK");
    let fields: Vec<&[u8]> = buffer[2..length].split(|&b| b == 0).collect();
    assert!(
        fields.iter().any(|f| *f == b"blksize"),
        "blksize is still negotiated"
    );
    assert!(
        !fields.iter().any(|f| *f == b"tsize"),
        "tsize cannot be answered for a netascii transfer: {:?}",
        String::from_utf8_lossy(&buffer[2..length])
    );

    client
        .send_to(&[0, 4, 0, 0], transfer)
        .await
        .expect("ACK 0");

    let (length, _) = tokio::time::timeout(Duration::from_secs(2), client.recv_from(&mut buffer))
        .await
        .expect("DATA timeout")
        .expect("datagram");
    assert_eq!(&buffer[..4], &[0, 3, 0, 1]);
    assert_eq!(
        &buffer[4..length],
        b"a\r\nb\r\nc\r\n",
        "the transfer is nine bytes, which is why the six-byte file size was the wrong answer"
    );

    shutdown_tx.send(true).expect("shutdown signal");
    server.await.expect("server task").expect("server result");
}

#[tokio::test]
async fn unanswered_requests_cannot_take_more_than_their_share_of_sockets() {
    let dir = tempfile::tempdir().expect("temporary directory");
    tokio::fs::write(dir.path().join("big.bin"), vec![7_u8; 4096])
        .await
        .expect("test file");

    let reservation = UdpSocket::bind("127.0.0.1:0")
        .await
        .expect("reserve test port");
    let listener_address = reservation.local_addr().expect("listener address");
    drop(reservation);

    let (events, mut event_rx) = mpsc::unbounded_channel();
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let server_dir = dir.path().to_path_buf();
    let server = tokio::spawn(async move {
        run(
            listener_address,
            server_dir,
            events,
            shutdown_rx,
            ServerConfig {
                max_concurrent_transfers: 4,
                ..ServerConfig::default()
            },
        )
        .await
    });

    wait_until_listening(&mut event_rx).await;

    // Each of these takes a slot and never acknowledges anything, so all four
    // stay held for the whole retry budget.
    let mut hoarders = Vec::new();
    for _ in 0..4 {
        let client = UdpSocket::bind("127.0.0.1:0").await.expect("client socket");
        client
            .send_to(&rrq("big.bin"), listener_address)
            .await
            .expect("RRQ");
        hoarders.push(client);
    }

    // Wait until every slot is actually taken, rather than guessing at a delay.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    let mut started = 0;
    while started < 4 {
        let event = tokio::time::timeout_at(deadline, event_rx.recv())
            .await
            .expect("the four transfers must start")
            .expect("event channel");
        if matches!(event, ServerEvent::TransferStarted(_)) {
            started += 1;
        }
    }

    let refused = UdpSocket::bind("127.0.0.1:0").await.expect("client socket");
    refused
        .send_to(&rrq("big.bin"), listener_address)
        .await
        .expect("RRQ");

    // The fifth gets nothing: answering a flood packet for packet is what the
    // limit exists to avoid. A real client retransmits and gets in later.
    let mut buffer = [0_u8; 616];
    let answered =
        tokio::time::timeout(Duration::from_millis(400), refused.recv_from(&mut buffer)).await;
    assert!(
        answered.is_err(),
        "a request over the limit must not be given a transfer socket"
    );

    shutdown_tx.send(true).expect("shutdown signal");
    server.await.expect("server task").expect("server result");
}

#[tokio::test]
async fn an_upload_past_the_size_limit_is_cut_off() {
    let dir = tempfile::tempdir().expect("temporary directory");

    let reservation = UdpSocket::bind("127.0.0.1:0")
        .await
        .expect("reserve test port");
    let listener_address = reservation.local_addr().expect("listener address");
    drop(reservation);

    let (events, mut event_rx) = mpsc::unbounded_channel();
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let server_dir = dir.path().to_path_buf();
    let server = tokio::spawn(async move {
        run(
            listener_address,
            server_dir,
            events,
            shutdown_rx,
            ServerConfig {
                max_upload_bytes: 1000,
                ..ServerConfig::default()
            },
        )
        .await
    });

    wait_until_listening(&mut event_rx).await;

    let client = UdpSocket::bind("127.0.0.1:0").await.expect("client socket");
    let mut request = Vec::from(&2_u16.to_be_bytes()[..]);
    request.extend_from_slice(b"flood.bin\0octet\0");
    client
        .send_to(&request, listener_address)
        .await
        .expect("WRQ");

    let mut buffer = [0_u8; 616];
    let (_, from) = tokio::time::timeout(Duration::from_secs(2), client.recv_from(&mut buffer))
        .await
        .expect("ACK 0 timeout")
        .expect("datagram");
    assert_eq!(&buffer[..4], &[0, 4, 0, 0]);

    // Full blocks, so the transfer never signals an end of its own. The third
    // one carries the total past 1000 bytes.
    let mut block = 1_u16;
    let error_code = loop {
        assert!(block <= 8, "the server never stopped the upload");
        let mut data = Vec::from(&3_u16.to_be_bytes()[..]);
        data.extend_from_slice(&block.to_be_bytes());
        data.extend_from_slice(&vec![9_u8; 512]);
        client.send_to(&data, from).await.expect("DATA");

        let (_, _) = tokio::time::timeout(Duration::from_secs(2), client.recv_from(&mut buffer))
            .await
            .expect("no answer to a DATA block")
            .expect("datagram");
        if buffer[..2] == [0, 5] {
            break u16::from_be_bytes([buffer[2], buffer[3]]);
        }
        assert_eq!(&buffer[..2], &[0, 4], "expected ACK or ERROR");
        block += 1;
    };

    // Code 3, "disk full or allocation exceeded".
    assert_eq!(error_code, 3);

    // Nothing oversized is left behind, staging file included. The error
    // reaches the client before the handler returns, and the staging file is
    // removed after that, so this waits for the cleanup rather than assuming
    // it has already happened by the time the error arrives.
    let leftovers = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let names: Vec<_> = std::fs::read_dir(dir.path())
                .expect("served directory")
                .filter_map(|e| e.ok())
                .map(|e| e.file_name().to_string_lossy().to_string())
                .collect();
            if names.is_empty() {
                return names;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await;
    assert!(
        leftovers.is_ok(),
        "the staging file was never cleaned up: {:?}",
        std::fs::read_dir(dir.path())
            .expect("served directory")
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().to_string())
            .collect::<Vec<_>>()
    );

    shutdown_tx.send(true).expect("shutdown signal");
    server.await.expect("server task").expect("server result");
}

#[tokio::test]
async fn a_filename_cannot_forge_a_line_in_the_log() {
    let dir = tempfile::tempdir().expect("temporary directory");

    let reservation = UdpSocket::bind("127.0.0.1:0")
        .await
        .expect("reserve test port");
    let listener_address = reservation.local_addr().expect("listener address");
    drop(reservation);

    let (events, mut event_rx) = mpsc::unbounded_channel();
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let server_dir = dir.path().to_path_buf();
    let server = tokio::spawn(async move {
        run(
            listener_address,
            server_dir,
            events,
            shutdown_rx,
            ServerConfig::default(),
        )
        .await
    });

    wait_until_listening(&mut event_rx).await;

    // Needs no valid file and no write access: one datagram is enough.
    let client = UdpSocket::bind("127.0.0.1:0").await.expect("client socket");
    let mut request = Vec::from(&1_u16.to_be_bytes()[..]);
    request.extend_from_slice(b"a\n[00:00:00] 10.0.0.1: RRQ \"secrets\" complete\0octet\0");
    client
        .send_to(&request, listener_address)
        .await
        .expect("RRQ");

    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    loop {
        let event = tokio::time::timeout_at(deadline, event_rx.recv())
            .await
            .expect("no log mentioning the request arrived")
            .expect("event channel");
        if let ServerEvent::Log(message) = event
            && message.contains("RRQ")
        {
            assert!(
                !message.contains('\n'),
                "a log message must be one line: {message:?}"
            );
            assert!(
                message.contains("\\n"),
                "the newline should still be visible as an escape: {message:?}"
            );
            break;
        }
    }

    shutdown_tx.send(true).expect("shutdown signal");
    server.await.expect("server task").expect("server result");
}

#[tokio::test]
async fn an_upload_onto_a_directory_is_refused_before_it_starts() {
    let dir = tempfile::tempdir().expect("temporary directory");
    std::fs::create_dir(dir.path().join("taken")).expect("a directory in the way");

    let reservation = UdpSocket::bind("127.0.0.1:0")
        .await
        .expect("reserve test port");
    let listener_address = reservation.local_addr().expect("listener address");
    drop(reservation);

    let (events, mut event_rx) = mpsc::unbounded_channel();
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let server_dir = dir.path().to_path_buf();
    let server = tokio::spawn(async move {
        run(
            listener_address,
            server_dir,
            events,
            shutdown_rx,
            ServerConfig::default(),
        )
        .await
    });

    wait_until_listening(&mut event_rx).await;

    let client = UdpSocket::bind("127.0.0.1:0").await.expect("client socket");
    let mut request = Vec::from(&2_u16.to_be_bytes()[..]);
    request.extend_from_slice(b"taken\0octet\0");
    client
        .send_to(&request, listener_address)
        .await
        .expect("WRQ");

    let mut buffer = [0_u8; 512];
    let (length, _) = tokio::time::timeout(Duration::from_secs(2), client.recv_from(&mut buffer))
        .await
        .expect("the client must not be left waiting for its own timeout")
        .expect("datagram");

    // ERROR, code 2 (access violation). An ACK here would start an upload that
    // can never be stored, and the client would be told it succeeded.
    assert_eq!(&buffer[..4], &[0, 5, 0, 2]);
    assert_eq!(buffer[length - 1], 0);

    // The directory is untouched, and no staging file was left beside it.
    assert!(dir.path().join("taken").is_dir());
    let strays: Vec<_> = std::fs::read_dir(dir.path())
        .expect("served directory")
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().to_string())
        .filter(|name| name != "taken")
        .collect();
    assert!(strays.is_empty(), "unexpected leftovers: {strays:?}");

    shutdown_tx.send(true).expect("shutdown signal");
    server.await.expect("server task").expect("server result");
}

#[tokio::test]
async fn a_request_for_a_directory_is_answered_with_an_error() {
    let dir = tempfile::tempdir().expect("temporary directory");
    std::fs::create_dir(dir.path().join("subdir")).expect("a directory to request");

    let reservation = UdpSocket::bind("127.0.0.1:0")
        .await
        .expect("reserve test port");
    let listener_address = reservation.local_addr().expect("listener address");
    drop(reservation);

    let (events, mut event_rx) = mpsc::unbounded_channel();
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let server_dir = dir.path().to_path_buf();
    let server = tokio::spawn(async move {
        run(
            listener_address,
            server_dir,
            events,
            shutdown_rx,
            ServerConfig::default(),
        )
        .await
    });

    wait_until_listening(&mut event_rx).await;

    let client = UdpSocket::bind("127.0.0.1:0").await.expect("client socket");
    client
        .send_to(&rrq("subdir"), listener_address)
        .await
        .expect("RRQ");

    let mut buffer = [0_u8; 512];
    let (length, _) = tokio::time::timeout(Duration::from_secs(2), client.recv_from(&mut buffer))
        .await
        .expect("the client must not be left waiting for its own timeout")
        .expect("datagram");

    // ERROR, code 1 (file not found), then a NUL-terminated message.
    assert_eq!(&buffer[..4], &[0, 5, 0, 1]);
    assert_eq!(buffer[length - 1], 0);

    shutdown_tx.send(true).expect("shutdown signal");
    server.await.expect("server task").expect("server result");
}

#[tokio::test]
async fn a_lost_oack_is_retransmitted_as_an_oack() {
    let dir = tempfile::tempdir().expect("temporary directory");

    let reservation = UdpSocket::bind("127.0.0.1:0")
        .await
        .expect("reserve test port");
    let listener_address = reservation.local_addr().expect("listener address");
    drop(reservation);

    let (events, mut event_rx) = mpsc::unbounded_channel();
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let server_dir = dir.path().to_path_buf();
    let server = tokio::spawn(async move {
        run(
            listener_address,
            server_dir,
            events,
            shutdown_rx,
            ServerConfig::default(),
        )
        .await
    });

    wait_until_listening(&mut event_rx).await;

    let client = UdpSocket::bind("127.0.0.1:0").await.expect("client socket");
    client
        .send_to(
            &wrq_with_options("up.txt", &[("blksize", "1024")]),
            listener_address,
        )
        .await
        .expect("WRQ");

    let mut buffer = [0_u8; 1024];
    let (_, from) = tokio::time::timeout(Duration::from_secs(2), client.recv_from(&mut buffer))
        .await
        .expect("OACK timeout")
        .expect("datagram");
    assert_eq!(
        u16::from_be_bytes([buffer[0], buffer[1]]),
        6,
        "the first reply to an option request is an OACK"
    );

    // Drop it, as a lossy link would, and wait for the server to try again.
    let (_, _) = tokio::time::timeout(Duration::from_secs(3), client.recv_from(&mut buffer))
        .await
        .expect("the server must retry")
        .expect("datagram");
    assert_eq!(
        u16::from_be_bytes([buffer[0], buffer[1]]),
        6,
        "a retransmission must repeat the OACK, not fall back to ACK 0"
    );

    // Finish the upload at the negotiated block size.
    let mut data = vec![0, 3, 0, 1];
    data.extend_from_slice(b"body");
    client.send_to(&data, from).await.expect("DATA 1");

    let (_, _) = tokio::time::timeout(Duration::from_secs(2), client.recv_from(&mut buffer))
        .await
        .expect("ACK timeout")
        .expect("datagram");
    assert_eq!(&buffer[..4], &[0, 4, 0, 1], "block 1 acknowledged");

    // The rename happens after the last ACK, on the transfer task.
    let uploaded = dir.path().join("up.txt");
    let body = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if let Ok(body) = tokio::fs::read(&uploaded).await {
                return body;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the upload must be promoted to its final name");
    assert_eq!(body, b"body");

    shutdown_tx.send(true).expect("shutdown signal");
    server.await.expect("server task").expect("server result");
}

fn wrq_with_options(filename: &str, options: &[(&str, &str)]) -> Vec<u8> {
    let mut request = Vec::new();
    request.extend_from_slice(&2_u16.to_be_bytes());
    request.extend_from_slice(filename.as_bytes());
    request.push(0);
    request.extend_from_slice(b"octet");
    request.push(0);
    for (key, value) in options {
        request.extend_from_slice(key.as_bytes());
        request.push(0);
        request.extend_from_slice(value.as_bytes());
        request.push(0);
    }
    request
}

#[tokio::test]
async fn a_client_replaying_one_ack_cannot_hold_a_download_open() {
    let dir = tempfile::tempdir().expect("temporary directory");
    tokio::fs::write(dir.path().join("big.bin"), vec![7_u8; 4096])
        .await
        .expect("test file");

    let reservation = UdpSocket::bind("127.0.0.1:0")
        .await
        .expect("reserve test port");
    let listener_address = reservation.local_addr().expect("listener address");
    drop(reservation);

    let (events, mut event_rx) = mpsc::unbounded_channel();
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let server_dir = dir.path().to_path_buf();
    let server = tokio::spawn(async move {
        run(
            listener_address,
            server_dir,
            events,
            shutdown_rx,
            ServerConfig {
                max_retries: 3,
                ..ServerConfig::default()
            },
        )
        .await
    });

    wait_until_listening(&mut event_rx).await;

    let client = UdpSocket::bind("127.0.0.1:0").await.expect("client socket");
    client
        .send_to(&rrq("big.bin"), listener_address)
        .await
        .expect("RRQ");

    // Answer every DATA block with ACK 0, which acknowledges nothing. Without
    // a bound this never ends: one full block sent per 4-byte ACK.
    let mut buffer = vec![0_u8; 1024];
    let mut blocks_sent = 0;
    loop {
        let received =
            tokio::time::timeout(Duration::from_millis(1500), client.recv_from(&mut buffer)).await;
        let Ok(Ok((_, from))) = received else {
            // The server went quiet, which is the point: it gave up.
            break;
        };
        blocks_sent += 1;
        assert!(
            blocks_sent <= 8,
            "the server is still resending after {blocks_sent} blocks; max_retries is 3"
        );
        client.send_to(&[0, 4, 0, 0], from).await.expect("ACK 0");
    }

    shutdown_tx.send(true).expect("shutdown signal");
    server.await.expect("server task").expect("server result");
}

#[tokio::test]
async fn a_client_replaying_one_ack_cannot_hold_a_windowed_download_open() {
    let dir = tempfile::tempdir().expect("temporary directory");
    tokio::fs::write(dir.path().join("big.bin"), vec![7_u8; 32768])
        .await
        .expect("test file");

    let reservation = UdpSocket::bind("127.0.0.1:0")
        .await
        .expect("reserve test port");
    let listener_address = reservation.local_addr().expect("listener address");
    drop(reservation);

    let (events, mut event_rx) = mpsc::unbounded_channel();
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let server_dir = dir.path().to_path_buf();
    let server = tokio::spawn(async move {
        run(
            listener_address,
            server_dir,
            events,
            shutdown_rx,
            ServerConfig {
                max_retries: 3,
                max_window_size: 4,
                ..ServerConfig::default()
            },
        )
        .await
    });

    wait_until_listening(&mut event_rx).await;

    let client = UdpSocket::bind("127.0.0.1:0").await.expect("client socket");
    client
        .send_to(
            &rrq_with_options("big.bin", &[("windowsize", "4")]),
            listener_address,
        )
        .await
        .expect("RRQ");

    let mut buffer = vec![0_u8; 1024];
    let (_, transfer) = tokio::time::timeout(Duration::from_secs(2), client.recv_from(&mut buffer))
        .await
        .expect("OACK timeout")
        .expect("OACK");
    assert_eq!(&buffer[..2], &[0, 6], "expected an OACK");
    client
        .send_to(&[0, 4, 0, 0], transfer)
        .await
        .expect("ACK 0");

    // ACK 0 is the block before the first window, so every reply looks like a
    // request to resend it. Nothing is ever acknowledged, so the server has to
    // stop rather than answer four blocks to every four-byte packet.
    let mut blocks_sent = 0;
    loop {
        let received =
            tokio::time::timeout(Duration::from_millis(1500), client.recv_from(&mut buffer)).await;
        let Ok(Ok((_, from))) = received else {
            // The server went quiet, which is the point: it gave up.
            break;
        };
        blocks_sent += 1;
        assert!(
            blocks_sent <= 40,
            "the server is still resending after {blocks_sent} blocks; max_retries is 3"
        );
        client.send_to(&[0, 4, 0, 0], from).await.expect("ACK 0");
    }

    shutdown_tx.send(true).expect("shutdown signal");
    server.await.expect("server task").expect("server result");
}

#[tokio::test]
async fn a_netascii_upload_ending_in_cr_keeps_that_byte() {
    let dir = tempfile::tempdir().expect("temporary directory");

    let reservation = UdpSocket::bind("127.0.0.1:0")
        .await
        .expect("reserve test port");
    let listener_address = reservation.local_addr().expect("listener address");
    drop(reservation);

    let (events, mut event_rx) = mpsc::unbounded_channel();
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let server_dir = dir.path().to_path_buf();
    let server = tokio::spawn(async move {
        run(
            listener_address,
            server_dir,
            events,
            shutdown_rx,
            ServerConfig::default(),
        )
        .await
    });

    wait_until_listening(&mut event_rx).await;

    let client = UdpSocket::bind("127.0.0.1:0").await.expect("client socket");
    let mut request = Vec::from(&2_u16.to_be_bytes()[..]);
    request.extend_from_slice(b"cr.txt\0netascii\0");
    client
        .send_to(&request, listener_address)
        .await
        .expect("WRQ");

    let mut buffer = [0_u8; 616];
    let (_, from) = tokio::time::timeout(Duration::from_secs(2), client.recv_from(&mut buffer))
        .await
        .expect("ACK 0 timeout")
        .expect("datagram");
    assert_eq!(&buffer[..4], &[0, 4, 0, 0]);

    // A short final block whose last byte is a lone CR: the decoder cannot
    // resolve it until it knows nothing more is coming.
    let mut data = vec![0, 3, 0, 1];
    data.extend_from_slice(b"line\r");
    client.send_to(&data, from).await.expect("DATA 1");

    tokio::time::timeout(Duration::from_secs(2), client.recv_from(&mut buffer))
        .await
        .expect("final ACK timeout")
        .expect("datagram");

    let uploaded = dir.path().join("cr.txt");
    let body = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if let Ok(body) = tokio::fs::read(&uploaded).await {
                return body;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the upload must be promoted");

    assert_eq!(body, b"line\r", "the trailing CR must not be dropped");

    shutdown_tx.send(true).expect("shutdown signal");
    server.await.expect("server task").expect("server result");
}

#[tokio::test]
async fn a_mid_transfer_timeout_never_repeats_the_oack() {
    let dir = tempfile::tempdir().expect("temporary directory");

    let reservation = UdpSocket::bind("127.0.0.1:0")
        .await
        .expect("reserve test port");
    let listener_address = reservation.local_addr().expect("listener address");
    drop(reservation);

    let (events, mut event_rx) = mpsc::unbounded_channel();
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let server_dir = dir.path().to_path_buf();
    let server = tokio::spawn(async move {
        run(
            listener_address,
            server_dir,
            events,
            shutdown_rx,
            ServerConfig::default(),
        )
        .await
    });

    wait_until_listening(&mut event_rx).await;

    let client = UdpSocket::bind("127.0.0.1:0").await.expect("client socket");
    client
        .send_to(
            &wrq_with_options("wrap.bin", &[("blksize", "8")]),
            listener_address,
        )
        .await
        .expect("WRQ");

    let mut buffer = [0_u8; 256];
    let (_, from) = tokio::time::timeout(Duration::from_secs(2), client.recv_from(&mut buffer))
        .await
        .expect("OACK timeout")
        .expect("datagram");
    assert_eq!(u16::from_be_bytes([buffer[0], buffer[1]]), 6, "OACK first");

    // One full block, so the transfer is under way, then go quiet. The server
    // must now repeat its ACK rather than the OACK, whatever the block number.
    client
        .send_to(
            &[0, 3, 0, 1, b'a', b'b', b'c', b'd', b'e', b'f', b'g', b'h'],
            from,
        )
        .await
        .expect("DATA 1");
    let _ = tokio::time::timeout(Duration::from_secs(2), client.recv_from(&mut buffer))
        .await
        .expect("ACK 1 timeout")
        .expect("datagram");
    assert_eq!(&buffer[..4], &[0, 4, 0, 1]);

    let (_, _) = tokio::time::timeout(Duration::from_secs(3), client.recv_from(&mut buffer))
        .await
        .expect("the server must retry")
        .expect("datagram");
    assert_eq!(
        u16::from_be_bytes([buffer[0], buffer[1]]),
        4,
        "once data has arrived a retransmission is an ACK, never the OACK"
    );

    shutdown_tx.send(true).expect("shutdown signal");
    let _ = server.await;
}

/// Start a server on a free loopback port and wait until it is listening.
async fn start(
    dir: &std::path::Path,
    config: ServerConfig,
) -> (
    SocketAddr,
    watch::Sender<bool>,
    tokio::task::JoinHandle<anyhow::Result<()>>,
    mpsc::UnboundedReceiver<ServerEvent>,
) {
    let reservation = UdpSocket::bind("127.0.0.1:0")
        .await
        .expect("reserve test port");
    let listener_address = reservation.local_addr().expect("listener address");
    drop(reservation);

    let (events, mut event_rx) = mpsc::unbounded_channel();
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let server_dir = dir.to_path_buf();
    let server = tokio::spawn(async move {
        run(listener_address, server_dir, events, shutdown_rx, config).await
    });

    wait_until_listening(&mut event_rx).await;
    (listener_address, shutdown_tx, server, event_rx)
}

#[tokio::test]
async fn a_duplicate_ack_does_not_resend_the_next_block() {
    let dir = tempfile::tempdir().expect("temporary directory");
    tokio::fs::write(dir.path().join("three.bin"), vec![5_u8; 1300])
        .await
        .expect("test file");
    let (listener_address, shutdown_tx, server, _events) =
        start(dir.path(), ServerConfig::default()).await;

    let client = UdpSocket::bind("127.0.0.1:0").await.expect("client socket");
    client
        .send_to(&rrq("three.bin"), listener_address)
        .await
        .expect("RRQ");

    let mut buffer = [0_u8; 616];
    let (_, from) = tokio::time::timeout(Duration::from_secs(2), client.recv_from(&mut buffer))
        .await
        .expect("DATA 1 timeout")
        .expect("datagram");
    assert_eq!(&buffer[..4], &[0, 3, 0, 1]);

    // The ACK for block 1 arrives twice, as it does when it was only late and
    // the client answered a retransmitted DATA 1 as well.
    client.send_to(&[0, 4, 0, 1], from).await.expect("ACK 1");
    client
        .send_to(&[0, 4, 0, 1], from)
        .await
        .expect("ACK 1 again");

    tokio::time::timeout(Duration::from_secs(2), client.recv_from(&mut buffer))
        .await
        .expect("DATA 2 timeout")
        .expect("datagram");
    assert_eq!(&buffer[..4], &[0, 3, 0, 2]);

    // Only a timeout may resend a block. Well inside the 500 ms timeout,
    // nothing more should arrive.
    let again =
        tokio::time::timeout(Duration::from_millis(250), client.recv_from(&mut buffer)).await;
    assert!(
        again.is_err(),
        "a duplicate ACK made the server send block {} again",
        u16::from_be_bytes([buffer[2], buffer[3]])
    );

    client.send_to(&[0, 4, 0, 2], from).await.expect("ACK 2");
    let (n, _) = tokio::time::timeout(Duration::from_secs(2), client.recv_from(&mut buffer))
        .await
        .expect("DATA 3 timeout")
        .expect("datagram");
    assert_eq!(&buffer[..4], &[0, 3, 0, 3]);
    assert_eq!(n, 4 + 276);
    client.send_to(&[0, 4, 0, 3], from).await.expect("ACK 3");

    shutdown_tx.send(true).expect("shutdown signal");
    server.await.expect("server task").expect("server result");
}

#[tokio::test]
async fn an_oversized_datagram_does_not_stop_the_listener() {
    let dir = tempfile::tempdir().expect("temporary directory");
    tokio::fs::write(dir.path().join("small.bin"), b"still here")
        .await
        .expect("test file");
    let (listener_address, shutdown_tx, server, _events) =
        start(dir.path(), ServerConfig::default()).await;

    // As large as this host will send. Linux and Windows take a full UDP
    // payload; macOS caps a datagram at net.inet.udp.maxdgram.
    let client = UdpSocket::bind("127.0.0.1:0").await.expect("client socket");
    let mut sent = false;
    for size in [65_507_usize, 16_384, 9_216, 8_192] {
        if client
            .send_to(&vec![0_u8; size], listener_address)
            .await
            .is_ok()
        {
            sent = true;
            break;
        }
    }
    assert!(sent, "no oversized datagram could be sent");

    client
        .send_to(&rrq("small.bin"), listener_address)
        .await
        .expect("RRQ");
    // The junk datagram is answered with an error of its own first.
    let mut buffer = [0_u8; 616];
    let (n, from) = loop {
        let (n, from) = tokio::time::timeout(Duration::from_secs(2), client.recv_from(&mut buffer))
            .await
            .expect("the listener stopped answering")
            .expect("datagram");
        if buffer[..2] != [0, 5] {
            break (n, from);
        }
    };
    assert_eq!(&buffer[..n], b"\0\x03\0\x01still here");
    client.send_to(&[0, 4, 0, 1], from).await.expect("ACK 1");

    shutdown_tx.send(true).expect("shutdown signal");
    server.await.expect("server task").expect("server result");
}

#[tokio::test]
async fn an_upload_that_cannot_be_stored_is_refused_before_it_starts() {
    let dir = tempfile::tempdir().expect("temporary directory");
    // A regular file where the upload needs a directory.
    tokio::fs::write(dir.path().join("plain.txt"), b"not a directory")
        .await
        .expect("test file");
    let (listener_address, shutdown_tx, server, _events) =
        start(dir.path(), ServerConfig::default()).await;

    let client = UdpSocket::bind("127.0.0.1:0").await.expect("client socket");
    let mut request = Vec::from(&2_u16.to_be_bytes()[..]);
    request.extend_from_slice(b"plain.txt/inner.bin\0octet\0");
    client
        .send_to(&request, listener_address)
        .await
        .expect("WRQ");

    let mut buffer = [0_u8; 616];
    tokio::time::timeout(Duration::from_secs(2), client.recv_from(&mut buffer))
        .await
        .expect("the server never answered")
        .expect("datagram");
    assert_eq!(
        &buffer[..4],
        &[0, 5, 0, 2],
        "expected Access violation, not an acknowledgment"
    );

    shutdown_tx.send(true).expect("shutdown signal");
    server.await.expect("server task").expect("server result");
}

#[cfg(unix)]
#[tokio::test]
async fn a_failed_upload_removes_its_own_staging_file() {
    let dir = tempfile::tempdir().expect("temporary directory");
    tokio::fs::write(dir.path().join("other.bin"), b"unrelated")
        .await
        .expect("test file");
    let (listener_address, shutdown_tx, server, _events) =
        start(dir.path(), ServerConfig::default()).await;

    let client = UdpSocket::bind("127.0.0.1:0").await.expect("client socket");
    let mut request = Vec::from(&2_u16.to_be_bytes()[..]);
    request.extend_from_slice(b"new.bin\0octet\0");
    client
        .send_to(&request, listener_address)
        .await
        .expect("WRQ");

    let mut buffer = [0_u8; 616];
    let (_, from) = tokio::time::timeout(Duration::from_secs(2), client.recv_from(&mut buffer))
        .await
        .expect("ACK 0 timeout")
        .expect("datagram");
    assert_eq!(&buffer[..4], &[0, 4, 0, 0]);

    let mut data = vec![0, 3, 0, 1];
    data.extend_from_slice(&[1_u8; 512]);
    client.send_to(&data, from).await.expect("DATA 1");
    tokio::time::timeout(Duration::from_secs(2), client.recv_from(&mut buffer))
        .await
        .expect("ACK 1 timeout")
        .expect("datagram");

    // The name now resolves somewhere else, so working the staging path out
    // again from the filename would no longer find this upload's file.
    std::os::unix::fs::symlink(dir.path().join("other.bin"), dir.path().join("new.bin"))
        .expect("symlink");
    client
        .send_to(b"\0\x05\0\0client gave up\0", from)
        .await
        .expect("ERROR");

    let names = || -> Vec<String> {
        let mut names: Vec<_> = std::fs::read_dir(dir.path())
            .expect("served directory")
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().to_string())
            .collect();
        names.sort();
        names
    };
    let cleaned = tokio::time::timeout(Duration::from_secs(2), async {
        while names().len() != 2 {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await;
    assert!(cleaned.is_ok(), "staging file left behind: {:?}", names());
    assert_eq!(names(), ["new.bin", "other.bin"]);

    shutdown_tx.send(true).expect("shutdown signal");
    server.await.expect("server task").expect("server result");
}

#[tokio::test]
async fn an_upload_that_cannot_be_promoted_is_never_acknowledged_as_complete() {
    let dir = tempfile::tempdir().expect("temporary directory");
    let (listener_address, shutdown_tx, server, _events) = start(
        dir.path(),
        ServerConfig {
            allow_overwrite: false,
            ..ServerConfig::default()
        },
    )
    .await;

    let client = UdpSocket::bind("127.0.0.1:0").await.expect("client socket");
    let mut request = Vec::from(&2_u16.to_be_bytes()[..]);
    request.extend_from_slice(b"late.bin\0octet\0");
    client
        .send_to(&request, listener_address)
        .await
        .expect("WRQ");

    let mut buffer = [0_u8; 616];
    let (_, from) = tokio::time::timeout(Duration::from_secs(2), client.recv_from(&mut buffer))
        .await
        .expect("ACK 0 timeout")
        .expect("datagram");
    assert_eq!(&buffer[..4], &[0, 4, 0, 0]);

    let mut data = vec![0, 3, 0, 1];
    data.extend_from_slice(&[1_u8; 512]);
    client.send_to(&data, from).await.expect("DATA 1");
    tokio::time::timeout(Duration::from_secs(2), client.recv_from(&mut buffer))
        .await
        .expect("ACK 1 timeout")
        .expect("datagram");
    assert_eq!(&buffer[..4], &[0, 4, 0, 1]);

    // Someone else creates the file while the upload is running, so the
    // upload cannot take its name.
    tokio::fs::write(dir.path().join("late.bin"), b"first")
        .await
        .expect("competing file");

    client
        .send_to(b"\0\x03\0\x02last", from)
        .await
        .expect("DATA 2");
    tokio::time::timeout(Duration::from_secs(2), client.recv_from(&mut buffer))
        .await
        .expect("no answer to the last block")
        .expect("datagram");
    assert_eq!(
        &buffer[..4],
        &[0, 5, 0, 6],
        "the client must hear File already exists, not an ACK for a file that was never stored"
    );
    assert_eq!(
        tokio::fs::read(dir.path().join("late.bin"))
            .await
            .expect("competing file"),
        b"first"
    );

    shutdown_tx.send(true).expect("shutdown signal");
    server.await.expect("server task").expect("server result");
}

#[tokio::test]
async fn a_windowed_upload_is_acknowledged_once_stored() {
    let dir = tempfile::tempdir().expect("temporary directory");
    let (listener_address, shutdown_tx, server, _events) = start(
        dir.path(),
        ServerConfig {
            max_window_size: 4,
            ..ServerConfig::default()
        },
    )
    .await;

    let client = UdpSocket::bind("127.0.0.1:0").await.expect("client socket");
    client
        .send_to(
            &wrq_with_options("win.bin", &[("blksize", "8"), ("windowsize", "4")]),
            listener_address,
        )
        .await
        .expect("WRQ");

    let mut buffer = [0_u8; 616];
    let (_, from) = tokio::time::timeout(Duration::from_secs(2), client.recv_from(&mut buffer))
        .await
        .expect("OACK timeout")
        .expect("datagram");
    assert_eq!(&buffer[..2], &[0, 6]);

    // Six blocks: one full window, then a window ending in a short block.
    let mut expected = Vec::new();
    for block in 1_u16..=6 {
        let payload: &[u8] = if block == 6 { b"end" } else { b"12345678" };
        expected.extend_from_slice(payload);
        let mut data = Vec::from(&3_u16.to_be_bytes()[..]);
        data.extend_from_slice(&block.to_be_bytes());
        data.extend_from_slice(payload);
        client.send_to(&data, from).await.expect("DATA");
        if block == 4 || block == 6 {
            tokio::time::timeout(Duration::from_secs(2), client.recv_from(&mut buffer))
                .await
                .expect("window ACK timeout")
                .expect("datagram");
            assert_eq!(&buffer[..2], &[0, 4]);
            assert_eq!(u16::from_be_bytes([buffer[2], buffer[3]]), block);
        }
    }

    // By the time the last block is acknowledged the file is in place.
    assert_eq!(
        tokio::fs::read(dir.path().join("win.bin"))
            .await
            .expect("the upload was acknowledged before it was stored"),
        expected
    );

    shutdown_tx.send(true).expect("shutdown signal");
    server.await.expect("server task").expect("server result");
}

/// Send DATA `block` carrying `payload` to `to`.
async fn send_data(client: &UdpSocket, to: SocketAddr, block: u16, payload: &[u8]) {
    let mut data = Vec::from(&3_u16.to_be_bytes()[..]);
    data.extend_from_slice(&block.to_be_bytes());
    data.extend_from_slice(payload);
    client.send_to(&data, to).await.expect("DATA");
}

/// Wait for an ACK of `block`, skipping any ACK for an earlier one.
async fn expect_ack(client: &UdpSocket, block: u16) {
    let mut buffer = [0_u8; 616];
    loop {
        tokio::time::timeout(Duration::from_secs(2), client.recv_from(&mut buffer))
            .await
            .unwrap_or_else(|_| panic!("no ACK {block}"))
            .expect("datagram");
        assert_eq!(&buffer[..2], &[0, 4], "expected an ACK");
        if u16::from_be_bytes([buffer[2], buffer[3]]) == block {
            return;
        }
    }
}

/// A windowed upload of eight-byte blocks, with a server that gives up after
/// three retries.
async fn start_windowed_upload(
    dir: &std::path::Path,
    filename: &str,
) -> (
    UdpSocket,
    SocketAddr,
    watch::Sender<bool>,
    tokio::task::JoinHandle<anyhow::Result<()>>,
) {
    let (listener_address, shutdown_tx, server, _events) = start(
        dir,
        ServerConfig {
            max_window_size: 8,
            max_retries: 3,
            ..ServerConfig::default()
        },
    )
    .await;
    let client = UdpSocket::bind("127.0.0.1:0").await.expect("client socket");
    client
        .send_to(
            &wrq_with_options(filename, &[("blksize", "8"), ("windowsize", "8")]),
            listener_address,
        )
        .await
        .expect("WRQ");
    let mut buffer = [0_u8; 616];
    let (_, from) = tokio::time::timeout(Duration::from_secs(2), client.recv_from(&mut buffer))
        .await
        .expect("OACK timeout")
        .expect("datagram");
    assert_eq!(&buffer[..2], &[0, 6]);
    (client, from, shutdown_tx, server)
}

#[tokio::test]
async fn a_resent_window_does_not_use_up_the_retries() {
    let dir = tempfile::tempdir().expect("temporary directory");
    let (client, from, shutdown_tx, server) = start_windowed_upload(dir.path(), "resent.bin").await;

    for block in 1..=8 {
        send_data(&client, from, block, b"abcdefgh").await;
    }
    expect_ack(&client, 8).await;

    // The client never saw that ACK and sends the whole window again: eight
    // blocks the server already has, more than its three retries.
    for block in 1..=8 {
        send_data(&client, from, block, b"abcdefgh").await;
    }
    send_data(&client, from, 9, b"end").await;
    expect_ack(&client, 9).await;

    let mut expected = b"abcdefgh".repeat(8);
    expected.extend_from_slice(b"end");
    assert_eq!(
        tokio::fs::read(dir.path().join("resent.bin"))
            .await
            .expect("upload"),
        expected
    );

    shutdown_tx.send(true).expect("shutdown signal");
    server.await.expect("server task").expect("server result");
}

#[tokio::test]
async fn a_lost_block_in_a_window_is_reported_without_waiting_for_a_timeout() {
    let dir = tempfile::tempdir().expect("temporary directory");
    let (client, from, shutdown_tx, server) = start_windowed_upload(dir.path(), "gap.bin").await;

    for block in 1..=8 {
        send_data(&client, from, block, b"abcdefgh").await;
    }
    expect_ack(&client, 8).await;

    // Block 9 is lost. What follows it cannot be stored, and the server says
    // where to restart well before its 500 ms timeout would.
    for block in 10..=16 {
        send_data(&client, from, block, b"abcdefgh").await;
    }
    let mut buffer = [0_u8; 616];
    tokio::time::timeout(Duration::from_millis(250), client.recv_from(&mut buffer))
        .await
        .expect("the gap went unreported")
        .expect("datagram");
    assert_eq!(&buffer[..4], &[0, 4, 0, 8]);

    for block in 9..=16 {
        send_data(&client, from, block, b"abcdefgh").await;
    }
    expect_ack(&client, 16).await;
    send_data(&client, from, 17, b"").await;
    expect_ack(&client, 17).await;

    assert_eq!(
        tokio::fs::read(dir.path().join("gap.bin"))
            .await
            .expect("upload"),
        b"abcdefgh".repeat(16)
    );

    shutdown_tx.send(true).expect("shutdown signal");
    server.await.expect("server task").expect("server result");
}

#[tokio::test]
async fn settings_no_transfer_could_use_are_refused() {
    let dir = tempfile::tempdir().expect("temporary directory");
    for config in [
        ServerConfig {
            timeout_ms: 0,
            ..ServerConfig::default()
        },
        ServerConfig {
            max_block_size: 4,
            ..ServerConfig::default()
        },
    ] {
        assert!(config.validate().is_err());
        let (events, _event_rx) = mpsc::unbounded_channel();
        let (_shutdown_tx, shutdown_rx) = watch::channel(false);
        let result = tokio::time::timeout(
            Duration::from_secs(2),
            run(
                "127.0.0.1:0".parse().expect("address"),
                dir.path().to_path_buf(),
                events,
                shutdown_rx,
                config,
            ),
        )
        .await
        .expect("run must return at once");
        assert!(result.is_err());
    }
    assert!(ServerConfig::default().validate().is_ok());
}

#[tokio::test]
async fn a_duplicate_block_does_not_cut_a_window_short() {
    let dir = tempfile::tempdir().expect("temporary directory");
    let (client, from, shutdown_tx, server) = start_windowed_upload(dir.path(), "dup.bin").await;

    for block in 1..=8 {
        send_data(&client, from, block, b"abcdefgh").await;
    }
    expect_ack(&client, 8).await;

    // A copy of a block stored long ago turns up mid-window. Acknowledging
    // early would send the client back to blocks it has already sent.
    send_data(&client, from, 9, b"abcdefgh").await;
    send_data(&client, from, 10, b"abcdefgh").await;
    send_data(&client, from, 3, b"abcdefgh").await;
    for block in 11..=16 {
        send_data(&client, from, block, b"abcdefgh").await;
    }
    let mut buffer = [0_u8; 616];
    tokio::time::timeout(Duration::from_secs(2), client.recv_from(&mut buffer))
        .await
        .expect("no ACK")
        .expect("datagram");
    assert_eq!(&buffer[..4], &[0, 4, 0, 16], "the window was cut short");

    send_data(&client, from, 17, b"").await;
    expect_ack(&client, 17).await;
    assert_eq!(
        tokio::fs::read(dir.path().join("dup.bin"))
            .await
            .expect("upload"),
        b"abcdefgh".repeat(16)
    );

    shutdown_tx.send(true).expect("shutdown signal");
    server.await.expect("server task").expect("server result");
}

#[tokio::test]
async fn resent_blocks_cannot_put_off_the_repeated_ack() {
    let dir = tempfile::tempdir().expect("temporary directory");
    let (client, from, shutdown_tx, server) = start_windowed_upload(dir.path(), "late.bin").await;

    for block in 1..=8 {
        send_data(&client, from, block, b"abcdefgh").await;
    }
    expect_ack(&client, 8).await;

    // The client never saw ACK 8 and keeps resending its window, faster than
    // the server's 500 ms timeout. The ACK still has to come round again.
    let mut buffer = [0_u8; 616];
    let repeated = tokio::time::timeout(Duration::from_millis(1500), async {
        loop {
            send_data(&client, from, 1, b"abcdefgh").await;
            if let Ok(Ok(_)) =
                tokio::time::timeout(Duration::from_millis(100), client.recv_from(&mut buffer))
                    .await
            {
                return u16::from_be_bytes([buffer[2], buffer[3]]);
            }
        }
    })
    .await
    .expect("the ACK was never repeated");
    assert_eq!(repeated, 8);

    send_data(&client, from, 9, b"").await;
    expect_ack(&client, 9).await;

    shutdown_tx.send(true).expect("shutdown signal");
    server.await.expect("server task").expect("server result");
}

#[tokio::test]
async fn a_promoted_upload_leaves_its_staging_name_alone() {
    let dir = tempfile::tempdir().expect("temporary directory");
    let (listener_address, shutdown_tx, server, _events) =
        start(dir.path(), ServerConfig::default()).await;

    let client = UdpSocket::bind("127.0.0.1:0").await.expect("client socket");
    let mut request = Vec::from(&2_u16.to_be_bytes()[..]);
    request.extend_from_slice(b"fw.bin\0octet\0");
    client
        .send_to(&request, listener_address)
        .await
        .expect("WRQ");
    let mut buffer = [0_u8; 616];
    let (_, from) = tokio::time::timeout(Duration::from_secs(2), client.recv_from(&mut buffer))
        .await
        .expect("ACK 0 timeout")
        .expect("datagram");
    send_data(&client, from, 1, b"short").await;
    expect_ack(&client, 1).await;

    // The upload is in place and the server is still dallying. Something
    // else now uses the staging name; the first transfer is number 1.
    let reused = dir.path().join("fw.bin.1.part");
    tokio::fs::write(&reused, b"someone else's")
        .await
        .expect("file at the staging name");
    tokio::time::sleep(Duration::from_millis(1500)).await;

    assert!(
        reused.exists(),
        "the finished transfer removed a file it does not own"
    );
    assert_eq!(
        tokio::fs::read(dir.path().join("fw.bin"))
            .await
            .expect("upload"),
        b"short"
    );

    shutdown_tx.send(true).expect("shutdown signal");
    server.await.expect("server task").expect("server result");
}

/// Receive `count` DATA packets and return their block numbers.
async fn receive_blocks(client: &UdpSocket, count: usize) -> Vec<u16> {
    let mut buffer = [0_u8; 616];
    let mut blocks = Vec::new();
    for _ in 0..count {
        tokio::time::timeout(Duration::from_secs(2), client.recv_from(&mut buffer))
            .await
            .expect("DATA timeout")
            .expect("datagram");
        assert_eq!(&buffer[..2], &[0, 3]);
        blocks.push(u16::from_be_bytes([buffer[2], buffer[3]]));
    }
    blocks
}

#[tokio::test]
async fn a_late_window_ack_does_not_resend_the_next_window() {
    let dir = tempfile::tempdir().expect("temporary directory");
    // Twelve full eight-byte blocks, then the empty block that ends it.
    tokio::fs::write(dir.path().join("win.bin"), b"abcdefgh".repeat(12))
        .await
        .expect("test file");
    let (listener_address, shutdown_tx, server, _events) = start(
        dir.path(),
        ServerConfig {
            max_window_size: 4,
            ..ServerConfig::default()
        },
    )
    .await;

    let client = UdpSocket::bind("127.0.0.1:0").await.expect("client socket");
    client
        .send_to(
            &rrq_with_options("win.bin", &[("blksize", "8"), ("windowsize", "4")]),
            listener_address,
        )
        .await
        .expect("RRQ");
    let mut buffer = [0_u8; 616];
    let (_, from) = tokio::time::timeout(Duration::from_secs(2), client.recv_from(&mut buffer))
        .await
        .expect("OACK timeout")
        .expect("datagram");
    assert_eq!(&buffer[..2], &[0, 6]);
    client.send_to(&[0, 4, 0, 0], from).await.expect("ACK 0");

    // The first window, then the same window again once the server's
    // timeout passes without an ACK.
    assert_eq!(receive_blocks(&client, 4).await, [1, 2, 3, 4]);
    assert_eq!(receive_blocks(&client, 4).await, [1, 2, 3, 4]);
    // One ACK for each copy, as a client that answers every window does.
    client.send_to(&[0, 4, 0, 4], from).await.expect("ACK 4");
    client
        .send_to(&[0, 4, 0, 4], from)
        .await
        .expect("ACK 4 again");

    assert_eq!(receive_blocks(&client, 4).await, [5, 6, 7, 8]);
    let again =
        tokio::time::timeout(Duration::from_millis(250), client.recv_from(&mut buffer)).await;
    assert!(
        again.is_err(),
        "the second ACK 4 had block {} sent again",
        u16::from_be_bytes([buffer[2], buffer[3]])
    );

    client.send_to(&[0, 4, 0, 8], from).await.expect("ACK 8");
    assert_eq!(receive_blocks(&client, 4).await, [9, 10, 11, 12]);
    client.send_to(&[0, 4, 0, 12], from).await.expect("ACK 12");
    assert_eq!(receive_blocks(&client, 1).await, [13]);
    client.send_to(&[0, 4, 0, 13], from).await.expect("ACK 13");

    shutdown_tx.send(true).expect("shutdown signal");
    server.await.expect("server task").expect("server result");
}

#[tokio::test]
async fn a_late_ack_to_the_oack_does_not_resend_the_first_window() {
    let dir = tempfile::tempdir().expect("temporary directory");
    tokio::fs::write(dir.path().join("win.bin"), b"abcdefgh".repeat(7))
        .await
        .expect("test file");
    let (listener_address, shutdown_tx, server, _events) = start(
        dir.path(),
        ServerConfig {
            max_window_size: 4,
            ..ServerConfig::default()
        },
    )
    .await;

    let client = UdpSocket::bind("127.0.0.1:0").await.expect("client socket");
    client
        .send_to(
            &rrq_with_options("win.bin", &[("blksize", "8"), ("windowsize", "4")]),
            listener_address,
        )
        .await
        .expect("RRQ");
    let mut buffer = [0_u8; 616];
    // The OACK, and the same OACK again once the timeout passes.
    let mut from = None;
    for _ in 0..2 {
        let (_, sender) =
            tokio::time::timeout(Duration::from_secs(2), client.recv_from(&mut buffer))
                .await
                .expect("OACK timeout")
                .expect("datagram");
        assert_eq!(&buffer[..2], &[0, 6]);
        from = Some(sender);
    }
    let from = from.expect("server address");
    client.send_to(&[0, 4, 0, 0], from).await.expect("ACK 0");
    client
        .send_to(&[0, 4, 0, 0], from)
        .await
        .expect("ACK 0 again");

    assert_eq!(receive_blocks(&client, 4).await, [1, 2, 3, 4]);
    let again =
        tokio::time::timeout(Duration::from_millis(250), client.recv_from(&mut buffer)).await;
    assert!(
        again.is_err(),
        "the second ACK 0 had block {} sent again",
        u16::from_be_bytes([buffer[2], buffer[3]])
    );

    client.send_to(&[0, 4, 0, 4], from).await.expect("ACK 4");
    assert_eq!(receive_blocks(&client, 4).await, [5, 6, 7, 8]);
    client.send_to(&[0, 4, 0, 8], from).await.expect("ACK 8");

    shutdown_tx.send(true).expect("shutdown signal");
    server.await.expect("server task").expect("server result");
}

#[tokio::test]
async fn stray_packets_do_not_draw_copies_of_the_oack() {
    let dir = tempfile::tempdir().expect("temporary directory");
    tokio::fs::write(dir.path().join("win.bin"), b"abcdefgh".repeat(3))
        .await
        .expect("test file");
    let (listener_address, shutdown_tx, server, _events) = start(
        dir.path(),
        ServerConfig {
            max_window_size: 4,
            ..ServerConfig::default()
        },
    )
    .await;

    let client = UdpSocket::bind("127.0.0.1:0").await.expect("client socket");
    client
        .send_to(
            &rrq_with_options("win.bin", &[("blksize", "8"), ("windowsize", "4")]),
            listener_address,
        )
        .await
        .expect("RRQ");
    let mut buffer = [0_u8; 616];
    let (_, from) = tokio::time::timeout(Duration::from_secs(2), client.recv_from(&mut buffer))
        .await
        .expect("OACK timeout")
        .expect("datagram");
    assert_eq!(&buffer[..2], &[0, 6]);

    // Packets that are not ACK 0, well inside the server's 500 ms timeout.
    for block in [5_u16, 9, 2] {
        let mut ack = vec![0, 4];
        ack.extend_from_slice(&block.to_be_bytes());
        client.send_to(&ack, from).await.expect("stray ACK");
    }
    let copy =
        tokio::time::timeout(Duration::from_millis(250), client.recv_from(&mut buffer)).await;
    assert!(copy.is_err(), "a stray packet was answered with the OACK");

    client.send_to(&[0, 4, 0, 0], from).await.expect("ACK 0");
    assert_eq!(receive_blocks(&client, 4).await, [1, 2, 3, 4]);
    client.send_to(&[0, 4, 0, 4], from).await.expect("ACK 4");

    shutdown_tx.send(true).expect("shutdown signal");
    server.await.expect("server task").expect("server result");
}

#[tokio::test]
async fn a_staging_name_can_be_neither_read_nor_written() {
    let dir = tempfile::tempdir().expect("temporary directory");
    // As if another client's upload of fw.bin, transfer 7, were under way.
    tokio::fs::write(dir.path().join("fw.bin.7.part"), b"half an upload")
        .await
        .expect("staging file");
    let (listener_address, shutdown_tx, server, _events) =
        start(dir.path(), ServerConfig::default()).await;

    let client = UdpSocket::bind("127.0.0.1:0").await.expect("client socket");
    let mut buffer = [0_u8; 616];

    client
        .send_to(&rrq("fw.bin.7.part"), listener_address)
        .await
        .expect("RRQ");
    tokio::time::timeout(Duration::from_secs(2), client.recv_from(&mut buffer))
        .await
        .expect("no answer to the RRQ")
        .expect("datagram");
    assert_eq!(&buffer[..4], &[0, 5, 0, 1], "expected File not found");

    let mut request = Vec::from(&2_u16.to_be_bytes()[..]);
    request.extend_from_slice(b"fw.bin.7.part\0octet\0");
    client
        .send_to(&request, listener_address)
        .await
        .expect("WRQ");
    tokio::time::timeout(Duration::from_secs(2), client.recv_from(&mut buffer))
        .await
        .expect("no answer to the WRQ")
        .expect("datagram");
    assert_eq!(&buffer[..4], &[0, 5, 0, 2], "expected Access violation");

    // The same name to a filesystem that ignores case, and a new one: the
    // next upload's staging name, guessed before that upload starts.
    let mut request = Vec::from(&2_u16.to_be_bytes()[..]);
    request.extend_from_slice(b"fw.bin.2.PART\0octet\0");
    client
        .send_to(&request, listener_address)
        .await
        .expect("WRQ");
    tokio::time::timeout(Duration::from_secs(2), client.recv_from(&mut buffer))
        .await
        .expect("no answer to the WRQ")
        .expect("datagram");
    assert_eq!(&buffer[..4], &[0, 5, 0, 2], "expected Access violation");

    assert_eq!(
        tokio::fs::read(dir.path().join("fw.bin.7.part"))
            .await
            .expect("staging file"),
        b"half an upload"
    );

    shutdown_tx.send(true).expect("shutdown signal");
    server.await.expect("server task").expect("server result");
}
