use std::collections::HashMap;

use anyhow::{Result, anyhow};

/// TFTP opcodes per RFC 1350 + RFC 2347.
const OPCODE_RRQ: u16 = 1;
const OPCODE_WRQ: u16 = 2;
const OPCODE_DATA: u16 = 3;
const OPCODE_ACK: u16 = 4;
const OPCODE_ERROR: u16 = 5;
const OPCODE_OACK: u16 = 6;

/// Default data payload per DATA packet (RFC 1350).
pub const BLOCK_SIZE: usize = 512;

/// Maximum negotiable blksize (largest payload that fits in a UDP datagram
/// with standard IP + UDP headers: 65535 - 20 - 8 - 4 = 65503, but the
/// common convention is 65464).
pub const MAX_BLKSIZE: usize = 65464;

/// Default window size (RFC 7440). A window of 1 behaves like classic TFTP.
pub const DEFAULT_WINDOWSIZE: u16 = 1;

/// Minimum timeout value (seconds) per RFC 2349.
pub const MIN_TIMEOUT: u8 = 1;

/// Maximum timeout value (seconds) per RFC 2349.
pub const MAX_TIMEOUT: u8 = 255;

/// A fully parsed TFTP packet.
#[derive(Debug, Clone)]
#[allow(clippy::upper_case_acronyms)]
pub enum Packet {
    RRQ {
        filename: String,
        mode: String,
        options: HashMap<String, String>,
    },
    WRQ {
        filename: String,
        mode: String,
        options: HashMap<String, String>,
    },
    DATA {
        block_num: u16,
        data: Vec<u8>,
    },
    ACK {
        block_num: u16,
    },
    ERROR {
        code: u16,
        msg: String,
    },
    /// Option Acknowledgment (RFC 2347).
    OACK {
        options: HashMap<String, String>,
    },
}

impl Packet {
    /// Parse raw bytes into a `Packet`.
    pub fn from_bytes(buf: &[u8]) -> Result<Self> {
        if buf.len() < 2 {
            return Err(anyhow!("packet too short"));
        }
        let opcode = u16::from_be_bytes([buf[0], buf[1]]);
        match opcode {
            OPCODE_RRQ => parse_request(buf, true),
            OPCODE_WRQ => parse_request(buf, false),
            OPCODE_DATA => parse_data(buf),
            OPCODE_ACK => parse_ack(buf),
            OPCODE_ERROR => parse_error(buf),
            OPCODE_OACK => parse_oack(buf),
            _ => Err(anyhow!("unknown opcode {opcode}")),
        }
    }

    /// Serialize the packet to bytes for transmission.
    pub fn to_bytes(&self) -> Vec<u8> {
        match self {
            Packet::RRQ {
                filename,
                mode,
                options,
            } => encode_request(OPCODE_RRQ, filename, mode, options),
            Packet::WRQ {
                filename,
                mode,
                options,
            } => encode_request(OPCODE_WRQ, filename, mode, options),
            Packet::DATA { block_num, data } => {
                let mut buf = Vec::with_capacity(4 + data.len());
                buf.extend_from_slice(&OPCODE_DATA.to_be_bytes());
                buf.extend_from_slice(&block_num.to_be_bytes());
                buf.extend_from_slice(data);
                buf
            }
            Packet::ACK { block_num } => {
                let mut buf = Vec::with_capacity(4);
                buf.extend_from_slice(&OPCODE_ACK.to_be_bytes());
                buf.extend_from_slice(&block_num.to_be_bytes());
                buf
            }
            Packet::ERROR { code, msg } => {
                let mut buf = Vec::with_capacity(5 + msg.len());
                buf.extend_from_slice(&OPCODE_ERROR.to_be_bytes());
                buf.extend_from_slice(&code.to_be_bytes());
                buf.extend_from_slice(msg.as_bytes());
                buf.push(0);
                buf
            }
            Packet::OACK { options } => {
                let mut buf = Vec::new();
                buf.extend_from_slice(&OPCODE_OACK.to_be_bytes());
                for (key, val) in options {
                    buf.extend_from_slice(key.as_bytes());
                    buf.push(0);
                    buf.extend_from_slice(val.as_bytes());
                    buf.push(0);
                }
                buf
            }
        }
    }

    /// Build an ERROR packet from a numeric code.
    #[cfg(test)]
    pub fn error(code: u16, msg: &str) -> Self {
        Packet::ERROR {
            code,
            msg: msg.to_string(),
        }
    }
}

// ---------------------------------------------------------------------------
// Internal parsing helpers
// ---------------------------------------------------------------------------

