use std::io::{Read, Write};

use bincode::Options;
use serde::{de::DeserializeOwned, Serialize};

use super::error::IpcError;
pub use super::limits::MAX_IPC_MESSAGE_BYTES;

/// Number of bytes in the length prefix header.
pub const IPC_LENGTH_PREFIX_BYTES: usize = 4;

const MAX_IPC_MESSAGE_BYTES_U32: u32 = MAX_IPC_MESSAGE_BYTES as u32;
fn bincode_options_with_limit(limit_bytes: usize) -> impl Options {
  // Use fixed-width integer encoding to keep the wire format predictable and easy to reason about
  // when hand-crafting regression cases.
  bincode::DefaultOptions::new()
    .with_fixint_encoding()
    .with_limit(limit_bytes as u64)
}

fn bincode_options() -> impl Options {
  bincode_options_with_limit(MAX_IPC_MESSAGE_BYTES)
}

/// Serialize `msg` into a payload suitable for [`write_frame`].
pub fn encode_bincode_payload<T: Serialize>(msg: &T) -> Result<Vec<u8>, IpcError> {
  encode_bincode_payload_with_limit(msg, MAX_IPC_MESSAGE_BYTES)
}

/// Serialize `msg` into a payload with an explicit maximum size.
pub fn encode_bincode_payload_with_limit<T: Serialize>(
  msg: &T,
  limit_bytes: usize,
) -> Result<Vec<u8>, IpcError> {
  Ok(bincode_options_with_limit(limit_bytes).serialize(msg)?)
}

/// Deserialize a message from a [`read_frame`] payload.
///
/// This uses a hard byte limit to prevent pathological container lengths from triggering large
/// allocations during deserialization.
pub fn decode_bincode_payload<T: DeserializeOwned>(payload: &[u8]) -> Result<T, IpcError> {
  decode_bincode_payload_with_limit(payload, MAX_IPC_MESSAGE_BYTES)
}

/// Deserialize a message from a framed payload using an explicit maximum size.
///
/// Note: This is intentionally strict: it rejects payloads larger than `limit_bytes` and also
/// rejects messages that leave trailing bytes after deserialization.
pub fn decode_bincode_payload_with_limit<T: DeserializeOwned>(
  payload: &[u8],
  limit_bytes: usize,
) -> Result<T, IpcError> {
  if payload.len() > limit_bytes {
    return Err(IpcError::MessageTooLarge {
      len: u32::try_from(payload.len()).unwrap_or(u32::MAX),
      max: u32::try_from(limit_bytes).unwrap_or(u32::MAX),
    });
  }

  let mut cursor = std::io::Cursor::new(payload);
  let msg: T = bincode_options_with_limit(limit_bytes).deserialize_from(&mut cursor)?;
  let consumed = usize::try_from(cursor.position()).map_err(|_| IpcError::ProtocolViolation {
    msg: "bincode cursor position does not fit in usize".to_string(),
  })?;
  if consumed != payload.len() {
    return Err(IpcError::ProtocolViolation {
      msg: format!(
        "trailing bytes after bincode message: {} bytes",
        payload.len().saturating_sub(consumed)
      ),
    });
  }
  Ok(msg)
}

/// Serialize `msg` using `bincode` and write it as a single [`write_frame`] frame.
pub fn write_bincode_frame<W: Write, T: Serialize>(writer: &mut W, msg: &T) -> Result<(), IpcError> {
  let payload = encode_bincode_payload(msg)?;
  write_frame(writer, &payload)
}

/// Read a single [`read_frame`] frame and deserialize it using `bincode`.
pub fn read_bincode_frame<R: Read, T: DeserializeOwned>(reader: &mut R) -> Result<T, IpcError> {
  let payload = read_frame(reader)?;
  decode_bincode_payload(&payload)
}

fn validate_frame_len(bytes_len: u32) -> Result<usize, IpcError> {
  if bytes_len == 0 {
    return Err(IpcError::ProtocolViolation {
      msg: "IPC frame length was zero".to_string(),
    });
  }
  if bytes_len > MAX_IPC_MESSAGE_BYTES_U32 {
    return Err(IpcError::MessageTooLarge {
      len: bytes_len,
      max: MAX_IPC_MESSAGE_BYTES_U32,
    });
  }
  // `u32` always fits in `usize` on supported targets (>= 32-bit).
  Ok(bytes_len as usize)
}

