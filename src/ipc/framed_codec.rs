use std::io;
use std::io::Read;
use std::io::Write;

use serde::de::DeserializeOwned;
use serde::Serialize;

/// Hard upper bound on the serialized payload size (not including the 4-byte length prefix).
///
/// This exists to prevent OOMs and stream desync from malformed frames.
pub const MAX_FRAME_LEN: usize = 64 * 1024 * 1024; // 64 MiB

/// Write a single length-delimited IPC frame (`u32` LE length prefix + serialized payload).
///
/// The payload is serialized using `bincode` and is bounded by [`MAX_FRAME_LEN`].
pub fn write_msg<W: Write, T: Serialize>(w: &mut W, msg: &T) -> io::Result<()> {
  let payload = bincode::serialize(msg).map_err(|err| {
    io::Error::new(
      io::ErrorKind::InvalidData,
      format!("IPC frame serialization failed: {err}"),
    )
  })?;

  if payload.len() > MAX_FRAME_LEN {
    return Err(io::Error::new(
      io::ErrorKind::InvalidInput,
      format!(
        "IPC frame too large: {} bytes (max {MAX_FRAME_LEN})",
        payload.len()
      ),
    ));
  }

  let len_u32 = u32::try_from(payload.len()).map_err(|_err| {
    io::Error::new(
      io::ErrorKind::InvalidInput,
      format!(
        "IPC frame length does not fit into u32: {} bytes",
        payload.len()
      ),
    )
  })?;

  w.write_all(&len_u32.to_le_bytes())?;
  w.write_all(&payload)?;
  Ok(())
}

/// Read a single length-delimited IPC frame (`u32` LE length prefix + serialized payload).
///
/// This performs defensive reads (reads the full payload and errors on EOF mid-frame) and enforces
/// [`MAX_FRAME_LEN`] before allocating the payload buffer.
pub fn read_msg<R: Read, T: DeserializeOwned>(r: &mut R) -> io::Result<T> {
  let mut len_buf = [0u8; 4];
  r.read_exact(&mut len_buf)?;
  let frame_len = u32::from_le_bytes(len_buf);

  let frame_len_usize = usize::try_from(frame_len).map_err(|_err| {
    io::Error::new(
      io::ErrorKind::InvalidData,
      format!("IPC frame length {frame_len} does not fit in usize"),
    )
  })?;

  if frame_len_usize > MAX_FRAME_LEN {
    return Err(io::Error::new(
      io::ErrorKind::InvalidData,
      format!("IPC frame length {frame_len_usize} exceeds MAX_FRAME_LEN {MAX_FRAME_LEN}"),
    ));
  }

  let mut payload = vec![0u8; frame_len_usize];
  r.read_exact(&mut payload)?;

  bincode::deserialize(&payload).map_err(|err| {
    io::Error::new(
      io::ErrorKind::InvalidData,
      format!("IPC frame deserialization failed: {err}"),
    )
  })
}

#[cfg(test)]
mod tests {
  use super::*;

  use std::io::Cursor;
  use crate::ipc::protocol::network::BrowserToNetwork;

  #[test]
  fn roundtrip_browser_to_network_message() {
    let msg = BrowserToNetwork::Fetch {
      request_id: 1,
      url: "https://example.com/".to_owned(),
    };

    let mut buf = Vec::new();
    write_msg(&mut buf, &msg).unwrap();

    let mut cursor = Cursor::new(buf);
    let decoded: BrowserToNetwork = read_msg(&mut cursor).unwrap();
    assert_eq!(decoded, msg);
    assert_eq!(cursor.position() as usize, cursor.get_ref().len());
  }

  #[test]
  fn read_rejects_oversized_frame_before_reading_payload() {
    struct LimitedRead {
      buf: [u8; 4],
      pos: usize,
    }

    impl Read for LimitedRead {
      fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if self.pos >= self.buf.len() {
          panic!("read_msg attempted to read beyond the length prefix");
        }
        let n = (self.buf.len() - self.pos).min(out.len());
        out[..n].copy_from_slice(&self.buf[self.pos..self.pos + n]);
        self.pos += n;
        Ok(n)
      }
    }

    let mut r = LimitedRead {
      buf: u32::MAX.to_le_bytes(),
      pos: 0,
    };

    let err = read_msg::<_, BrowserToNetwork>(&mut r).unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::InvalidData);
  }

  #[test]
  fn read_errors_on_truncated_frame() {
    let declared_len: u32 = 10;
    let mut bytes = Vec::new();
    bytes.extend_from_slice(&declared_len.to_le_bytes());
    bytes.extend_from_slice(&[1, 2, 3, 4, 5]); // truncated payload

    let mut cursor = Cursor::new(bytes);
    let err = read_msg::<_, BrowserToNetwork>(&mut cursor).unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::UnexpectedEof);
  }
}