/// Parse RRQ / WRQ: 2‑byte opcode | filename\0 | mode\0 [| option\0 | value\0 ]*
fn parse_request(buf: &[u8], is_rrq: bool) -> Result<Packet> {
    let payload = &buf[2..];
    let fields: Vec<&[u8]> = payload.split(|&b| b == 0).collect();

    if fields.len() < 2 {
        return Err(anyhow!("missing filename or mode"));
    }

    let filename = String::from_utf8(fields[0].to_vec())?;
    let mode = String::from_utf8(fields[1].to_vec())?.to_ascii_lowercase();

    if filename.is_empty() {
        return Err(anyhow!("empty filename"));
    }

    // Parse RFC 2347 options (key-value pairs after mode).
    let mut options = HashMap::new();
    let mut i = 2;
    while i + 1 < fields.len() {
        let key = String::from_utf8(fields[i].to_vec())?.to_ascii_lowercase();
        let val = String::from_utf8(fields[i + 1].to_vec())?;
        if !key.is_empty() {
            options.insert(key, val);
        }
        i += 2;
    }

    if is_rrq {
        Ok(Packet::RRQ {
            filename,
            mode,
            options,
        })
    } else {
        Ok(Packet::WRQ {
            filename,
            mode,
            options,
        })
    }
}

/// Parse DATA: 2‑byte opcode | 2‑byte block# | 0‥N bytes
fn parse_data(buf: &[u8]) -> Result<Packet> {
    if buf.len() < 4 {
        return Err(anyhow!("DATA packet too short"));
    }
    let block_num = u16::from_be_bytes([buf[2], buf[3]]);
    let data = buf[4..].to_vec();
    Ok(Packet::DATA { block_num, data })
}

/// Parse ACK: 2‑byte opcode | 2‑byte block#
fn parse_ack(buf: &[u8]) -> Result<Packet> {
    if buf.len() < 4 {
        return Err(anyhow!("ACK packet too short"));
    }
    let block_num = u16::from_be_bytes([buf[2], buf[3]]);
    Ok(Packet::ACK { block_num })
}

/// Parse ERROR: 2‑byte opcode | 2‑byte code | msg\0
fn parse_error(buf: &[u8]) -> Result<Packet> {
    if buf.len() < 5 {
        return Err(anyhow!("ERROR packet too short"));
    }
    let code = u16::from_be_bytes([buf[2], buf[3]]);
    let msg_bytes = &buf[4..];
    // Strip trailing NUL if present.
    let end = msg_bytes
        .iter()
        .position(|&b| b == 0)
        .unwrap_or(msg_bytes.len());
    let msg = String::from_utf8_lossy(&msg_bytes[..end]).to_string();
    Ok(Packet::ERROR { code, msg })
}

/// Parse OACK: 2‑byte opcode | [option\0 | value\0]*
fn parse_oack(buf: &[u8]) -> Result<Packet> {
    let payload = &buf[2..];
    let fields: Vec<&[u8]> = payload.split(|&b| b == 0).collect();
    let mut options = HashMap::new();
    let mut i = 0;
    while i + 1 < fields.len() {
        let key = String::from_utf8(fields[i].to_vec())?.to_ascii_lowercase();
        let val = String::from_utf8(fields[i + 1].to_vec())?;
        if !key.is_empty() {
            options.insert(key, val);
        }
        i += 2;
    }
    Ok(Packet::OACK { options })
}

fn encode_request(
    opcode: u16,
    filename: &str,
    mode: &str,
    options: &HashMap<String, String>,
) -> Vec<u8> {
    let mut buf = Vec::with_capacity(4 + filename.len() + mode.len());
    buf.extend_from_slice(&opcode.to_be_bytes());
    buf.extend_from_slice(filename.as_bytes());
    buf.push(0);
    buf.extend_from_slice(mode.as_bytes());
    buf.push(0);
    for (key, val) in options {
        buf.extend_from_slice(key.as_bytes());
        buf.push(0);
        buf.extend_from_slice(val.as_bytes());
        buf.push(0);
    }
    buf
}

// ---------------------------------------------------------------------------
// Netascii conversion (RFC 1350)
// ---------------------------------------------------------------------------

/// State tracker for netascii-to-octet conversion across block boundaries.
/// Netascii encodes: `\r\n` → LF, `\r\0` → CR. A bare `\r` at the end of a
/// block might be followed by `\n` or `\0` in the next block, so we must
/// remember the trailing `\r`.
#[derive(Debug, Default)]
pub struct NetasciiDecoder {
    /// True if the previous block ended with `\r` that hasn't been resolved yet.
    pub pending_cr: bool,
}

