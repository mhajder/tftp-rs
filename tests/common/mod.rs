//! Client-side helpers shared by the test files: TFTP packet builders and
//! parsers, and a way to start the real server on a free port.

// Each test file uses a different subset of these.
#![allow(dead_code)]

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::Path;
use std::time::Duration;

use tftp_rs::server::{ServerConfig, ServerEvent, run};
use tokio::net::UdpSocket;
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;

/// How long any single wait in a test may take before it counts as a hang.
pub const WAIT: Duration = Duration::from_secs(2);

/// A request packet: `opcode` 1 is RRQ, 2 is WRQ.
pub fn request(opcode: u16, filename: &str, mode: &str, options: &[(&str, &str)]) -> Vec<u8> {
    let mut packet = Vec::from(&opcode.to_be_bytes()[..]);
    packet.extend_from_slice(filename.as_bytes());
    packet.push(0);
    packet.extend_from_slice(mode.as_bytes());
    packet.push(0);
    for (key, value) in options {
        packet.extend_from_slice(key.as_bytes());
        packet.push(0);
        packet.extend_from_slice(value.as_bytes());
        packet.push(0);
    }
    packet
}

/// An octet RRQ without options.
pub fn rrq(filename: &str) -> Vec<u8> {
    request(1, filename, "octet", &[])
}

/// An octet RRQ with options.
pub fn rrq_with_options(filename: &str, options: &[(&str, &str)]) -> Vec<u8> {
    request(1, filename, "octet", options)
}

/// An octet WRQ without options.
pub fn wrq(filename: &str) -> Vec<u8> {
    request(2, filename, "octet", &[])
}

/// An octet WRQ with options.
pub fn wrq_with_options(filename: &str, options: &[(&str, &str)]) -> Vec<u8> {
    request(2, filename, "octet", options)
}

/// An ACK packet.
pub fn ack(block: u16) -> Vec<u8> {
    let mut packet = Vec::from(&4_u16.to_be_bytes()[..]);
    packet.extend_from_slice(&block.to_be_bytes());
    packet
}

/// A DATA packet.
pub fn data(block: u16, payload: &[u8]) -> Vec<u8> {
    let mut packet = Vec::from(&3_u16.to_be_bytes()[..]);
    packet.extend_from_slice(&block.to_be_bytes());
    packet.extend_from_slice(payload);
    packet
}

/// An ERROR packet.
pub fn error(code: u16, message: &str) -> Vec<u8> {
    let mut packet = Vec::from(&5_u16.to_be_bytes()[..]);
    packet.extend_from_slice(&code.to_be_bytes());
    packet.extend_from_slice(message.as_bytes());
    packet.push(0);
    packet
}

/// The opcode of a packet.
pub fn opcode(packet: &[u8]) -> u16 {
    u16::from_be_bytes([packet[0], packet[1]])
}

/// The block number of a DATA or ACK packet, or the code of an ERROR.
pub fn block(packet: &[u8]) -> u16 {
    u16::from_be_bytes([packet[2], packet[3]])
}

/// The options of an OACK packet.
pub fn oack_options(packet: &[u8]) -> HashMap<String, String> {
    let fields: Vec<String> = packet[2..]
        .split(|&b| b == 0)
        .filter(|field| !field.is_empty())
        .map(|field| String::from_utf8_lossy(field).into_owned())
        .collect();
    fields
        .chunks(2)
        .filter(|pair| pair.len() == 2)
        .map(|pair| (pair[0].clone(), pair[1].clone()))
        .collect()
}

/// Wait for the server's "Listening on" log line and return the address it
/// is listening on.
///
/// The server logs the address its socket actually got, so a test can bind
/// port 0 and learn the port here, instead of reserving a port by binding and
/// dropping it, which another process can take in between.
pub async fn wait_until_listening(events: &mut mpsc::UnboundedReceiver<ServerEvent>) -> SocketAddr {
    loop {
        let event = tokio::time::timeout(WAIT, events.recv())
            .await
            .expect("server startup timeout")
            .expect("server event channel");
        if let ServerEvent::Log(message) = event
            && let Some(rest) = message.strip_prefix("Listening on ")
        {
            // "Listening on 127.0.0.1:1234", optionally followed by
            // " via interface lo0".
            let address = rest.split(' ').next().expect("listener address");
            return address.parse().expect("listener address");
        }
    }
}

