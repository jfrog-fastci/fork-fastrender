//! Browser ↔ renderer IPC protocol definitions.
//!
//! This module intentionally uses only serde-friendly primitives (plus a few small helper structs)
//! so messages can cross a process boundary safely. Large payloads like pixel buffers must be sent
//! out-of-band via file descriptor (FD) attachments (e.g. shared memory).

use crate::ipc::protocol::cancel::CancelGensSnapshot;
use crate::ipc::IpcError;
use bincode::Options;
use serde::de::Error as _;
use serde::{Deserialize, Serialize};

/// Protocol version for Browser ↔ Renderer messages.
///
/// Bump this when breaking the serialized schema.
pub const RENDERER_PROTOCOL_VERSION: u32 = 2;

/// Maximum number of bytes the transport is allowed to decode for a single message payload.
///
/// The IPC transport must enforce this via `bincode::Options::with_limit` (or equivalent).
///
/// Note: this limit only applies to the serialized message body. Large binary payloads should be
/// passed as FD attachments and are counted separately.
pub const RENDERER_IPC_DECODE_LIMIT_BYTES: u64 = 256 * 1024;

/// Conservative upper bound for URLs carried in control messages.
///
/// This is an explicit semantic cap in addition to the transport-wide decode limit.
pub const MAX_URL_BYTES: usize = 8 * 1024;

/// Maximum number of files that can be transferred in a single file-input message.
pub const FILE_INPUT_MAX_FILES: usize = 16;
/// Maximum UTF-8 byte length of an individual file name.
pub const FILE_INPUT_MAX_NAME_BYTES: usize = 256;
/// Maximum total bytes (sum of `FileMeta.size`) allowed in a single file-input message.
///
/// This is defensive: the renderer must still read the actual bytes from the attached FDs until EOF.
pub const FILE_INPUT_MAX_TOTAL_BYTES_META: u64 = 512 * 1024 * 1024; // 512 MiB

/// Sanity bounds for device pixel ratio values received from the renderer.
///
/// Values outside this range are treated as protocol violations.
pub const MIN_DPR: f32 = 0.1;
pub const MAX_DPR: f32 = 16.0;

/// Return the canonical bincode options for renderer IPC decoding.
///
/// The transport should use these options (or an equivalent configuration) when decoding messages
/// from an untrusted peer.
pub fn bincode_options() -> impl bincode::Options {
  bincode::options().with_limit(RENDERER_IPC_DECODE_LIMIT_BYTES)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BoundedStringTooLong {
  pub len: usize,
  pub max: usize,
}

/// A UTF-8 string with an explicit max byte length.
///
/// This is primarily a protocol hardening tool: fields like URLs should not be allowed to balloon
/// even when the global decode limit is raised.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct BoundedString<const MAX: usize>(String);

impl<const MAX: usize> BoundedString<MAX> {
  pub fn new(value: impl Into<String>) -> Result<Self, BoundedStringTooLong> {
    let value = value.into();
    if value.len() > MAX {
      return Err(BoundedStringTooLong {
        len: value.len(),
        max: MAX,
      });
    }
    Ok(Self(value))
  }

  pub fn as_str(&self) -> &str {
    &self.0
  }

  pub fn into_string(self) -> String {
    self.0
  }
}

impl<const MAX: usize> std::ops::Deref for BoundedString<MAX> {
  type Target = str;

  fn deref(&self) -> &Self::Target {
    self.as_str()
  }
}

impl<const MAX: usize> TryFrom<String> for BoundedString<MAX> {
  type Error = BoundedStringTooLong;

  fn try_from(value: String) -> Result<Self, Self::Error> {
    Self::new(value)
  }
}

impl<const MAX: usize> TryFrom<&str> for BoundedString<MAX> {
  type Error = BoundedStringTooLong;

  fn try_from(value: &str) -> Result<Self, Self::Error> {
    Self::new(value)
  }
}

impl<const MAX: usize> From<BoundedString<MAX>> for String {
  fn from(value: BoundedString<MAX>) -> Self {
    value.into_string()
  }
}

impl<const MAX: usize> Serialize for BoundedString<MAX> {
  fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
    serializer.serialize_str(&self.0)
  }
}

impl<'de, const MAX: usize> Deserialize<'de> for BoundedString<MAX> {
  fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
    let value = String::deserialize(deserializer)?;
    if value.len() > MAX {
      return Err(D::Error::custom(format!(
        "string too long: {} bytes (max {})",
        value.len(),
        MAX
      )));
    }
    Ok(Self(value))
  }
}

