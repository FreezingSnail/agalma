//! Length-prefixed JSON framing (decision D1).
//!
//! A frame is `4-byte big-endian u32 length` followed by exactly that many bytes
//! of UTF-8 JSON. The maximum payload is [`MAX_FRAME`] (16 MiB); larger frames
//! are rejected before any allocation proportional to the declared size.

use std::io::{self, Read, Write};

use serde::de::DeserializeOwned;
use serde::Serialize;

/// Maximum frame payload size: 16 MiB (decision D1).
pub const MAX_FRAME: usize = 16 * 1024 * 1024;

/// Width of the length prefix in bytes.
pub const LENGTH_PREFIX: usize = 4;

/// Failure while encoding or decoding a frame.
#[derive(Debug)]
pub enum FrameError {
    /// The payload (or its declared length) exceeds [`MAX_FRAME`].
    TooLarge { len: usize, max: usize },
    /// Underlying I/O failure (including truncated frames).
    Io(io::Error),
    /// The payload was not valid JSON for the requested type.
    Json(serde_json::Error),
}

impl std::fmt::Display for FrameError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FrameError::TooLarge { len, max } => {
                write!(f, "frame of {len} bytes exceeds the {max}-byte limit")
            }
            FrameError::Io(e) => write!(f, "frame I/O error: {e}"),
            FrameError::Json(e) => write!(f, "frame JSON error: {e}"),
        }
    }
}

impl std::error::Error for FrameError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            FrameError::Io(e) => Some(e),
            FrameError::Json(e) => Some(e),
            FrameError::TooLarge { .. } => None,
        }
    }
}

impl From<io::Error> for FrameError {
    fn from(e: io::Error) -> Self {
        FrameError::Io(e)
    }
}

impl From<serde_json::Error> for FrameError {
    fn from(e: serde_json::Error) -> Self {
        FrameError::Json(e)
    }
}

fn check_len(len: usize) -> Result<(), FrameError> {
    if len > MAX_FRAME {
        Err(FrameError::TooLarge { len, max: MAX_FRAME })
    } else {
        Ok(())
    }
}

/// Serialize `value` to JSON bytes, rejecting payloads over [`MAX_FRAME`].
pub fn encode_json<T: Serialize>(value: &T) -> Result<Vec<u8>, FrameError> {
    let payload = serde_json::to_vec(value)?;
    check_len(payload.len())?;
    Ok(payload)
}

/// Deserialize JSON payload bytes into `T`.
pub fn decode_json<T: DeserializeOwned>(payload: &[u8]) -> Result<T, FrameError> {
    Ok(serde_json::from_slice(payload)?)
}

/// Prefix `payload` with its big-endian `u32` length, enforcing [`MAX_FRAME`].
pub fn frame_payload(payload: &[u8]) -> Result<Vec<u8>, FrameError> {
    check_len(payload.len())?;
    let mut out = Vec::with_capacity(LENGTH_PREFIX + payload.len());
    out.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    out.extend_from_slice(payload);
    Ok(out)
}

/// Serialize `value` and write one complete frame.
pub fn write_value<W: Write, T: Serialize>(writer: &mut W, value: &T) -> Result<(), FrameError> {
    let payload = encode_json(value)?;
    write_payload(writer, &payload)
}

/// Write one complete frame around an already-serialized `payload`.
pub fn write_payload<W: Write>(writer: &mut W, payload: &[u8]) -> Result<(), FrameError> {
    let framed = frame_payload(payload)?;
    writer.write_all(&framed)?;
    writer.flush()?;
    Ok(())
}

