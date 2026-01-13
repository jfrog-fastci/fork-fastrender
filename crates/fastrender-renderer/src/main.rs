#![forbid(unsafe_code)]

use fastrender_ipc::{IpcTransport, RendererToBrowser};
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

  fn recv(&mut self) -> Result<Option<fastrender_ipc::BrowserToRenderer>, Self::Error> {
    match bincode::deserialize_from(&mut self.reader) {
      Ok(msg) => Ok(Some(msg)),
      Err(err) => {
        // Treat EOF as a clean shutdown.
        if let bincode::ErrorKind::Io(io_err) = err.as_ref() {
          if io_err.kind() == io::ErrorKind::UnexpectedEof {
            return Ok(None);
          }
        }
        Err(err)
      }
    }
  }

  fn send(&mut self, msg: RendererToBrowser) -> Result<(), Self::Error> {
    bincode::serialize_into(&mut self.writer, &msg)?;
    self.writer.flush().map_err(|e| Box::new(bincode::ErrorKind::Io(e)))
  }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
  let stdin = io::stdin();
  let stdout = io::stdout();
  let transport = StdioTransport::new(stdin.lock(), stdout.lock());
  fastrender_renderer::RendererMainLoop::new(transport).run()?;
  Ok(())
}

