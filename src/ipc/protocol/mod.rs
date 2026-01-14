//! Multiprocess IPC protocol between the trusted browser process and the untrusted renderer.
//!
//! Design goals:
//! - Keep **renderer → browser** messages allocation-bounded and easy to validate.
//! - Prefer indices/handles over transferring bulk data.
//! - Validate every message from the renderer before acting on it.

pub mod cancel;
pub mod network;
pub mod renderer;

use serde::{Deserialize, Serialize};

use super::IpcError;

fn protocol_violation(msg: impl Into<String>) -> IpcError {
  IpcError::ProtocolViolation { msg: msg.into() }
}

/// Current IPC protocol version.
///
/// Bumped when message shapes or semantics change in an incompatible way.
pub const IPC_PROTOCOL_VERSION: u32 = 1;

/// Pixel format is currently fixed to premultiplied RGBA8.
pub const BYTES_PER_PIXEL: usize = 4;

/// Hard cap for the number of frame buffers the browser can advertise.
pub const MAX_FRAME_BUFFERS: usize = 32;

/// Upper bound for identifiers sent over IPC (shared memory IDs, etc).
pub const MAX_ID_LEN: usize = 256;

/// Upper bound for arbitrary protocol strings (UTF-8 bytes).
pub const MAX_IPC_STRING_BYTES: usize = 1024;

/// Upper bound for renderer crash strings.
pub const MAX_CRASH_REASON_LEN: usize = MAX_IPC_STRING_BYTES;

/// Sane bounds for device pixel ratio (DPR).
///
/// - `0.1` allows "weird" zoom states without rejecting legitimate pages.
/// - `16.0` is far beyond any real device DPR, but still prevents pathological values.
pub const MIN_DPR: f32 = 0.1;
pub const MAX_DPR: f32 = 16.0;

/// Description of a single shared memory buffer, created by the browser and mapped by the
/// renderer.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct FrameBufferDesc {
  /// Stable index used by the renderer when referring to this buffer.
  pub buffer_index: u32,

  /// Browser-chosen identifier used by the renderer to open/map the shared memory segment.
  pub shmem_id: String,

  /// Total size (in bytes) of the shared memory segment.
  pub byte_len: usize,

  /// Maximum width (in pixels) the renderer is allowed to write into this buffer.
  pub max_width_px: u32,

  /// Maximum height (in pixels) the renderer is allowed to write into this buffer.
  pub max_height_px: u32,

  /// Bytes between the start of consecutive rows.
  pub stride_bytes: usize,
}

impl FrameBufferDesc {
  /// Validates the description itself (i.e. internal consistency).
  ///
  /// This is primarily used by the renderer when receiving `SetFrameBuffers`, and by unit tests.
  pub fn validate(&self) -> Result<(), IpcError> {
    if self.shmem_id.is_empty() {
      return Err(protocol_violation("shared memory id is empty"));
    }
    if self.shmem_id.len() > MAX_ID_LEN {
      return Err(protocol_violation(format!(
        "shared memory id too long: {} (max {MAX_ID_LEN})",
        self.shmem_id.len()
      )));
    }
    if self.byte_len == 0 {
      return Err(protocol_violation("frame buffer byte_len must be non-zero"));
    }
    if self.max_width_px == 0 || self.max_height_px == 0 {
      return Err(protocol_violation(
        "frame buffer max_width_px/max_height_px must be non-zero",
      ));
    }
    if self.stride_bytes == 0 {
      return Err(protocol_violation("frame buffer stride_bytes must be non-zero"));
    }

    let max_width_usize = usize::try_from(self.max_width_px)
      .map_err(|_| protocol_violation("max_width_px does not fit in usize"))?;
    let min_row_bytes = max_width_usize
      .checked_mul(BYTES_PER_PIXEL)
      .ok_or_else(|| protocol_violation("arithmetic overflow while computing min_row_bytes"))?;
    if self.stride_bytes < min_row_bytes {
      return Err(protocol_violation(format!(
        "frame buffer stride_bytes={} is smaller than min_row_bytes={min_row_bytes}",
        self.stride_bytes
      )));
    }

    let max_height_usize = usize::try_from(self.max_height_px)
      .map_err(|_| protocol_violation("max_height_px does not fit in usize"))?;
    let required_bytes = self
      .stride_bytes
      .checked_mul(max_height_usize)
      .ok_or_else(|| protocol_violation("arithmetic overflow while computing required_bytes"))?;
    if required_bytes > self.byte_len {
      return Err(protocol_violation(format!(
        "frame buffer backing store too small: required={required_bytes} available={}",
        self.byte_len
      )));
    }

    Ok(())
  }
}