/// Read one frame payload.
///
/// Returns `Ok(None)` when the reader is at a clean end of stream (zero bytes
/// read before the prefix). A declared length above [`MAX_FRAME`] fails with
/// [`FrameError::TooLarge`] *before* the body is read, so a hostile peer cannot
/// force a large allocation.
pub fn read_payload<R: Read>(reader: &mut R) -> Result<Option<Vec<u8>>, FrameError> {
    let mut prefix = [0u8; LENGTH_PREFIX];
    let got = read_full(reader, &mut prefix)?;
    if got == 0 {
        return Ok(None);
    }
    if got < LENGTH_PREFIX {
        return Err(FrameError::Io(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "truncated length prefix",
        )));
    }
    let len = u32::from_be_bytes(prefix) as usize;
    check_len(len)?;
    let mut payload = vec![0u8; len];
    let got = read_full(reader, &mut payload)?;
    if got < len {
        return Err(FrameError::Io(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "truncated frame body",
        )));
    }
    Ok(Some(payload))
}

/// Read until `buf` is full or the reader reports EOF; returns bytes read.
fn read_full<R: Read>(reader: &mut R, buf: &mut [u8]) -> Result<usize, FrameError> {
    let mut filled = 0usize;
    while filled < buf.len() {
        match reader.read(&mut buf[filled..]) {
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(FrameError::Io(e)),
        }
    }
    Ok(filled)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::io::Cursor;

    #[test]
    fn round_trips_a_json_value() {
        let value = json!({"v": 1, "id": 42, "method": "echo", "params": {"x": true}});
        let payload = encode_json(&value).expect("encode");
        let framed = frame_payload(&payload).expect("frame");

        // Prefix is the big-endian payload length.
        assert_eq!(&framed[..4], &(payload.len() as u32).to_be_bytes());

        let mut reader = Cursor::new(framed);
        let read = read_payload(&mut reader).expect("read").expect("some");
        assert_eq!(read, payload);
        let decoded: serde_json::Value = decode_json(&read).expect("decode");
        assert_eq!(decoded, value);

        // Clean EOF after one frame.
        assert!(read_payload(&mut reader).expect("eof").is_none());
    }

    #[test]
    fn encode_rejects_oversized_payload() {
        let big = "x".repeat(MAX_FRAME + 1);
        match encode_json(&json!({ "blob": big })) {
            Err(FrameError::TooLarge { len, max }) => {
                assert!(len > MAX_FRAME);
                assert_eq!(max, MAX_FRAME);
            }
            other => panic!("expected TooLarge, got {other:?}"),
        }
    }

    #[test]
    fn frame_payload_rejects_oversized_length() {
        let payload = vec![0u8; MAX_FRAME + 1];
        assert!(matches!(
            frame_payload(&payload),
            Err(FrameError::TooLarge { .. })
        ));
    }

    #[test]
    fn read_rejects_oversized_declared_length_without_body() {
        // Only a prefix that claims a frame over the cap; no body follows.
        let mut bytes = ((MAX_FRAME as u32) + 1).to_be_bytes().to_vec();
        // Extra bytes that must never be read into a huge allocation.
        bytes.extend_from_slice(b"garbage");
        let mut reader = Cursor::new(bytes);
        match read_payload(&mut reader) {
            Err(FrameError::TooLarge { len, max }) => {
                assert_eq!(len, MAX_FRAME + 1);
                assert_eq!(max, MAX_FRAME);
            }
            other => panic!("expected TooLarge, got {other:?}"),
        }
    }

    #[test]
    fn read_reports_truncated_prefix() {
        let mut reader = Cursor::new(vec![0u8, 1u8]);
        assert!(matches!(
            read_payload(&mut reader),
            Err(FrameError::Io(_))
        ));
    }

    #[test]
    fn read_reports_truncated_body() {
        let mut bytes = 8u32.to_be_bytes().to_vec();
        bytes.extend_from_slice(b"abc"); // only 3 of 8 bytes
        let mut reader = Cursor::new(bytes);
        assert!(matches!(
            read_payload(&mut reader),
            Err(FrameError::Io(_))
        ));
    }

    #[test]
    fn clean_eof_yields_none() {
        let mut reader = Cursor::new(Vec::<u8>::new());
        assert!(read_payload(&mut reader).expect("read").is_none());
    }
}
