use std::io::{Read, Write};

use super::error::IpcError;

/// Maximum payload size accepted by `read_frame` / `write_frame`.
///
/// This is intentionally small to cap memory usage and avoid unbounded allocations when parsing a
/// length-prefixed protocol.
pub const MAX_IPC_MESSAGE_BYTES: usize = 4 * 1024 * 1024; // 4 MiB

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
  fn eof_is_an_error() {
    let mut cursor = Cursor::new(vec![0, 1, 2]); // 3 bytes; incomplete u32 prefix.
    let err = read_frame(&mut cursor).unwrap_err();
    assert!(matches!(err, IpcError::UnexpectedEof));
  }
}