/// Start the server on a free loopback port and wait until it is listening.
pub async fn start(
    dir: &Path,
    config: ServerConfig,
) -> (
    SocketAddr,
    watch::Sender<bool>,
    JoinHandle<anyhow::Result<()>>,
    mpsc::UnboundedReceiver<ServerEvent>,
) {
    let (events, mut event_rx) = mpsc::unbounded_channel();
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let server_dir = dir.to_path_buf();
    let server = tokio::spawn(async move {
        run(
            SocketAddr::from(([127, 0, 0, 1], 0)),
            server_dir,
            events,
            shutdown_rx,
            config,
        )
        .await
    });

    let listener_address = wait_until_listening(&mut event_rx).await;
    (listener_address, shutdown_tx, server, event_rx)
}

/// Wait for the first event that `matches` accepts, skipping the others.
pub async fn next_event(
    events: &mut mpsc::UnboundedReceiver<ServerEvent>,
    matches: impl Fn(&ServerEvent) -> bool,
) -> ServerEvent {
    loop {
        let event = tokio::time::timeout(WAIT, events.recv())
            .await
            .expect("no matching server event")
            .expect("server event channel");
        if matches(&event) {
            return event;
        }
    }
}

/// Wait for a log line containing `text`.
pub async fn expect_log(events: &mut mpsc::UnboundedReceiver<ServerEvent>, text: &str) -> String {
    match next_event(
        events,
        |event| matches!(event, ServerEvent::Log(line) if line.contains(text)),
    )
    .await
    {
        ServerEvent::Log(line) => line,
        _ => unreachable!(),
    }
}

/// Receive one datagram, failing the test if none arrives in time.
pub async fn recv(client: &UdpSocket) -> (Vec<u8>, SocketAddr) {
    let mut buffer = vec![0_u8; 70_000];
    let (length, from) = tokio::time::timeout(WAIT, client.recv_from(&mut buffer))
        .await
        .expect("no datagram from the server")
        .expect("datagram");
    buffer.truncate(length);
    (buffer, from)
}

/// Whether a datagram arrives within `within`.
pub async fn nothing_arrives(client: &UdpSocket, within: Duration) -> bool {
    let mut buffer = [0_u8; 70_000];
    tokio::time::timeout(within, client.recv_from(&mut buffer))
        .await
        .is_err()
}

/// Send a DATA packet.
pub async fn send_data(client: &UdpSocket, to: SocketAddr, block: u16, payload: &[u8]) {
    client
        .send_to(&data(block, payload), to)
        .await
        .expect("DATA");
}

/// Wait for an ACK of `block`, skipping any ACK for an earlier one.
pub async fn expect_ack(client: &UdpSocket, block: u16) {
    let mut buffer = [0_u8; 616];
    loop {
        tokio::time::timeout(WAIT, client.recv_from(&mut buffer))
            .await
            .unwrap_or_else(|_| panic!("no ACK {block}"))
            .expect("datagram");
        assert_eq!(&buffer[..2], &[0, 4], "expected an ACK");
        if u16::from_be_bytes([buffer[2], buffer[3]]) == block {
            return;
        }
    }
}

/// Receive `count` DATA packets and return their block numbers.
pub async fn receive_blocks(client: &UdpSocket, count: usize) -> Vec<u16> {
    let mut buffer = [0_u8; 616];
    let mut blocks = Vec::new();
    for _ in 0..count {
        tokio::time::timeout(WAIT, client.recv_from(&mut buffer))
            .await
            .expect("DATA timeout")
            .expect("datagram");
        assert_eq!(&buffer[..2], &[0, 3]);
        blocks.push(u16::from_be_bytes([buffer[2], buffer[3]]));
    }
    blocks
}
