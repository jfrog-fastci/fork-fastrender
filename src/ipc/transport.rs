//! IPC transport helpers (bincode + length-prefixed framing).
//!
//! This module provides a small `Transport` wrapper around a `Read` + `Write` pair.
//! The transport encodes messages with `bincode` and frames them using the length-prefix
//! format from [`crate::ipc::framing`].
//!
//! ## Deadlines / timeouts
//!
//! IPC is a security boundary: the peer can stop sending bytes while keeping the socket
//! open. Blocking I/O in the browser process must therefore have an external deadline so
//! the UI stays responsive even if a renderer/network process wedges.
//!
//! On Unix platforms, `*_with_timeout` uses `poll(2)` to wait for readiness between
//! incremental `read(2)`/`write(2)` calls, enforcing a hard wall-clock timeout.

use super::error::IpcError;
use super::framing::{decode_bincode_payload, encode_bincode_payload, IPC_LENGTH_PREFIX_BYTES};
use super::limits::MAX_IPC_MESSAGE_BYTES;
use serde::de::DeserializeOwned;
use serde::Serialize;
use std::io::{Read, Write};
use std::time::Duration;

/// Length-delimited IPC transport that encodes payloads using `bincode`.
#[derive(Debug)]
pub struct Transport<R, W> {
  reader: R,
  writer: W,
}

impl<R, W> Transport<R, W> {
  pub fn new(reader: R, writer: W) -> Self {
    Self { reader, writer }
  }

  pub fn into_inner(self) -> (R, W) {
    (self.reader, self.writer)
  }

  pub fn reader_mut(&mut self) -> &mut R {
    &mut self.reader
  }

  pub fn writer_mut(&mut self) -> &mut W {
    &mut self.writer
  }
}

impl<R: Read, W: Write> Transport<R, W> {
  pub fn send<T: Serialize>(&mut self, msg: &T) -> Result<(), IpcError> {
    let payload = encode_bincode_payload(msg)?;
    write_frame_unchecked(&mut self.writer, &payload)?;
    Ok(())
  }

  pub fn recv<T: DeserializeOwned>(&mut self) -> Result<T, IpcError> {
    let payload = read_frame_unchecked(&mut self.reader)?;
    decode_bincode_payload(&payload)
  }
}

fn validate_len_prefix(bytes_len: u32) -> Result<usize, IpcError> {
  if bytes_len == 0 {
    return Err(IpcError::ProtocolViolation {
      msg: "IPC frame length was zero".to_string(),
    });
  }
  let max_u32: u32 = MAX_IPC_MESSAGE_BYTES
    .try_into()
    .unwrap_or(u32::MAX);
  if bytes_len > max_u32 {
    return Err(IpcError::MessageTooLarge { len: bytes_len, max: max_u32 });
  }
  // `u32` always fits in `usize` on supported targets (>= 32-bit).
  Ok(bytes_len as usize)
}