/// Shared scroll state reported alongside rendered frames.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(deny_unknown_fields)]
pub struct ScrollMetrics {
  pub scroll_x_px: u32,
  pub scroll_y_px: u32,
  pub content_width_px: u32,
  pub content_height_px: u32,
}

/// Messages sent from the **trusted browser** to the **untrusted renderer**.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub enum BrowserToRenderer {
  Hello { protocol_version: u32 },

  /// Replace the entire frame buffer set. `generation` monotonically increases for each update.
  SetFrameBuffers {
    generation: u64,
    buffers: Vec<FrameBufferDesc>,
  },

  /// Acknowledge that the browser has finished consuming a frame.
  ///
  /// In shared-memory / pooled-buffer transports, the renderer must not reuse or overwrite the
  /// corresponding buffer until it receives this ack.
  FrameAck { frame_seq: u64 },

  ReleaseFrameBuffer {
    generation: u64,
    buffer_index: u32,
  },

  /// Request that the renderer exit gracefully.
  ///
  /// The optional `reason` is intended for diagnostics only and must stay bounded.
  Shutdown { reason: Option<String> },
}

/// Messages sent from the **untrusted renderer** to the **trusted browser**.
///
/// These messages must be validated before use.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub enum RendererToBrowser {
  HelloAck { protocol_version: u32 },

  /// Acknowledges a [`BrowserToRenderer::Shutdown`] request.
  ShutdownAck,

  /// Indicates that a buffer contains a fully-rendered frame.
  ///
  /// The browser uses `generation` + `buffer_index` to locate the corresponding shared memory
  /// mapping and then reads `width_px`/`height_px` within the negotiated limits.
  FrameReady {
    generation: u64,
    buffer_index: u32,
    width_px: u32,
    height_px: u32,
    viewport_css: (u32, u32),
    dpr: f32,
    scroll_metrics: ScrollMetrics,
    /// True when the rendered document contains time-based effects (CSS animations/transitions,
    /// animated images, JS timers/rAF, etc).
    wants_ticks: bool,
  },

  /// The renderer encountered a fatal error and is about to exit (or has exited).
  ///
  /// Note: `reason` is a string (allocating), so callers must validate its length.
  Crashed { reason: String },
}

impl BrowserToRenderer {
  /// Deserialize a browser message from a framed payload and validate it before returning.
  ///
  /// This is the recommended entry point for the **renderer** process when receiving messages from
  /// the browser.
  pub fn decode_and_validate_payload(payload: &[u8]) -> Result<Self, IpcError> {
    let msg: Self = super::framing::decode_bincode_payload(payload)?;
    msg.validate()?;
    Ok(msg)
  }