/// Write a single length-prefixed frame to `writer`.
///
/// Frame format: `u32` little-endian payload length, followed by payload bytes.
pub fn write_frame<W: Write>(writer: &mut W, payload: &[u8]) -> Result<(), IpcError> {
  write_frame_with_max(writer, payload, MAX_IPC_MESSAGE_BYTES)
}

/// Write a single length-prefixed frame to `writer`, using `max_bytes` as the size cap.
pub fn write_frame_with_max<W: Write>(
  writer: &mut W,
  payload: &[u8],
  max_bytes: usize,
) -> Result<(), IpcError> {
  if payload.is_empty() {
    return Err(IpcError::InvalidParameters {
      msg: "IPC frame payload must be non-empty".to_string(),
    });
  }
  let effective_max = max_bytes.min(MAX_IPC_MESSAGE_BYTES);
  if payload.len() > effective_max {
    return Err(IpcError::MessageTooLarge {
      len: u32::try_from(payload.len()).unwrap_or(u32::MAX),
      max: u32::try_from(effective_max).unwrap_or(u32::MAX),
    });
  }
  let bytes_len = payload.len() as u32;

  let _ = validate_frame_len(bytes_len)?;
  let len_prefix = bytes_len.to_le_bytes();
  writer.write_all(&len_prefix)?;
  writer.write_all(payload)?;
  Ok(())
}

/// Read a single length-prefixed frame from `reader`.
///
/// Returns an error on EOF, invalid lengths, or I/O failure.
pub fn read_frame<R: Read>(reader: &mut R) -> Result<Vec<u8>, IpcError> {
  read_frame_with_max(reader, MAX_IPC_MESSAGE_BYTES)
}

/// Read a single length-prefixed frame from `reader`, using `max_bytes` as the size cap.
pub fn read_frame_with_max<R: Read>(reader: &mut R, max_bytes: usize) -> Result<Vec<u8>, IpcError> {
  let mut len_prefix = [0u8; IPC_LENGTH_PREFIX_BYTES];
  reader.read_exact(&mut len_prefix)?;
  let bytes_len = u32::from_le_bytes(len_prefix);
  let len = validate_frame_len(bytes_len)?;

  let effective_max = max_bytes.min(MAX_IPC_MESSAGE_BYTES);
  if len > effective_max {
    return Err(IpcError::MessageTooLarge {
      len: bytes_len,
      max: u32::try_from(effective_max).unwrap_or(u32::MAX),
    });
  }

  let mut payload = Vec::new();
  payload.try_reserve_exact(len).map_err(|err| {
    IpcError::Io(std::io::Error::new(
      std::io::ErrorKind::Other,
      format!("IPC frame allocation failed (len={len}): {err:?}"),
    ))
  })?;
  payload.resize(len, 0);
  reader.read_exact(&mut payload)?;
  Ok(payload)
}

#[derive(Debug, Clone, Copy)]
pub struct DecodedFrame<'a> {
  pub declared_len: usize,
  pub message_bytes: &'a [u8],
  pub remaining: &'a [u8],
}

/// Decode a single length-prefixed frame from a prefix and the currently available payload bytes.
///
/// This helper is useful for fuzzing and for in-memory decoding paths: it never allocates based on
/// the declared length; it only returns borrowed slices.
pub fn decode_frame_from_parts(prefix: [u8; 4], payload: &[u8]) -> Result<DecodedFrame<'_>, IpcError> {
  let bytes_len = u32::from_le_bytes(prefix);
  let declared_len = validate_frame_len(bytes_len)?;
  if payload.len() < declared_len {
    return Err(IpcError::ProtocolViolation {
      msg: format!(
        "IPC frame payload truncated: declared_len={declared_len} available={}",
        payload.len()
      ),
    });
  }

  let (message_bytes, remaining) = payload.split_at(declared_len);
  Ok(DecodedFrame {
    declared_len,
    message_bytes,
    remaining,
  })
}

/// Decode a single length-prefixed frame from a raw byte slice.
///
/// The first four bytes are interpreted as the length prefix (little-endian), and the remaining
/// bytes are treated as the payload.
pub fn decode_frame_from_bytes(data: &[u8]) -> Result<DecodedFrame<'_>, IpcError> {
  if data.len() < IPC_LENGTH_PREFIX_BYTES {
    return Err(IpcError::ProtocolViolation {
      msg: format!(
        "IPC frame missing length prefix: need {IPC_LENGTH_PREFIX_BYTES} bytes, got {}",
        data.len()
      ),
    });
  }
  let (prefix_bytes, payload) = data.split_at(IPC_LENGTH_PREFIX_BYTES);
  let prefix = [prefix_bytes[0], prefix_bytes[1], prefix_bytes[2], prefix_bytes[3]];
  decode_frame_from_parts(prefix, payload)
}