fn read_frame_unchecked<R: Read>(reader: &mut R) -> Result<Vec<u8>, IpcError> {
  let mut len_prefix = [0u8; IPC_LENGTH_PREFIX_BYTES];
  reader.read_exact(&mut len_prefix)?;
  let bytes_len = u32::from_le_bytes(len_prefix);
  let len = validate_len_prefix(bytes_len)?;
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

fn write_frame_unchecked<W: Write>(writer: &mut W, payload: &[u8]) -> Result<(), IpcError> {
  let bytes_len =
    u32::try_from(payload.len()).unwrap_or(u32::MAX);
  // Validate (and ensure it can be converted to `usize` if needed by downstream code).
  let _ = validate_len_prefix(bytes_len)?;
  writer.write_all(&bytes_len.to_le_bytes())?;
  writer.write_all(payload)?;
  Ok(())
}

#[cfg(unix)]
mod unix_deadlines {
  use super::{decode_bincode_payload, encode_bincode_payload, validate_len_prefix, IpcError};
  use super::IPC_LENGTH_PREFIX_BYTES;
  use serde::de::DeserializeOwned;
  use serde::Serialize;
  use std::io::{Read, Write};
  use std::os::unix::io::AsRawFd;
  use std::time::{Duration, Instant};
 
  fn poll_timeout_ms(timeout: Duration) -> libc::c_int {
    if timeout.is_zero() {
      return 0;
    }
    let mut ms = timeout.as_millis();
    if ms == 0 {
      ms = 1;
    }
    let max = libc::c_int::MAX as u128;
    if ms > max {
      libc::c_int::MAX
    } else {
      ms as libc::c_int
    }
  }
 
  fn poll_ready(fd: libc::c_int, events: libc::c_short, deadline: Instant) -> Result<(), IpcError> {
    loop {
      let remaining = deadline.saturating_duration_since(Instant::now());
      if remaining.is_zero() {
        return Err(IpcError::Timeout);
      }
 
      let mut pfd = libc::pollfd {
        fd,
        events,
        revents: 0,
      };
 
      // SAFETY: `pfd` points to a valid pollfd struct for the duration of the call.
      let rc = unsafe { libc::poll(&mut pfd, 1, poll_timeout_ms(remaining)) };
      if rc == 0 {
        return Err(IpcError::Timeout);
      }
      if rc < 0 {
        let err = std::io::Error::last_os_error();
        if err.kind() == std::io::ErrorKind::Interrupted {
          continue;
        }
        return Err(IpcError::Io(err));
      }
      return Ok(());
    }
  }
 
  fn read_exact_with_deadline<R: Read + AsRawFd>(
    reader: &mut R,
    buf: &mut [u8],
    deadline: Instant,
  ) -> Result<(), IpcError> {
    let mut filled = 0usize;
    while filled < buf.len() {
      poll_ready(reader.as_raw_fd(), libc::POLLIN, deadline)?;
      match reader.read(&mut buf[filled..]) {
        Ok(0) => return Err(IpcError::Disconnected),
        Ok(n) => filled += n,
        Err(err) if err.kind() == std::io::ErrorKind::Interrupted => {}
        Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {}
        Err(err) => return Err(IpcError::Io(err)),
      }
    }
    Ok(())
  }
 
  fn write_all_with_deadline<W: Write + AsRawFd>(
    writer: &mut W,
    buf: &[u8],
    deadline: Instant,
  ) -> Result<(), IpcError> {
    let mut written = 0usize;
    while written < buf.len() {
      poll_ready(writer.as_raw_fd(), libc::POLLOUT, deadline)?;
      match writer.write(&buf[written..]) {
        Ok(0) => {
          return Err(IpcError::Io(std::io::Error::new(
            std::io::ErrorKind::WriteZero,
            "IPC write returned 0 bytes",
          )))
        }
        Ok(n) => written += n,
        Err(err) if err.kind() == std::io::ErrorKind::Interrupted => {}
        Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {}
        Err(err) => return Err(IpcError::Io(err)),
      }
    }
    Ok(())
  }
 
  pub(super) fn recv_with_timeout<R: Read + AsRawFd, T: DeserializeOwned>(
    reader: &mut R,
    timeout: Duration,
  ) -> Result<T, IpcError> {
    let deadline = Instant::now() + timeout;
 
    let mut len_prefix = [0u8; IPC_LENGTH_PREFIX_BYTES];
    read_exact_with_deadline(reader, &mut len_prefix, deadline)?;
    let bytes_len = u32::from_le_bytes(len_prefix);
    let len = validate_len_prefix(bytes_len)?;

    let mut payload = Vec::new();
    payload.try_reserve_exact(len).map_err(|err| {
      IpcError::Io(std::io::Error::new(
        std::io::ErrorKind::Other,
        format!("IPC frame allocation failed (len={len}): {err:?}"),
      ))
    })?;
    payload.resize(len, 0);
    read_exact_with_deadline(reader, &mut payload, deadline)?;
    decode_bincode_payload(&payload)
  }
 
  pub(super) fn send_with_timeout<W: Write + AsRawFd, T: Serialize>(
    writer: &mut W,
    msg: &T,
    timeout: Duration,
  ) -> Result<(), IpcError> {
    let deadline = Instant::now() + timeout;
 
    let payload = encode_bincode_payload(msg)?;
    let bytes_len = u32::try_from(payload.len()).unwrap_or(u32::MAX);
    let _ = validate_len_prefix(bytes_len)?;
 
    write_all_with_deadline(writer, &bytes_len.to_le_bytes(), deadline)?;
    write_all_with_deadline(writer, &payload, deadline)?;
    Ok(())
  }
}

#[cfg(unix)]
impl<R: Read, W: Write> Transport<R, W> {
  /// Receive a bincode-framed message, failing with [`IpcError::Timeout`] if no progress can be made
  /// within `timeout`.
  pub fn recv_with_timeout<T: DeserializeOwned>(
    &mut self,
    timeout: Duration,
  ) -> Result<T, IpcError>
  where
    R: std::os::unix::io::AsRawFd,
  {
    unix_deadlines::recv_with_timeout(&mut self.reader, timeout)
  }

  /// Send a bincode-framed message, failing with [`IpcError::Timeout`] if the socket stays blocked
  /// for longer than `timeout`.
  pub fn send_with_timeout<T: Serialize>(
    &mut self,
    msg: &T,
    timeout: Duration,
  ) -> Result<(), IpcError>
  where
    W: std::os::unix::io::AsRawFd,
  {
    unix_deadlines::send_with_timeout(&mut self.writer, msg, timeout)
  }
}

#[cfg(not(unix))]
impl<R: Read, W: Write> Transport<R, W> {
  pub fn recv_with_timeout<T: DeserializeOwned>(
    &mut self,
    _timeout: Duration,
  ) -> Result<T, IpcError> {
    Err(IpcError::Unsupported {
      msg: "recv_with_timeout is not supported on this platform".to_string(),
    })
  }

  pub fn send_with_timeout<T: Serialize>(
    &mut self,
    _msg: &T,
    _timeout: Duration,
  ) -> Result<(), IpcError> {
    Err(IpcError::Unsupported {
      msg: "send_with_timeout is not supported on this platform".to_string(),
    })
  }
}

#[cfg(test)]
mod timeout {
  use super::*;
  use std::time::Duration;

  #[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
  struct TestMsg {
    id: u32,
    text: String,
  }

  #[cfg(target_os = "linux")]
  #[test]
  fn recv_with_timeout_times_out_when_peer_sends_nothing() {
    use std::os::unix::net::UnixStream;

    let (a, _b) = UnixStream::pair().expect("socketpair");
    // Use a clone for the writer side even though the test never writes; this matches typical usage
    // where the transport owns separate reader/writer handles.
    let reader = a.try_clone().expect("try_clone");
    let mut transport = Transport::new(reader, a);

    let err = transport
      .recv_with_timeout::<TestMsg>(Duration::from_millis(50))
      .expect_err("recv should time out");
    assert!(matches!(err, IpcError::Timeout), "unexpected error: {err:?}");
  }

  #[cfg(target_os = "linux")]
  #[test]
  fn roundtrip_with_timeout_smoke() {
    use std::os::unix::net::UnixStream;

    let (a, b) = UnixStream::pair().expect("socketpair");
    let mut left = Transport::new(a.try_clone().expect("try_clone"), a);
    let mut right = Transport::new(b.try_clone().expect("try_clone"), b);

    let msg = TestMsg {
      id: 7,
      text: "hello".to_string(),
    };

    left
      .send_with_timeout(&msg, Duration::from_secs(1))
      .expect("send");
    let got: TestMsg = right
      .recv_with_timeout(Duration::from_secs(1))
      .expect("recv");
    assert_eq!(got, msg);
  }
}