pub type UrlString = BoundedString<MAX_URL_BYTES>;

/// File metadata accompanying a read-only FD-attached file payload.
///
/// For security, the renderer must treat the accompanying file descriptor as the source of truth:
/// - Read until EOF for the actual byte length.
/// - Do **not** attempt to open any host paths (the renderer is expected to be sandboxed).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileMeta {
  /// Basename only (no host paths).
  pub name: String,
  /// Best-effort file size in bytes.
  pub size: u64,
  /// Best-effort MIME type hint.
  pub mime: Option<String>,
}

impl FileMeta {
  fn validate(&self) -> Result<(), IpcError> {
    let name_len = self.name.as_bytes().len();
    if name_len > FILE_INPUT_MAX_NAME_BYTES {
      return Err(IpcError::ProtocolViolation {
        msg: format!(
          "file name too long: {name_len} bytes (max {FILE_INPUT_MAX_NAME_BYTES})"
        ),
      });
    }

    // Enforce "basename only" so the browser can't leak host paths to the renderer.
    if self.name.contains('/') || self.name.contains('\\') {
      return Err(IpcError::ProtocolViolation {
        msg: format!(
          "file name must be a basename (no path separators): {:?}",
          self.name
        ),
      });
    }

    Ok(())
  }
}

fn validate_files(files: &[FileMeta]) -> Result<(), IpcError> {
  if files.len() > FILE_INPUT_MAX_FILES {
    return Err(IpcError::ProtocolViolation {
      msg: format!(
        "too many files: len {} (max {FILE_INPUT_MAX_FILES})",
        files.len()
      ),
    });
  }

  let mut total: u128 = 0;
  for file in files {
    file.validate()?;
    total += u128::from(file.size);
  }

  if total > u128::from(FILE_INPUT_MAX_TOTAL_BYTES_META) {
    let total_u64 = u64::try_from(total).unwrap_or(u64::MAX);
    return Err(IpcError::ProtocolViolation {
      msg: format!(
        "total file size metadata too large: {total_u64} bytes (max {FILE_INPUT_MAX_TOTAL_BYTES_META})"
      ),
    });
  }

  Ok(())
}

/// Descriptor for a rendered frame whose pixels are carried out-of-band in an attached FD.
///
/// The corresponding `RendererToBrowser::FrameReady` must be accompanied by exactly **one** FD
/// whose contents are `byte_len` bytes of **premultiplied RGBA8** pixels laid out row-major with
/// `stride_bytes` bytes per row.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SharedFrameDescriptor {
  pub width_px: u32,
  pub height_px: u32,
  pub stride_bytes: u32,
  pub byte_len: u64,
}

impl SharedFrameDescriptor {
  pub const BYTES_PER_PIXEL_RGBA8: u32 = 4;

  pub fn new_rgba8(width_px: u32, height_px: u32) -> Self {
    let stride_bytes = width_px.saturating_mul(Self::BYTES_PER_PIXEL_RGBA8);
    let byte_len = stride_bytes as u64 * height_px as u64;
    Self {
      width_px,
      height_px,
      stride_bytes,
      byte_len,
    }
  }

  pub fn validate(&self) -> Result<(), IpcError> {
    if self.width_px == 0 || self.height_px == 0 {
      return Err(IpcError::ProtocolViolation {
        msg: format!(
          "frame dimensions must be non-zero (width_px={}, height_px={})",
          self.width_px, self.height_px
        ),
      });
    }
    if self.stride_bytes == 0 {
      return Err(IpcError::ProtocolViolation {
        msg: "frame stride_bytes must be non-zero".to_string(),
      });
    }
    if self.byte_len == 0 {
      return Err(IpcError::ProtocolViolation {
        msg: "frame byte_len must be non-zero".to_string(),
      });
    }

    // Compute row length in bytes (RGBA8) with checked arithmetic.
    let row_bytes_u64 = u64::from(self.width_px)
      .checked_mul(u64::from(Self::BYTES_PER_PIXEL_RGBA8))
      .ok_or_else(|| IpcError::ProtocolViolation {
        msg: "arithmetic overflow while computing row_bytes".to_string(),
      })?;
    let stride_u64 = u64::from(self.stride_bytes);
    if stride_u64 < row_bytes_u64 {
      return Err(IpcError::ProtocolViolation {
        msg: format!("frame row bytes {row_bytes_u64} exceed stride_bytes {stride_u64}"),
      });
    }

    // Minimum number of bytes required to address `height_px` rows at the given stride.
    // We do not require the mapping to include padding after the final row.
    let height_u64 = u64::from(self.height_px);
    let required_bytes_u64 = height_u64
      .checked_sub(1)
      .and_then(|h_minus_1| h_minus_1.checked_mul(stride_u64))
      .and_then(|prefix| prefix.checked_add(row_bytes_u64))
      .ok_or_else(|| IpcError::ProtocolViolation {
        msg: "arithmetic overflow while computing required_bytes".to_string(),
      })?;

    if required_bytes_u64 > self.byte_len {
      return Err(IpcError::ProtocolViolation {
        msg: format!(
          "frame exceeds buffer length: required={required_bytes_u64} available={}",
          self.byte_len
        ),
      });
    }

    // Cap the overall mapping length to match the in-process pixmap guardrail.
    let max = crate::paint::pixmap::MAX_PIXMAP_BYTES;
    if self.byte_len > max {
      return Err(IpcError::ProtocolViolation {
        msg: format!("frame buffer byte_len {} exceeds max {}", self.byte_len, max),
      });
    }

    Ok(())
  }
}