#[cfg(test)]
mod tests {
  use super::*;
  use std::io::Cursor;
  use serde::{Deserialize, Serialize};

  #[test]
  fn roundtrip_small_payload() {
    let payload = b"hello world";
    let mut buf = Vec::new();
    write_frame(&mut buf, payload).unwrap();

    let mut cursor = Cursor::new(buf);
    let got = read_frame(&mut cursor).unwrap();
    assert_eq!(got, payload);
  }

  #[test]
  fn rejects_oversized_length_without_allocating_claimed_size() {
    // If `read_frame` attempts to allocate this size, the test binary will likely OOM/abort.
    let mut buf = Vec::new();
    buf.extend_from_slice(&u32::MAX.to_le_bytes());

    let mut cursor = Cursor::new(buf);
    let err = read_frame(&mut cursor).unwrap_err();
    assert!(
      matches!(err, IpcError::MessageTooLarge { .. }),
      "unexpected error: {err:?}"
    );
  }

  #[test]
  fn rejects_length_just_over_cap() {
    // A regression should fail with `MessageTooLarge` (not `Disconnected`) and must not attempt to
    // read/allocate the claimed payload.
    let oversized_len: u32 = (MAX_IPC_MESSAGE_BYTES + 1)
      .try_into()
      .expect("MAX_IPC_MESSAGE_BYTES should fit in u32 for framing");
    let mut buf = Vec::new();
    buf.extend_from_slice(&oversized_len.to_le_bytes());

    let mut cursor = Cursor::new(buf);
    let err = read_frame(&mut cursor).unwrap_err();
    assert!(
      matches!(err, IpcError::MessageTooLarge { .. }),
      "unexpected error: {err:?}"
    );
  }

  #[test]
  fn eof_is_an_error() {
    let mut cursor = Cursor::new(vec![0, 1, 2]); // 3 bytes; incomplete u32 prefix.
    let err = read_frame(&mut cursor).unwrap_err();
    assert!(matches!(err, IpcError::Disconnected));
  }

  #[test]
  fn length_at_cap_is_accepted() {
    let len_u32: u32 = MAX_IPC_MESSAGE_BYTES
      .try_into()
      .expect("MAX_IPC_MESSAGE_BYTES should fit in u32 for framing");
    let ok = validate_frame_len(len_u32).unwrap();
    assert_eq!(ok, MAX_IPC_MESSAGE_BYTES);
  }

  #[test]
  fn bincode_frame_roundtrip() {
    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
    struct TestMsg {
      a: u32,
      b: u32,
    }

    let msg = TestMsg { a: 1, b: 2 };
    let mut buf = Vec::new();
    write_bincode_frame(&mut buf, &msg).unwrap();

    let mut cursor = Cursor::new(buf);
    let got: TestMsg = read_bincode_frame(&mut cursor).unwrap();
    assert_eq!(got, msg);
  }

  #[test]
  fn bincode_decode_rejects_oversized_container_len() {
    // Craft a bincode payload for a `Vec<u8>` whose declared length exceeds the hard IPC limit.
    // If `decode_bincode_payload` ignored the limit, this would attempt to allocate a large buffer
    // before failing with EOF.
    let declared_len = (MAX_IPC_MESSAGE_BYTES as u64) + 1;
    let payload = declared_len.to_le_bytes();
    let mut frame = Vec::new();
    frame.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    frame.extend_from_slice(&payload);

    let mut cursor = Cursor::new(frame);
    let err = read_bincode_frame::<_, Vec<u8>>(&mut cursor).unwrap_err();
    match err {
      IpcError::Codec { source } => {
        assert!(
          matches!(source.as_ref(), bincode::ErrorKind::SizeLimit),
          "expected SizeLimit error, got {source:?}"
        );
      }
      other => panic!("unexpected error: {other:?}"),
    }
  }

  #[test]
  fn bincode_decode_rejects_trailing_bytes() {
    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
    struct TestMsg {
      a: u32,
    }

    let msg = TestMsg { a: 7 };
    let mut payload = encode_bincode_payload(&msg).expect("encode");
    payload.extend_from_slice(b"junk");

    let err = decode_bincode_payload::<TestMsg>(&payload).expect_err("expected trailing bytes error");
    assert!(matches!(err, IpcError::ProtocolViolation { .. }));
  }
}