  /// Validate a browser → renderer message.
  ///
  /// Even though the browser is trusted, this provides a deterministic, bounded contract for the
  /// renderer process and helps avoid self-DoS footguns (e.g. accidentally sending huge shutdown
  /// reasons).
  pub fn validate(&self) -> Result<(), IpcError> {
    match self {
      BrowserToRenderer::Hello { protocol_version } => {
        if *protocol_version != IPC_PROTOCOL_VERSION {
          return Err(protocol_violation(format!(
            "protocol version mismatch: got {protocol_version}, expected {IPC_PROTOCOL_VERSION}"
          )));
        }
        Ok(())
      }

      BrowserToRenderer::SetFrameBuffers { buffers, .. } => {
        if buffers.len() > MAX_FRAME_BUFFERS {
          return Err(protocol_violation(format!(
            "too many frame buffers: len {} (max {MAX_FRAME_BUFFERS})",
            buffers.len()
          )));
        }
        for (idx, desc) in buffers.iter().enumerate() {
          desc.validate()?;
          if desc.buffer_index != idx as u32 {
            return Err(protocol_violation(format!(
              "buffer_index {} out of range (buffer_count={})",
              desc.buffer_index,
              buffers.len()
            )));
          }
        }
        Ok(())
      }

      BrowserToRenderer::FrameAck { .. } => Ok(()),

      BrowserToRenderer::ReleaseFrameBuffer { .. } => Ok(()),
      BrowserToRenderer::FrameAck { .. } => Ok(()),

      BrowserToRenderer::Shutdown { reason } => {
        if let Some(reason) = reason {
          if reason.len() > MAX_IPC_STRING_BYTES {
            return Err(protocol_violation(format!(
              "shutdown reason too long: {} (max {MAX_IPC_STRING_BYTES})",
              reason.len()
            )));
          }
        }
        Ok(())
      }
    }
  }
}

/// State held by the browser to validate renderer messages that reference negotiated buffers.
#[derive(Debug, Clone)]
pub struct FrameBufferSet {
  pub generation: u64,
  pub buffers: Vec<FrameBufferDesc>,
}

impl FrameBufferSet {
  pub fn validate(&self) -> Result<(), IpcError> {
    if self.buffers.len() > MAX_FRAME_BUFFERS {
      return Err(protocol_violation(format!(
        "frame buffer list too large: len {} (max {MAX_FRAME_BUFFERS})",
        self.buffers.len()
      )));
    }
    for (idx, desc) in self.buffers.iter().enumerate() {
      desc.validate()?;
      // Keep the mapping indexable without a HashMap.
      if desc.buffer_index != idx as u32 {
        return Err(protocol_violation(format!(
          "buffer_index {} out of range (buffer_count={})",
          desc.buffer_index,
          self.buffers.len()
        )));
      }
    }
    Ok(())
  }

  pub fn get(&self, buffer_index: u32) -> Result<&FrameBufferDesc, IpcError> {
    let idx =
      usize::try_from(buffer_index).map_err(|_| protocol_violation("buffer_index does not fit in usize"))?;
    self.buffers.get(idx).ok_or_else(|| {
      protocol_violation(format!(
        "buffer_index {buffer_index} out of range (buffer_count={})",
        self.buffers.len()
      ))
    })
  }
}

/// Validation context for renderer → browser messages.
#[derive(Debug, Clone, Copy)]
pub struct RendererToBrowserValidationContext<'a> {
  /// Protocol version the browser expects.
  pub expected_protocol_version: u32,

  /// Current negotiated frame buffer set (if any).
  pub frame_buffers: Option<&'a FrameBufferSet>,
}

impl RendererToBrowser {
  /// Deserialize a renderer message from a framed payload and validate it before returning.
  ///
  /// This is the recommended entry point for the **browser** process when receiving messages from
  /// the untrusted renderer.
  pub fn decode_and_validate_payload(
    payload: &[u8],
    ctx: &RendererToBrowserValidationContext<'_>,
  ) -> Result<Self, IpcError> {
    let msg: Self = super::framing::decode_bincode_payload(payload)?;
    msg.validate(ctx)?;
    Ok(msg)
  }