/// Minimal scroll bounds information for the root scroll container (viewport).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScrollBoundsMinimal {
  pub min_x: f32,
  pub min_y: f32,
  pub max_x: f32,
  pub max_y: f32,
}

impl ScrollBoundsMinimal {
  fn validate(&self) -> Result<(), IpcError> {
    let all_finite = self.min_x.is_finite()
      && self.min_y.is_finite()
      && self.max_x.is_finite()
      && self.max_y.is_finite();
    if !all_finite {
      return Err(IpcError::ProtocolViolation {
        msg: "scroll bounds contain non-finite floats".to_string(),
      });
    }
    if self.min_x > self.max_x || self.min_y > self.max_y {
      return Err(IpcError::ProtocolViolation {
        msg: "scroll bounds min > max".to_string(),
      });
    }
    Ok(())
  }
}

/// Minimal scroll state information surfaced to the browser/UI process.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScrollStateMinimal {
  /// Current viewport scroll offset in CSS pixels.
  pub viewport_scroll_css: (f32, f32),
  /// Scrollable bounds for the root scroll container in CSS pixels.
  pub bounds_css: ScrollBoundsMinimal,
}

impl ScrollStateMinimal {
  fn validate(&self) -> Result<(), IpcError> {
    let (x, y) = self.viewport_scroll_css;
    if !x.is_finite() || !y.is_finite() {
      return Err(IpcError::ProtocolViolation {
        msg: "viewport_scroll_css contains non-finite floats".to_string(),
      });
    }
    self.bounds_css.validate()
  }
}

fn validate_dpr(dpr: f32) -> Result<(), IpcError> {
  if !dpr.is_finite() || dpr < MIN_DPR || dpr > MAX_DPR {
    return Err(IpcError::ProtocolViolation {
      msg: format!("invalid device pixel ratio {dpr}"),
    });
  }
  Ok(())
}

