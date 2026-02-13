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
}