impl NetasciiDecoder {
    pub fn new() -> Self {
        Self { pending_cr: false }
    }

    /// Convert a netascii-encoded chunk to octet (binary) data.
    /// Updates internal state to handle `\r` spanning block boundaries.
    pub fn decode(&mut self, input: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(input.len());
        let mut i = 0;

        // Handle pending CR from previous block.
        if self.pending_cr {
            self.pending_cr = false;
            if !input.is_empty() {
                match input[0] {
                    b'\n' => {
                        out.push(b'\n'); // \r\n → \n
                        i = 1;
                    }
                    0 => {
                        out.push(b'\r'); // \r\0 → \r
                        i = 1;
                    }
                    _ => {
                        out.push(b'\r'); // bare \r
                    }
                }
            } else {
                out.push(b'\r');
            }
        }

        while i < input.len() {
            if input[i] == b'\r' {
                if i + 1 < input.len() {
                    match input[i + 1] {
                        b'\n' => {
                            out.push(b'\n');
                            i += 2;
                        }
                        0 => {
                            out.push(b'\r');
                            i += 2;
                        }
                        _ => {
                            out.push(b'\r');
                            i += 1;
                        }
                    }
                } else {
                    // \r at end of block — defer until next block.
                    self.pending_cr = true;
                    i += 1;
                }
            } else {
                out.push(input[i]);
                i += 1;
            }
        }

        out
    }
}

/// State tracker for octet-to-netascii conversion across block boundaries.
/// We need to expand `\n` → `\r\n` and `\r` → `\r\0`.
/// When a conversion causes the output to exceed `blksize`, overflow bytes
/// are stored and prepended to the next block.
#[derive(Debug, Default)]
pub struct NetasciiEncoder {
    /// Overflow bytes that didn't fit in the previous block.
    pub overflow: Vec<u8>,
}

impl NetasciiEncoder {
    pub fn new() -> Self {
        Self {
            overflow: Vec::new(),
        }
    }

    /// Encode octet (binary) data to netascii, respecting `blksize`.
    /// Returns the encoded block (may be smaller or equal to blksize).
    /// Any overflow is stored internally and will be prepended to the next call.
    pub fn encode(&mut self, input: &[u8], blksize: usize) -> Vec<u8> {
        let mut out = Vec::with_capacity(blksize);

        // First, emit any overflow from previous block.
        if !self.overflow.is_empty() {
            let drain: Vec<u8> = std::mem::take(&mut self.overflow);
            for &b in &drain {
                if out.len() >= blksize {
                    self.overflow.push(b);
                } else {
                    out.push(b);
                }
            }
        }

        for &b in input {
            if out.len() >= blksize {
                // All remaining input goes to overflow, but still needs conversion.
                self.overflow_encode_byte(b);
            } else {
                match b {
                    b'\n' => {
                        out.push(b'\r');
                        if out.len() >= blksize {
                            self.overflow.push(b'\n');
                        } else {
                            out.push(b'\n');
                        }
                    }
                    b'\r' => {
                        out.push(b'\r');
                        if out.len() >= blksize {
                            self.overflow.push(0);
                        } else {
                            out.push(0);
                        }
                    }
                    _ => out.push(b),
                }
            }
        }

        out
    }

    /// Check if there is buffered overflow data remaining.
    pub fn has_overflow(&self) -> bool {
        !self.overflow.is_empty()
    }

    /// Drain remaining overflow into a final block.
    pub fn drain_overflow(&mut self, blksize: usize) -> Vec<u8> {
        let drain = std::mem::take(&mut self.overflow);
        if drain.len() <= blksize {
            drain
        } else {
            // Extremely unlikely but handle gracefully.
            self.overflow = drain[blksize..].to_vec();
            drain[..blksize].to_vec()
        }
    }