/// Messages sent from the (trusted) browser process to the (sandboxed) renderer process.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub enum BrowserToRenderer {
  /// Initial handshake from browser → renderer.
  Hello {
    version: u32,
    /// Capability bitset for feature negotiation.
    capabilities: u64,
  },

  CreateTab {
    tab_id: u64,
    initial_url: Option<UrlString>,
  },

  Navigate {
    tab_id: u64,
    url: UrlString,
    /// Opaque reason code (mirrors `ui::messages::NavigationReason` in-process).
    reason: u8,
  },

  /// Update the cooperative cancellation generations for a tab.
  ///
  /// The browser should send this when it bumps its local gens (e.g. before sending a new
  /// navigation or repaint request) so in-flight renderer work can cancel cooperatively.
  CancelUpdate { tab_id: u64, gens: CancelGensSnapshot },

  ViewportChanged {
    tab_id: u64,
    viewport_css: (u32, u32),
    dpr: f32,
  },

  PointerMove {
    tab_id: u64,
    pos_css: (f32, f32),
    button: u8,
    modifiers: u8,
  },

  PointerDown {
    tab_id: u64,
    pos_css: (f32, f32),
    button: u8,
    modifiers: u8,
    click_count: u8,
  },

  PointerUp {
    tab_id: u64,
    pos_css: (f32, f32),
    button: u8,
    modifiers: u8,
  },

  /// Drop one or more local files onto the page at `pos_css`.
  ///
  /// The message must be sent with `files.len()` attached read-only file descriptors, each
  /// positioned at the start of the file.
  DropFiles {
    tab_id: u64,
    pos_css: (f32, f32),
    files: Vec<FileMeta>,
  },

  Scroll {
    tab_id: u64,
    delta_css: (f32, f32),
    /// Pointer position in viewport-local CSS pixels, when known.
    pointer_css: Option<(f32, f32)>,
  },

  KeyAction {
    tab_id: u64,
    /// Opaque key action code (mirrors `interaction::KeyAction` in-process).
    key: u8,
    modifiers: u8,
  },

  /// User chose one or more files in a file picker popup for an `<input type=file>` control.
  ///
  /// The message must be sent with `files.len()` attached read-only file descriptors, each
  /// positioned at the start of the file.
  FilePickerChoose {
    tab_id: u64,
    input_node_id: u64,
    files: Vec<FileMeta>,
  },

  /// Acknowledge a previously delivered [`RendererToBrowser::FrameReady`] message.
  ///
  /// The browser must send this once it has either:
  /// - uploaded/copied the frame pixels out of the attached FD, or
  /// - decided to drop the frame without using it.
  ///
  /// After the ack is received, the renderer may reuse any resources associated with the frame
  /// (shared memory segment, pooled buffer, etc).
  FrameAck { frame_seq: u64 },

  /// Release a shared frame buffer slot back to the renderer.
  ///
  /// Some multiprocess experiments use a fixed pool of shared buffers identified by `(generation,
  /// buffer_index)` instead of per-frame FDs. In that mode the browser releases a buffer explicitly
  /// once it has copied/uploaded the pixels.
  ReleaseFrameBuffer { generation: u64, buffer_index: u32 },

  /// Close a tab (drop all associated renderer-side state).
  TabClosed { tab_id: u64 },

  /// Terminate the renderer process.
  Shutdown,
}

impl BrowserToRenderer {
  /// Returns the number of file descriptors that must accompany this message.
  pub fn expected_fds(&self) -> usize {
    match self {
      BrowserToRenderer::FilePickerChoose { files, .. }
      | BrowserToRenderer::DropFiles { files, .. } => files.len(),
      _ => 0,
    }
  }

  pub fn validate(&self) -> Result<(), IpcError> {
    match self {
      BrowserToRenderer::FilePickerChoose { files, .. }
      | BrowserToRenderer::DropFiles { files, .. } => validate_files(files),
      _ => Ok(()),
    }
  }
}

/// Messages sent from the renderer process to the browser process.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub enum RendererToBrowser {
  /// Handshake response from renderer → browser.
  HelloAck {},

  /// A new frame is available.
  ///
  /// The pixel buffer is not serialized; it must be supplied as a single attached FD.
  ///
  /// ## Flow control / shared-buffer lifetime
  ///
  /// The renderer must treat the attached pixel buffer (FD / shared memory mapping) as **in use**
  /// by the browser until it receives [`BrowserToRenderer::FrameAck`] for the same `frame_seq`.
  ///
  /// The renderer should cap the number of un-acked frames in flight (commonly 1–2) to avoid
  /// blocking indefinitely if the browser/UI process is slow. When saturated, it should drop
  /// intermediate frames (keep only the latest pending repaint) rather than queueing unbounded
  /// frames.
  FrameReady {
    tab_id: u64,
    /// Monotonically increasing renderer-chosen sequence number for this frame.
    ///
    /// Uniqueness is scoped to a single browser↔renderer connection.
    frame_seq: u64,
    frame: SharedFrameDescriptor,
    viewport_css: (u32, u32),
    dpr: f32,
    /// True when the rendered document contains time-based effects (CSS animations/transitions,
    /// animated images, JS timers/rAF, etc).
    wants_ticks: bool,
    scroll_state_minimal: ScrollStateMinimal,
  },
}

/// Default maximum number of un-acked frames a renderer should allow in-flight.
///
/// A value of `1` is simplest (strictly one frame at a time). `2` allows basic double-buffering
/// without letting a slow browser/UI backlog the renderer indefinitely.
pub const DEFAULT_MAX_FRAMES_IN_FLIGHT: usize = 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameFlowControlError {
  MaxInFlightZero,
  AckForUnknownFrame { frame_seq: u64 },
}

