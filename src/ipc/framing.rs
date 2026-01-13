use std::io::{Read, Write};

use bincode::Options;
use serde::{de::DeserializeOwned, Serialize};

use super::error::IpcError;

/// Maximum payload size accepted by `read_frame` / `write_frame`.
///
/// This is intentionally small to cap memory usage and avoid unbounded allocations when parsing a
/// length-prefixed protocol.
///
/// The cap is sized to safely accommodate the largest *allowed* WebSocket payload (4 MiB) plus
/// serialization overhead for IPC envelopes. Keep this value in sync with any IPC-level limits
/// enforced by the network process once WebSocket traffic is proxied over renderer↔network IPC.
pub const MAX_IPC_MESSAGE_BYTES: usize = 8 * 1024 * 1024; // 8 MiB

fn bincode_options() -> impl Options {
  // Use fixed-width integer encoding to keep the wire format predictable and easy to reason about
  // when hand-crafting regression cases.
  bincode::DefaultOptions::new()
    .with_fixint_encoding()
    .with_limit(MAX_IPC_MESSAGE_BYTES as u64)
}

/// Serialize `msg` into a payload suitable for [`write_frame`].
pub fn encode_bincode_payload<T: Serialize>(msg: &T) -> Result<Vec<u8>, IpcError> {
  Ok(bincode_options().serialize(msg)?)
}

/// Deserialize a message from a [`read_frame`] payload.
///
/// This uses a hard byte limit to prevent pathological container lengths from triggering large
/// allocations during deserialization.
pub fn decode_bincode_payload<T: DeserializeOwned>(payload: &[u8]) -> Result<T, IpcError> {
  Ok(bincode_options().deserialize(payload)?)
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

/// Write a single length-prefixed frame to `writer`.
///
/// Frame format: `u32` little-endian payload length, followed by payload bytes.
pub fn write_frame<W: Write>(writer: &mut W, payload: &[u8]) -> Result<(), IpcError> {
  if payload.is_empty() {
    return Err(IpcError::ZeroLength);
  }
  if payload.len() > MAX_IPC_MESSAGE_BYTES {
    return Err(IpcError::FrameTooLarge {
      len: payload.len(),
      max: MAX_IPC_MESSAGE_BYTES,
    });
  }

  // Safe: `MAX_IPC_MESSAGE_BYTES` is far below `u32::MAX`.
  let len_prefix = (payload.len() as u32).to_le_bytes();
  writer.write_all(&len_prefix)?;
  writer.write_all(payload)?;
  Ok(())
}

/// Read a single length-prefixed frame from `reader`.
///
/// Returns an error on EOF, invalid lengths, or I/O failure.
pub fn read_frame<R: Read>(reader: &mut R) -> Result<Vec<u8>, IpcError> {
  let mut len_prefix = [0u8; 4];
  reader.read_exact(&mut len_prefix)?;
  let len = u32::from_le_bytes(len_prefix) as usize;

  if len == 0 {
    return Err(IpcError::ZeroLength);
  }
  if len > MAX_IPC_MESSAGE_BYTES {
    return Err(IpcError::FrameTooLarge {
      len,
      max: MAX_IPC_MESSAGE_BYTES,
    });
  }

  // Allocate only after validating the declared size.
  let mut payload = vec![0u8; len];
  reader.read_exact(&mut payload)?;
  Ok(payload)
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
      matches!(err, IpcError::FrameTooLarge { .. }),
      "unexpected error: {err:?}"
    );
  }

  #[test]
  fn rejects_length_just_over_cap() {
    // A regression should fail with `FrameTooLarge` (not `UnexpectedEof`) and must not attempt to
    // read/allocate the claimed payload.
    let oversized_len: u32 = (MAX_IPC_MESSAGE_BYTES + 1)
      .try_into()
      .expect("MAX_IPC_MESSAGE_BYTES should fit in u32 for framing");
    let mut buf = Vec::new();
    buf.extend_from_slice(&oversized_len.to_le_bytes());

    let mut cursor = Cursor::new(buf);
    let err = read_frame(&mut cursor).unwrap_err();
    assert!(
      matches!(err, IpcError::FrameTooLarge { .. }),
      "unexpected error: {err:?}"
    );
  }

  #[test]
  fn eof_is_an_error() {
    let mut cursor = Cursor::new(vec![0, 1, 2]); // 3 bytes; incomplete u32 prefix.
    let err = read_frame(&mut cursor).unwrap_err();
    assert!(matches!(err, IpcError::UnexpectedEof));
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
      IpcError::Codec(source) => {
        assert!(
          matches!(source.as_ref(), bincode::ErrorKind::SizeLimit),
          "expected SizeLimit error, got {source:?}"
        );
      }
      other => panic!("unexpected error: {other:?}"),
    }
  }
}
