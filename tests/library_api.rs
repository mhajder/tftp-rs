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
