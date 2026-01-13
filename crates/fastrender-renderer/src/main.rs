#![forbid(unsafe_code)]

use bincode::Options;
use fastrender_ipc::{BrowserToRenderer, IpcTransport, RendererToBrowser, MAX_IPC_MESSAGE_BYTES};
use std::io::{self, Read, Write};

/// Simple stdio-based IPC transport using `bincode`.
///
/// This is primarily for early development/testing. Real browser integration will likely use a
/// dedicated socket/pipe transport and shared memory for frame buffers.
struct StdioTransport<R: Read, W: Write> {
  reader: R,
  writer: W,
}

impl<R: Read, W: Write> StdioTransport<R, W> {
  fn new(reader: R, writer: W) -> Self {
    Self { reader, writer }
  }
}

impl<R: Read, W: Write> IpcTransport for StdioTransport<R, W> {
  type Error = Box<bincode::ErrorKind>;

  fn recv(&mut self) -> Result<Option<BrowserToRenderer>, Self::Error> {
    let mut len_prefix = [0u8; 4];
    match self.reader.read_exact(&mut len_prefix) {
      Ok(()) => {}
      Err(err) => {
        // Treat EOF as a clean shutdown.
        if err.kind() == io::ErrorKind::UnexpectedEof {
          return Ok(None);
        }
        return Err(Box::new(bincode::ErrorKind::Io(err)));
      }
    }

    let len = u32::from_le_bytes(len_prefix) as usize;
    if len == 0 {
      return Err(Box::new(bincode::ErrorKind::Custom(
        "IPC frame length prefix was zero".to_string(),
      )));
    }
    if len > MAX_IPC_MESSAGE_BYTES {
      return Err(Box::new(bincode::ErrorKind::Custom(format!(
        "IPC frame too large: {len} bytes (max {MAX_IPC_MESSAGE_BYTES})"
      ))));
    }

    let mut limited = self.reader.by_ref().take(len as u64);
    let opts = bincode::DefaultOptions::new().with_limit(len as u64);
    let msg = opts.deserialize_from(&mut limited)?;

    // Ensure the frame is fully consumed; trailing bytes indicate protocol desync.
    if limited.limit() != 0 {
      let _ = io::copy(&mut limited, &mut io::sink());
      return Err(Box::new(bincode::ErrorKind::Custom(
        "IPC frame had trailing bytes after bincode decode".to_string(),
      )));
    }

    Ok(Some(msg))
  }

  fn send(&mut self, msg: RendererToBrowser) -> Result<(), Self::Error> {
    let opts = bincode::DefaultOptions::new();
    let len = opts.serialized_size(&msg)?;
    if len == 0 {
      return Err(Box::new(bincode::ErrorKind::Custom(
        "IPC message serialized to zero bytes".to_string(),
      )));
    }
    if len > (u32::MAX as u64) {
      return Err(Box::new(bincode::ErrorKind::Custom(format!(
        "IPC message too large for u32 length prefix: {len} bytes"
      ))));
    }
    let len = len as usize;
    if len > MAX_IPC_MESSAGE_BYTES {
      return Err(Box::new(bincode::ErrorKind::Custom(format!(
        "IPC message too large: {len} bytes (max {MAX_IPC_MESSAGE_BYTES})"
      ))));
    }

    self
      .writer
      .write_all(&(len as u32).to_le_bytes())
      .map_err(|e| Box::new(bincode::ErrorKind::Io(e)))?;
    opts.serialize_into(&mut self.writer, &msg)?;
    self
      .writer
      .flush()
      .map_err(|e| Box::new(bincode::ErrorKind::Io(e)))
  }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
  let stdin = io::stdin();
  let stdout = io::stdout();
  let transport = StdioTransport::new(stdin.lock(), stdout.lock());
  fastrender_renderer::RendererMainLoop::new(transport).run()?;
  Ok(())
}
