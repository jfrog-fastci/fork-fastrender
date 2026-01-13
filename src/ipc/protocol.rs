//! Multiprocess IPC protocol between the trusted browser process and the untrusted renderer.
//!
//! Design goals:
//! - Keep **renderer → browser** messages allocation-bounded and easy to validate.
//! - Prefer indices/handles over transferring bulk data.
//! - Validate every message from the renderer before acting on it.

use serde::{Deserialize, Serialize};

use super::IpcError;

/// Current IPC protocol version.
///
/// Bumped when message shapes or semantics change in an incompatible way.
pub const IPC_PROTOCOL_VERSION: u32 = 1;

/// Pixel format is currently fixed to premultiplied RGBA8.
pub const BYTES_PER_PIXEL: u32 = 4;

/// Hard cap for the number of frame buffers the browser can advertise.
pub const MAX_FRAME_BUFFERS: u32 = 32;

/// Upper bound for identifiers sent over IPC (shared memory IDs, etc).
pub const MAX_ID_LEN: u32 = 256;

/// Upper bound for renderer crash strings.
pub const MAX_CRASH_REASON_LEN: u32 = 1024;

/// Sane bounds for device pixel ratio (DPR).
///
/// - `0.1` allows "weird" zoom states without rejecting legitimate pages.
/// - `16.0` is far beyond any real device DPR, but still prevents pathological values.
pub const MIN_DPR: f32 = 0.1;
pub const MAX_DPR: f32 = 16.0;

/// Description of a single shared memory buffer, created by the browser and mapped by the
/// renderer.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FrameBufferDesc {
  /// Stable index used by the renderer when referring to this buffer.
  pub buffer_index: u32,

  /// Browser-chosen identifier used by the renderer to open/map the shared memory segment.
  pub shmem_id: String,

  /// Total size (in bytes) of the shared memory segment.
  pub byte_len: u32,

  /// Maximum width (in pixels) the renderer is allowed to write into this buffer.
  pub max_width_px: u32,

  /// Maximum height (in pixels) the renderer is allowed to write into this buffer.
  pub max_height_px: u32,

  /// Bytes between the start of consecutive rows.
  pub stride_bytes: u32,
}

impl FrameBufferDesc {
  /// Validates the description itself (i.e. internal consistency).
  ///
  /// This is primarily used by the renderer when receiving `SetFrameBuffers`, and by unit tests.
  pub fn validate(&self) -> Result<(), IpcError> {
    if self.shmem_id.is_empty() {
      return Err(IpcError::EmptyId);
    }
    if self.shmem_id.len() > MAX_ID_LEN as _ {
      return Err(IpcError::IdTooLong {
        len: self.shmem_id.len(),
        max: MAX_ID_LEN as _,
      });
    }
    if self.byte_len == 0 {
      return Err(IpcError::FrameBufferByteLenZero);
    }
    if self.max_width_px == 0 || self.max_height_px == 0 {
      return Err(IpcError::FrameBufferMaxDimensionsZero);
    }
    if self.stride_bytes == 0 {
      return Err(IpcError::FrameBufferStrideZero);
    }

    let min_row_bytes = u64::from(self.max_width_px)
      .checked_mul(u64::from(BYTES_PER_PIXEL))
      .ok_or(IpcError::ArithmeticOverflow)?;
    let min_row_bytes_u32 = u32::try_from(min_row_bytes).map_err(|_| IpcError::ArithmeticOverflow)?;
    if self.stride_bytes < min_row_bytes_u32 {
      return Err(IpcError::FrameBufferStrideTooSmall {
        stride_bytes: self.stride_bytes as _,
        min_row_bytes: min_row_bytes_u32 as _,
      });
    }

    let required_bytes = u64::from(self.stride_bytes)
      .checked_mul(u64::from(self.max_height_px))
      .ok_or(IpcError::ArithmeticOverflow)?;
    let required_bytes_u32 =
      u32::try_from(required_bytes).map_err(|_| IpcError::ArithmeticOverflow)?;
    if required_bytes_u32 > self.byte_len {
      return Err(IpcError::FrameBufferTooSmall {
        required_bytes: required_bytes_u32 as _,
        byte_len: self.byte_len as _,
      });
    }

    Ok(())
  }
}

/// Shared scroll state reported alongside rendered frames.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct ScrollMetrics {
  pub scroll_x_px: u32,
  pub scroll_y_px: u32,
  pub content_width_px: u32,
  pub content_height_px: u32,
}

/// Messages sent from the **trusted browser** to the **untrusted renderer**.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum BrowserToRenderer {
  Hello {
    protocol_version: u32,
  },

  /// Replace the entire frame buffer set. `generation` monotonically increases for each update.
  SetFrameBuffers {
    generation: u64,
    buffers: Vec<FrameBufferDesc>,
  },

  ReleaseFrameBuffer {
    generation: u64,
    buffer_index: u32,
  },
}

/// Messages sent from the **untrusted renderer** to the **trusted browser**.
///
/// These messages must be validated before use.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum RendererToBrowser {
  HelloAck {
    protocol_version: u32,
  },

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
    wants_ticks: bool,
  },

  /// The renderer encountered a fatal error and is about to exit (or has exited).
  ///
  /// Note: `reason` is a string (allocating), so callers must validate its length.
  Crashed {
    reason: String,
  },
}