/// Outcome of attempting to send a new frame while enforcing an in-flight limit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameSendDecision {
  /// The frame may be sent now with the provided `frame_seq`.
  Send { frame_seq: u64 },
  /// The frame should be dropped (coalesced).
  ///
  /// Callers typically mark themselves "dirty" so they will render/send the latest frame once an
  /// ack arrives and capacity is available.
  Drop,
}

/// Minimal in-memory helper for renderer-side frame flow control.
///
/// This struct tracks:
/// - which `frame_seq` values are currently in flight, and
/// - whether at least one frame was dropped while saturated (meaning we should send another frame
///   once capacity is available).
///
/// It intentionally does **not** store frame contents; it is suitable for driving either:
/// - "render then try to send" pipelines, or
/// - "check capacity before rendering" pipelines.
#[derive(Debug, Clone)]
pub struct FrameInFlightCounter {
  max_in_flight: usize,
  next_seq: u64,
  in_flight: Vec<u64>,
  pending: bool,
}

impl FrameInFlightCounter {
  pub fn new(max_in_flight: usize) -> Result<Self, FrameFlowControlError> {
    if max_in_flight == 0 {
      return Err(FrameFlowControlError::MaxInFlightZero);
    }
    Ok(Self {
      max_in_flight,
      next_seq: 1,
      in_flight: Vec::with_capacity(max_in_flight),
      pending: false,
    })
  }

  pub fn max_in_flight(&self) -> usize {
    self.max_in_flight
  }

  pub fn in_flight_len(&self) -> usize {
    self.in_flight.len()
  }

  pub fn has_pending(&self) -> bool {
    self.pending
  }

  fn alloc_seq(&mut self) -> u64 {
    // `0` is reserved as an "invalid" sentinel.
    loop {
      let seq = self.next_seq;
      self.next_seq = self.next_seq.wrapping_add(1);
      if seq != 0 {
        return seq;
      }
    }
  }

  /// Called when the renderer has a new frame ready and wants to send it.
  pub fn on_frame_ready(&mut self) -> FrameSendDecision {
    if self.in_flight.len() >= self.max_in_flight {
      self.pending = true;
      return FrameSendDecision::Drop;
    }

    let seq = self.alloc_seq();
    self.in_flight.push(seq);
    self.pending = false;
    FrameSendDecision::Send { frame_seq: seq }
  }

  /// Called when the browser acks a previously received `frame_seq`.
  ///
  /// Returns `Ok(true)` when:
  /// - at least one frame was dropped while saturated (`pending == true`), and
  /// - there is now capacity to send another frame.
  ///
  /// Callers can use this as a cheap "wake up the render loop" signal.
  pub fn on_frame_acked(&mut self, frame_seq: u64) -> Result<bool, FrameFlowControlError> {
    let Some(pos) = self.in_flight.iter().position(|&s| s == frame_seq) else {
      return Err(FrameFlowControlError::AckForUnknownFrame { frame_seq });
    };
    self.in_flight.swap_remove(pos);
    Ok(self.pending && self.in_flight.len() < self.max_in_flight)
  }
}

impl RendererToBrowser {
  /// Validate a renderer → browser message.
  pub fn validate(&self) -> Result<(), IpcError> {
    match self {
      RendererToBrowser::HelloAck {} => Ok(()),
      RendererToBrowser::FrameReady {
        tab_id: _,
        frame_seq,
        frame,
        viewport_css: _,
        dpr,
        wants_ticks: _,
        scroll_state_minimal,
      } => {
        if *frame_seq == 0 {
          return Err(IpcError::ProtocolViolation {
            msg: "frame_seq must be non-zero".to_string(),
          });
        }
        frame.validate()?;
        validate_dpr(*dpr)?;
        scroll_state_minimal.validate()
      }
    }
  }

  /// Returns the number of file descriptors that must accompany this message.
  pub fn expected_fds(&self) -> usize {
    match self {
      RendererToBrowser::FrameReady { .. } => 1,
      RendererToBrowser::HelloAck {} => 0,
    }
  }
}

#[cfg(test)]
mod frame_flow_control {
  use super::*;
  use bincode::Options;

  fn roundtrip<T>(value: &T) -> T
  where
    T: Serialize + for<'de> Deserialize<'de> + PartialEq + std::fmt::Debug,
  {
    let bytes = bincode_options().serialize(value).unwrap();
    let decoded: T = bincode_options().deserialize(&bytes).unwrap();
    decoded
  }