    fn overflow_encode_byte(&mut self, b: u8) {
        match b {
            b'\n' => {
                self.overflow.push(b'\r');
                self.overflow.push(b'\n');
            }
            b'\r' => {
                self.overflow.push(b'\r');
                self.overflow.push(0);
            }
            _ => self.overflow.push(b),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_rrq() {
        let pkt = Packet::RRQ {
            filename: "hello.txt".into(),
            mode: "octet".into(),
            options: HashMap::new(),
        };
        let bytes = pkt.to_bytes();
        let parsed = Packet::from_bytes(&bytes).unwrap();
        match parsed {
            Packet::RRQ { filename, mode, .. } => {
                assert_eq!(filename, "hello.txt");
                assert_eq!(mode, "octet");
            }
            _ => panic!("expected RRQ"),
        }
    }

    #[test]
    fn round_trip_data() {
        let pkt = Packet::DATA {
            block_num: 42,
            data: vec![1, 2, 3],
        };
        let bytes = pkt.to_bytes();
        let parsed = Packet::from_bytes(&bytes).unwrap();
        match parsed {
            Packet::DATA { block_num, data } => {
                assert_eq!(block_num, 42);
                assert_eq!(data, vec![1, 2, 3]);
            }
            _ => panic!("expected DATA"),
        }
    }

    #[test]
    fn round_trip_ack() {
        let pkt = Packet::ACK { block_num: 7 };
        let bytes = pkt.to_bytes();
        let parsed = Packet::from_bytes(&bytes).unwrap();
        match parsed {
            Packet::ACK { block_num } => assert_eq!(block_num, 7),
            _ => panic!("expected ACK"),
        }
    }

    #[test]
    fn round_trip_error() {
        let pkt = Packet::error(1, "File not found");
        let bytes = pkt.to_bytes();
        let parsed = Packet::from_bytes(&bytes).unwrap();
        match parsed {
            Packet::ERROR { code, msg } => {
                assert_eq!(code, 1);
                assert_eq!(msg, "File not found");
            }
            _ => panic!("expected ERROR"),
        }
    }

    #[test]
    fn parse_rrq_with_blksize_option() {
        let mut buf = Vec::new();
        buf.extend_from_slice(&1u16.to_be_bytes());
        buf.extend_from_slice(b"test.bin\0octet\0blksize\08192\0");
        let parsed = Packet::from_bytes(&buf).unwrap();
        match parsed {
            Packet::RRQ {
                filename, options, ..
            } => {
                assert_eq!(filename, "test.bin");
                assert_eq!(options.get("blksize").unwrap(), "8192");
            }
            _ => panic!("expected RRQ"),
        }
    }

    #[test]
    fn round_trip_oack() {
        let mut options = HashMap::new();
        options.insert("blksize".to_string(), "8192".to_string());
        let pkt = Packet::OACK { options };
        let bytes = pkt.to_bytes();
        let parsed = Packet::from_bytes(&bytes).unwrap();
        match parsed {
            Packet::OACK { options } => {
                assert_eq!(options.get("blksize").unwrap(), "8192");
            }
            _ => panic!("expected OACK"),
        }
    }

    #[test]
    fn netascii_decode_crlf() {
        let mut dec = NetasciiDecoder::new();
        let input = b"hello\r\nworld\r\n";
        let output = dec.decode(input);
        assert_eq!(output, b"hello\nworld\n");
    }

    #[test]
    fn netascii_decode_cr_nul() {
        let mut dec = NetasciiDecoder::new();
        let input = b"foo\r\0bar";
        let output = dec.decode(input);
        assert_eq!(output, b"foo\rbar");
    }

    #[test]
    fn netascii_decode_split_cr() {
        let mut dec = NetasciiDecoder::new();
        // \r at end of first block, \n at start of second
        let out1 = dec.decode(b"abc\r");
        let out2 = dec.decode(b"\ndef");
        let mut combined = out1;
        combined.extend_from_slice(&out2);
        assert_eq!(combined, b"abc\ndef");
    }

    #[test]
    fn netascii_encode_basic() {
        let mut enc = NetasciiEncoder::new();
        let input = b"hello\nworld\n";
        let output = enc.encode(input, 512);
        assert_eq!(output, b"hello\r\nworld\r\n");
        assert!(!enc.has_overflow());
    }

    #[test]
    fn netascii_encode_cr() {
        let mut enc = NetasciiEncoder::new();
        let input = b"foo\rbar";
        let output = enc.encode(input, 512);
        assert_eq!(output, b"foo\r\0bar");
    }

    #[test]
    fn netascii_encode_overflow() {
        let mut enc = NetasciiEncoder::new();
        // Block size of 5: "ab\n" encodes to "ab\r\n" (4 bytes), then "c" = 5 bytes.
        // After that "d\n" would need "d\r\n" (3 bytes) overflowing.
        let input = b"ab\ncd\n";
        let out1 = enc.encode(input, 5);
        assert_eq!(out1.len(), 5);
        assert_eq!(out1, b"ab\r\nc");
        assert!(enc.has_overflow());
        let out2 = enc.drain_overflow(512);
        assert_eq!(out2, b"d\r\n");
    }
}
