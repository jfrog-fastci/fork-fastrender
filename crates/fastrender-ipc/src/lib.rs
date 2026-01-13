#![forbid(unsafe_code)]

use serde::{Deserialize, Serialize};

/// Identifier for a frame (tab/iframe) hosted inside a renderer process.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct FrameId(pub u64);

/// Pixel buffer for a fully-rendered frame.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FrameBuffer {
  pub width: u32,
  pub height: u32,
  /// RGBA8 pixel data, row-major, length = `width * height * 4`.
  pub rgba8: Vec<u8>,
}

/// Messages sent from the browser process to a renderer process.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum BrowserToRenderer {
  /// Create per-frame state for `frame_id`.
  ///
  /// The browser is responsible for choosing a unique `FrameId` within the target renderer
  /// process.
  CreateFrame { frame_id: FrameId },
  /// Destroy per-frame state for `frame_id`.
  DestroyFrame { frame_id: FrameId },
  /// Navigate the given frame to `url`.
  Navigate { frame_id: FrameId, url: String },
  /// Resize the viewport for the given frame (CSS pixels).
  Resize {
    frame_id: FrameId,
    width: u32,
    height: u32,
    dpr: f32,
  },
  /// Request a repaint of the given frame.
  RequestRepaint { frame_id: FrameId },
  /// Terminate the renderer process.
  Shutdown,
}

/// Messages sent from a renderer process back to the browser process.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum RendererToBrowser {
  /// A rendered frame is ready for presentation.
  FrameReady { frame_id: FrameId, buffer: FrameBuffer },
  /// Report a recoverable error related to a specific frame (if any).
  Error {
    frame_id: Option<FrameId>,
    message: String,
  },
}

/// Abstract transport for browser↔renderer IPC.
///
/// This is intentionally minimal so unit tests can use an in-memory transport, while production
/// builds can use pipes/sockets/shared-memory.
pub trait IpcTransport {
  type Error;

  /// Receive the next message from the browser.
  ///
  /// Returning `Ok(None)` indicates the transport is closed (renderer should shut down).
  fn recv(&mut self) -> Result<Option<BrowserToRenderer>, Self::Error>;

  /// Send a message to the browser.
  fn send(&mut self, msg: RendererToBrowser) -> Result<(), Self::Error>;
}