  #[test]
  fn browser_to_renderer_roundtrips() {
    let msg = BrowserToRenderer::Hello {
      version: RENDERER_PROTOCOL_VERSION,
      capabilities: 0x1234,
    };
    assert_eq!(msg, roundtrip(&msg));

    let msg = BrowserToRenderer::FrameAck { frame_seq: 123 };
    assert_eq!(msg, roundtrip(&msg));

    let msg = BrowserToRenderer::Navigate {
      tab_id: 42,
      url: UrlString::try_from("https://example.com/").unwrap(),
      reason: 1,
    };
    assert_eq!(msg, roundtrip(&msg));

    let msg = BrowserToRenderer::CancelUpdate {
      tab_id: 42,
      gens: CancelGensSnapshot { nav: 1, paint: 2 },
    };
    assert_eq!(msg, roundtrip(&msg));

    let msg = BrowserToRenderer::FilePickerChoose {
      tab_id: 1,
      input_node_id: 99,
      files: vec![FileMeta {
        name: "a.txt".to_string(),
        size: 1,
        mime: None,
      }],
    };
    assert_eq!(msg, roundtrip(&msg));

    let msg = BrowserToRenderer::DropFiles {
      tab_id: 1,
      pos_css: (1.0, 2.0),
      files: vec![FileMeta {
        name: "b.bin".to_string(),
        size: 2,
        mime: Some("application/octet-stream".to_string()),
      }],
    };
    assert_eq!(msg, roundtrip(&msg));
  }

  #[test]
  fn renderer_to_browser_roundtrips() {
    let msg = RendererToBrowser::HelloAck {};
    assert_eq!(msg, roundtrip(&msg));

    let msg = RendererToBrowser::FrameReady {
      tab_id: 7,
      frame_seq: 1,
      frame: SharedFrameDescriptor::new_rgba8(800, 600),
      viewport_css: (800, 600),
      dpr: 2.0,
      wants_ticks: true,
      scroll_state_minimal: ScrollStateMinimal {
        viewport_scroll_css: (10.0, 20.0),
        bounds_css: ScrollBoundsMinimal {
          min_x: 0.0,
          min_y: 0.0,
          max_x: 100.0,
          max_y: 200.0,
        },
      },
    };
    assert_eq!(msg, roundtrip(&msg));
  }

  #[test]
  fn expected_fd_counts() {
    let url = UrlString::try_from("https://example.com/").unwrap();

    let b2r_zero_fd_cases = [
      BrowserToRenderer::Hello {
        version: RENDERER_PROTOCOL_VERSION,
        capabilities: 0,
      },
      BrowserToRenderer::CreateTab {
        tab_id: 1,
        initial_url: Some(url.clone()),
      },
      BrowserToRenderer::Navigate {
        tab_id: 1,
        url: url.clone(),
        reason: 0,
      },
      BrowserToRenderer::CancelUpdate {
        tab_id: 1,
        gens: CancelGensSnapshot { nav: 1, paint: 1 },
      },
      BrowserToRenderer::ViewportChanged {
        tab_id: 1,
        viewport_css: (800, 600),
        dpr: 1.0,
      },
      BrowserToRenderer::PointerMove {
        tab_id: 1,
        pos_css: (1.0, 2.0),
        button: 1,
        modifiers: 0,
      },
      BrowserToRenderer::PointerDown {
        tab_id: 1,
        pos_css: (1.0, 2.0),
        button: 1,
        modifiers: 0,
        click_count: 1,
      },
      BrowserToRenderer::PointerUp {
        tab_id: 1,
        pos_css: (1.0, 2.0),
        button: 1,
        modifiers: 0,
      },
      BrowserToRenderer::Scroll {
        tab_id: 1,
        delta_css: (0.0, 10.0),
        pointer_css: Some((5.0, 5.0)),
      },
      BrowserToRenderer::KeyAction {
        tab_id: 1,
        key: 0,
        modifiers: 0,
      },
      BrowserToRenderer::FrameAck { frame_seq: 9 },
      BrowserToRenderer::ReleaseFrameBuffer {
        generation: 1,
        buffer_index: 0,
      },
      BrowserToRenderer::TabClosed { tab_id: 1 },
      BrowserToRenderer::Shutdown,
    ];

    for msg in b2r_zero_fd_cases {
      assert_eq!(msg.expected_fds(), 0, "{msg:?}");
    }

    let msg = BrowserToRenderer::FilePickerChoose {
      tab_id: 1,
      input_node_id: 1,
      files: vec![
        FileMeta {
          name: "a.txt".to_string(),
          size: 0,
          mime: None,
        },
        FileMeta {
          name: "b.txt".to_string(),
          size: 0,
          mime: None,
        },
      ],
    };
    assert_eq!(msg.expected_fds(), 2);

    let msg = BrowserToRenderer::DropFiles {
      tab_id: 1,
      pos_css: (0.0, 0.0),
      files: vec![FileMeta {
        name: "c.txt".to_string(),
        size: 0,
        mime: None,
      }],
    };
    assert_eq!(msg.expected_fds(), 1);

    let r2b_cases = [
      (RendererToBrowser::HelloAck {}, 0),
      (
        RendererToBrowser::FrameReady {
          tab_id: 1,
          frame_seq: 1,
          frame: SharedFrameDescriptor::new_rgba8(1, 1),
          viewport_css: (1, 1),
          dpr: 1.0,
          wants_ticks: false,
          scroll_state_minimal: ScrollStateMinimal {
            viewport_scroll_css: (0.0, 0.0),
            bounds_css: ScrollBoundsMinimal {
              min_x: 0.0,
              min_y: 0.0,
              max_x: 0.0,
              max_y: 0.0,
            },
          },
        },
        1,
      ),
    ];

    for (msg, expected) in r2b_cases {
      assert_eq!(msg.expected_fds(), expected, "{msg:?}");
    }
  }