  /// Validate an incoming renderer message before acting on it.
  pub fn validate(&self, ctx: &RendererToBrowserValidationContext<'_>) -> Result<(), IpcError> {
    match self {
      RendererToBrowser::HelloAck { protocol_version } => {
        if *protocol_version != ctx.expected_protocol_version {
          return Err(protocol_violation(format!(
            "protocol version mismatch: got {protocol_version}, expected {}",
            ctx.expected_protocol_version
          )));
        }
        Ok(())
      }

      RendererToBrowser::FrameReady {
        generation,
        buffer_index,
        width_px,
        height_px,
        viewport_css: _,
        dpr,
        scroll_metrics: _,
        wants_ticks: _,
      } => {
        let Some(frame_buffers) = ctx.frame_buffers else {
          return Err(protocol_violation(format!(
            "generation mismatch: got {generation}, expected 0"
          )));
        };
        if *generation != frame_buffers.generation {
          return Err(protocol_violation(format!(
            "generation mismatch: got {generation}, expected {}",
            frame_buffers.generation
          )));
        }

        if *width_px == 0 || *height_px == 0 {
          return Err(protocol_violation(format!(
            "frame dimensions must be non-zero (width_px={width_px}, height_px={height_px})"
          )));
        }

        if !dpr.is_finite() || *dpr < MIN_DPR || *dpr > MAX_DPR {
          return Err(protocol_violation(format!("invalid device pixel ratio {dpr}")));
        }

        let desc = frame_buffers.get(*buffer_index)?;

        if *width_px > desc.max_width_px || *height_px > desc.max_height_px {
          return Err(protocol_violation(format!(
            "frame dimensions exceed negotiated maximums: {width_px}x{height_px} > {}x{}",
            desc.max_width_px, desc.max_height_px
          )));
        }

        let width_usize =
          usize::try_from(*width_px).map_err(|_| protocol_violation("width_px does not fit in usize"))?;
        let height_usize =
          usize::try_from(*height_px).map_err(|_| protocol_violation("height_px does not fit in usize"))?;
        let row_bytes = width_usize
          .checked_mul(BYTES_PER_PIXEL)
          .ok_or_else(|| protocol_violation("arithmetic overflow while computing row_bytes"))?;
        if row_bytes > desc.stride_bytes {
          return Err(protocol_violation(format!(
            "frame row bytes {row_bytes} exceed stride_bytes {}",
            desc.stride_bytes
          )));
        }

        let required_bytes = desc
          .stride_bytes
          .checked_mul(height_usize)
          .ok_or_else(|| protocol_violation("arithmetic overflow while computing required_bytes"))?;
        if required_bytes > desc.byte_len {
          return Err(protocol_violation(format!(
            "frame exceeds shared memory buffer: required={required_bytes} available={}",
            desc.byte_len
          )));
        }

        Ok(())
      }

      RendererToBrowser::Crashed { reason } => {
        if reason.len() > MAX_CRASH_REASON_LEN {
          return Err(protocol_violation(format!(
            "crash reason too long: {} (max {MAX_CRASH_REASON_LEN})",
            reason.len()
          )));
        }
        Ok(())
      }

      RendererToBrowser::ShutdownAck => Ok(()),
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  fn mk_valid_buffers() -> FrameBufferSet {
    let desc = FrameBufferDesc {
      buffer_index: 0,
      shmem_id: "buf0".to_string(),
      byte_len: 400 * 100,
      max_width_px: 100,
      max_height_px: 100,
      stride_bytes: 400,
    };
    desc.validate().expect("desc should be valid");
    let buffers = FrameBufferSet {
      generation: 7,
      buffers: vec![desc],
    };
    buffers.validate().expect("buffer set should be valid");
    buffers
  }

  #[test]
  fn valid_frame_ready_passes() {
    let buffers = mk_valid_buffers();
    let ctx = RendererToBrowserValidationContext {
      expected_protocol_version: IPC_PROTOCOL_VERSION,
      frame_buffers: Some(&buffers),
    };
    let msg = RendererToBrowser::FrameReady {
      generation: buffers.generation,
      buffer_index: 0,
      width_px: 80,
      height_px: 60,
      viewport_css: (80, 60),
      dpr: 1.0,
      scroll_metrics: ScrollMetrics::default(),
      wants_ticks: false,
    };
    let payload = super::super::framing::encode_bincode_payload(&msg).expect("encode payload");
    let decoded =
      RendererToBrowser::decode_and_validate_payload(&payload, &ctx).expect("frame should validate");
    assert_eq!(decoded, msg);
  }

  #[test]
  fn invalid_buffer_index_rejected() {
    let buffers = mk_valid_buffers();
    let ctx = RendererToBrowserValidationContext {
      expected_protocol_version: IPC_PROTOCOL_VERSION,
      frame_buffers: Some(&buffers),
    };
    let msg = RendererToBrowser::FrameReady {
      generation: buffers.generation,
      buffer_index: 9,
      width_px: 1,
      height_px: 1,
      viewport_css: (1, 1),
      dpr: 1.0,
      scroll_metrics: ScrollMetrics::default(),
      wants_ticks: false,
    };
    let payload = super::super::framing::encode_bincode_payload(&msg).expect("encode payload");
    let err = RendererToBrowser::decode_and_validate_payload(&payload, &ctx)
      .expect_err("expected invalid buffer index");
    assert!(matches!(err, IpcError::ProtocolViolation { .. }));
  }

  #[test]
  fn width_height_overflow_rejected() {
    let buffers = mk_valid_buffers();
    let ctx = RendererToBrowserValidationContext {
      expected_protocol_version: IPC_PROTOCOL_VERSION,
      frame_buffers: Some(&buffers),
    };
    let msg = RendererToBrowser::FrameReady {
      generation: buffers.generation,
      buffer_index: 0,
      width_px: 101,
      height_px: 60,
      viewport_css: (1, 1),
      dpr: 1.0,
      scroll_metrics: ScrollMetrics::default(),
      wants_ticks: false,
    };
    let payload = super::super::framing::encode_bincode_payload(&msg).expect("encode payload");
    let err = RendererToBrowser::decode_and_validate_payload(&payload, &ctx)
      .expect_err("expected oversized dimensions to be rejected");
    assert!(matches!(err, IpcError::ProtocolViolation { .. }));
  }

  #[test]
  fn non_finite_dpr_rejected() {
    let buffers = mk_valid_buffers();
    let ctx = RendererToBrowserValidationContext {
      expected_protocol_version: IPC_PROTOCOL_VERSION,
      frame_buffers: Some(&buffers),
    };
    let msg = RendererToBrowser::FrameReady {
      generation: buffers.generation,
      buffer_index: 0,
      width_px: 1,
      height_px: 1,
      viewport_css: (1, 1),
      dpr: f32::NAN,
      scroll_metrics: ScrollMetrics::default(),
      wants_ticks: false,
    };
    let payload = super::super::framing::encode_bincode_payload(&msg).expect("encode payload");
    let err = RendererToBrowser::decode_and_validate_payload(&payload, &ctx)
      .expect_err("expected dpr to be rejected");
    assert!(matches!(err, IpcError::ProtocolViolation { .. }));
  }

  #[test]
  fn shutdown_reason_too_long_rejected() {
    let msg = BrowserToRenderer::Shutdown {
      reason: Some("a".repeat(MAX_IPC_STRING_BYTES + 1)),
    };
    let payload = super::super::framing::encode_bincode_payload(&msg).expect("encode payload");
    let err =
      BrowserToRenderer::decode_and_validate_payload(&payload).expect_err("expected shutdown to fail");
    assert!(matches!(err, IpcError::ProtocolViolation { .. }));
  }

  #[test]
  fn frame_ack_validates() {
    let msg = BrowserToRenderer::FrameAck { frame_seq: 123 };
    let payload = super::super::framing::encode_bincode_payload(&msg).expect("encode payload");
    let decoded =
      BrowserToRenderer::decode_and_validate_payload(&payload).expect("frame ack should validate");
    assert_eq!(decoded, msg);
  }
}