/// State held by the browser to validate renderer messages that reference negotiated buffers.
#[derive(Debug, Clone)]
pub struct FrameBufferSet {
  pub generation: u64,
  pub buffers: Vec<FrameBufferDesc>,
}

impl FrameBufferSet {
  pub fn validate(&self) -> Result<(), IpcError> {
    if self.buffers.len() > MAX_FRAME_BUFFERS as _ {
      return Err(IpcError::TooManyFrameBuffers {
        len: self.buffers.len(),
        max: MAX_FRAME_BUFFERS as _,
      });
    }
    for (idx, desc) in self.buffers.iter().enumerate() {
      desc.validate()?;
      // Keep the mapping indexable without a HashMap.
      if desc.buffer_index != idx as u32 {
        return Err(IpcError::InvalidBufferIndex {
          buffer_index: desc.buffer_index,
          buffer_count: self.buffers.len(),
        });
      }
    }
    Ok(())
  }

  pub fn get(&self, buffer_index: u32) -> Result<&FrameBufferDesc, IpcError> {
    let idx = buffer_index.try_into().map_err(|_| IpcError::ArithmeticOverflow)?;
    self.buffers.iter().nth(idx).ok_or(IpcError::InvalidBufferIndex {
      buffer_index,
      buffer_count: self.buffers.len(),
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
          return Err(IpcError::ProtocolVersionMismatch {
            got: *protocol_version,
            expected: ctx.expected_protocol_version,
          });
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
          return Err(IpcError::GenerationMismatch {
            got: *generation,
            expected: 0,
          });
        };
        if *generation != frame_buffers.generation {
          return Err(IpcError::GenerationMismatch {
            got: *generation,
            expected: frame_buffers.generation,
          });
        }

        if *width_px == 0 || *height_px == 0 {
          return Err(IpcError::FrameDimensionsZero {
            width_px: *width_px,
            height_px: *height_px,
          });
        }

        if !dpr.is_finite() || *dpr < MIN_DPR || *dpr > MAX_DPR {
          return Err(IpcError::InvalidDpr { dpr: *dpr });
        }

        let desc = frame_buffers.get(*buffer_index)?;

        if *width_px > desc.max_width_px || *height_px > desc.max_height_px {
          return Err(IpcError::FrameDimensionsExceedMax {
            width_px: *width_px,
            height_px: *height_px,
            max_width_px: desc.max_width_px,
            max_height_px: desc.max_height_px,
          });
        }

        let row_bytes = u64::from(*width_px)
          .checked_mul(u64::from(BYTES_PER_PIXEL))
          .ok_or(IpcError::ArithmeticOverflow)?;
        let row_bytes_u32 = u32::try_from(row_bytes).map_err(|_| IpcError::ArithmeticOverflow)?;
        if row_bytes_u32 > desc.stride_bytes {
          return Err(IpcError::FrameRowBytesExceedStride {
            row_bytes: row_bytes_u32 as _,
            stride_bytes: desc.stride_bytes as _,
          });
        }

        let required_bytes = u64::from(desc.stride_bytes)
          .checked_mul(u64::from(*height_px))
          .ok_or(IpcError::ArithmeticOverflow)?;
        let required_bytes_u32 =
          u32::try_from(required_bytes).map_err(|_| IpcError::ArithmeticOverflow)?;
        if required_bytes_u32 > desc.byte_len {
          return Err(IpcError::FrameExceedsBufferLen {
            required_bytes: required_bytes_u32 as _,
            byte_len: desc.byte_len as _,
          });
        }

        Ok(())
      }

      RendererToBrowser::Crashed { reason } => {
        if reason.len() > MAX_CRASH_REASON_LEN as _ {
          return Err(IpcError::CrashReasonTooLong {
            len: reason.len(),
            max: MAX_CRASH_REASON_LEN as _,
          });
        }
        Ok(())
      }
    }
  }
}

// Compile-time guard: the IPC protocol module must not mention architecture-dependent pointer-sized
// integers in the serialized message surface.
//
// This is enforced textually so accidental reintroductions are caught immediately.
const _: () = {
  const SRC: &[u8] = include_bytes!("protocol.rs");
  const FORBIDDEN: [u8; 5] = [0x75, 0x73, 0x69, 0x7a, 0x65]; // "u" "s" "i" "z" "e"

  const fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    if needle.is_empty() {
      return false;
    }
    let mut i = 0;
    while i + needle.len() <= haystack.len() {
      let mut j = 0;
      while j < needle.len() {
        if haystack[i + j] != needle[j] {
          break;
        }
        j += 1;
      }
      if j == needle.len() {
        return true;
      }
      i += 1;
    }
    false
  }

  if contains(SRC, &FORBIDDEN) {
    panic!("ipc protocol contains a forbidden architecture-dependent integer type");
  }
};

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
    assert!(matches!(err, IpcError::InvalidBufferIndex { .. }));
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
    assert!(matches!(err, IpcError::FrameDimensionsExceedMax { .. }));
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
    assert!(matches!(err, IpcError::InvalidDpr { .. }));
  }
}