  #[test]
  fn bounded_string_enforces_max_len() {
    let too_long = "a".repeat(MAX_URL_BYTES + 1);
    assert!(UrlString::try_from(too_long).is_err());

    // Ensure the serde path enforces the same cap.
    let too_long = "b".repeat(MAX_URL_BYTES + 1);
    let bytes = bincode_options().serialize(&too_long).unwrap();
    let err = bincode_options().deserialize::<UrlString>(&bytes).unwrap_err();
    let formatted = format!("{err:?}");
    assert!(!formatted.is_empty());
  }

  #[test]
  fn frame_in_flight_counter_coalesces_and_wakes_on_ack() {
    let mut counter = FrameInFlightCounter::new(1).unwrap();
    assert_eq!(counter.max_in_flight(), 1);
    assert_eq!(counter.in_flight_len(), 0);
    assert!(!counter.has_pending());

    let first = counter.on_frame_ready();
    assert_eq!(first, FrameSendDecision::Send { frame_seq: 1 });
    assert_eq!(counter.in_flight_len(), 1);

    let dropped = counter.on_frame_ready();
    assert_eq!(dropped, FrameSendDecision::Drop);
    assert!(counter.has_pending());

    let err = counter.on_frame_acked(999).unwrap_err();
    assert_eq!(
      err,
      FrameFlowControlError::AckForUnknownFrame { frame_seq: 999 }
    );

    // Acking the in-flight frame frees capacity; since we dropped at least one frame, we should be
    // woken up to send a fresh one.
    assert_eq!(counter.on_frame_acked(1).unwrap(), true);
    assert_eq!(counter.in_flight_len(), 0);
    assert!(counter.has_pending(), "pending is cleared only once we send again");

    let second = counter.on_frame_ready();
    assert_eq!(second, FrameSendDecision::Send { frame_seq: 2 });
    assert_eq!(counter.in_flight_len(), 1);
    assert!(!counter.has_pending());
  }
}

#[cfg(test)]
mod file_inputs {
  use super::*;

  fn file(name: &str, size: u64) -> FileMeta {
    FileMeta {
      name: name.to_string(),
      size,
      mime: None,
    }
  }

  #[test]
  fn serialization_roundtrip_file_picker_choose() {
    let msg = BrowserToRenderer::FilePickerChoose {
      tab_id: 1,
      input_node_id: 99,
      files: vec![file("a.txt", 1), file("b.bin", 2)],
    };

    let bytes = bincode_options().serialize(&msg).expect("serialize");
    let decoded: BrowserToRenderer = bincode_options().deserialize(&bytes).expect("deserialize");
    assert_eq!(decoded, msg);
  }

  #[test]
  fn serialization_roundtrip_drop_files() {
    let msg = BrowserToRenderer::DropFiles {
      tab_id: 2,
      pos_css: (12.5, 42.0),
      files: vec![file("c.png", 3)],
    };

    let bytes = bincode_options().serialize(&msg).expect("serialize");
    let decoded: BrowserToRenderer = bincode_options().deserialize(&bytes).expect("deserialize");
    assert_eq!(decoded, msg);
  }

  #[test]
  fn expected_fds_matches_file_count() {
    let msg = BrowserToRenderer::DropFiles {
      tab_id: 1,
      pos_css: (0.0, 0.0),
      files: vec![file("a.txt", 0), file("b.txt", 0), file("c.txt", 0)],
    };
    assert_eq!(msg.expected_fds(), 3);
  }

  #[test]
  fn validator_rejects_too_many_files() {
    let mut files = Vec::new();
    for i in 0..(FILE_INPUT_MAX_FILES + 1) {
      files.push(file(&format!("f{i}.txt"), 1));
    }

    let msg = BrowserToRenderer::FilePickerChoose {
      tab_id: 1,
      input_node_id: 1,
      files,
    };

    let err = msg.validate().expect_err("expected validation failure");
    assert!(matches!(err, IpcError::ProtocolViolation { .. }));
  }

  #[test]
  fn validator_rejects_oversized_name() {
    let name = "a".repeat(FILE_INPUT_MAX_NAME_BYTES + 1);
    let msg = BrowserToRenderer::DropFiles {
      tab_id: 1,
      pos_css: (0.0, 0.0),
      files: vec![file(&name, 1)],
    };

    let err = msg.validate().expect_err("expected validation failure");
    assert!(matches!(err, IpcError::ProtocolViolation { .. }));
  }
}

#[cfg(test)]
mod renderer_to_browser_validation {
  use super::*;

  fn valid_scroll_state() -> ScrollStateMinimal {
    ScrollStateMinimal {
      viewport_scroll_css: (0.0, 0.0),
      bounds_css: ScrollBoundsMinimal {
        min_x: 0.0,
        min_y: 0.0,
        max_x: 100.0,
        max_y: 200.0,
      },
    }
  }

  #[test]
  fn frame_ready_validates() {
    let msg = RendererToBrowser::FrameReady {
      tab_id: 1,
      frame_seq: 1,
      frame: SharedFrameDescriptor::new_rgba8(16, 8),
      viewport_css: (800, 600),
      dpr: 2.0,
      wants_ticks: false,
      scroll_state_minimal: valid_scroll_state(),
    };
    msg.validate().unwrap();
  }

  #[test]
  fn frame_ready_rejects_non_finite_dpr() {
    let msg = RendererToBrowser::FrameReady {
      tab_id: 1,
      frame_seq: 1,
      frame: SharedFrameDescriptor::new_rgba8(1, 1),
      viewport_css: (1, 1),
      dpr: f32::NAN,
      wants_ticks: false,
      scroll_state_minimal: valid_scroll_state(),
    };
    let err = msg.validate().unwrap_err();
    assert!(matches!(err, IpcError::ProtocolViolation { .. }));
  }

  #[test]
  fn frame_ready_rejects_frame_seq_zero() {
    let msg = RendererToBrowser::FrameReady {
      tab_id: 1,
      frame_seq: 0,
      frame: SharedFrameDescriptor::new_rgba8(1, 1),
      viewport_css: (1, 1),
      dpr: 1.0,
      wants_ticks: false,
      scroll_state_minimal: valid_scroll_state(),
    };
    let err = msg.validate().unwrap_err();
    assert!(matches!(err, IpcError::ProtocolViolation { .. }));
  }

  #[test]
  fn frame_descriptor_rejects_stride_too_small() {
    let desc = SharedFrameDescriptor {
      width_px: 10,
      height_px: 10,
      stride_bytes: 1,
      byte_len: 1,
    };
    let err = desc.validate().unwrap_err();
    assert!(matches!(err, IpcError::ProtocolViolation { .. }));
  }

  #[test]
  fn frame_descriptor_rejects_byte_len_too_small() {
    let desc = SharedFrameDescriptor {
      width_px: 1,
      height_px: 2,
      stride_bytes: 4,
      byte_len: 4,
    };
    let err = desc.validate().unwrap_err();
    assert!(matches!(err, IpcError::ProtocolViolation { .. }));
  }

  #[test]
  fn frame_descriptor_rejects_byte_len_too_large() {
    let max = crate::paint::pixmap::MAX_PIXMAP_BYTES;
    let desc = SharedFrameDescriptor {
      width_px: 1,
      height_px: 1,
      stride_bytes: 4,
      byte_len: max + 1,
    };
    let err = desc.validate().unwrap_err();
    assert!(matches!(err, IpcError::ProtocolViolation { .. }));
  }
}
