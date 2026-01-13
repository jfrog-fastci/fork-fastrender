//! Browser ↔ renderer IPC protocol definitions.
//!
//! This module intentionally uses only serde-friendly primitives (plus a few small helper structs)
//! so messages can cross a process boundary safely. Large payloads like pixel buffers must be sent
//! out-of-band via file descriptor (FD) attachments (e.g. shared memory).

use crate::ipc::protocol::cancel::CancelGensSnapshot;
use bincode::Options;
use serde::de::Error as _;
use serde::{Deserialize, Serialize};

/// Protocol version for Browser ↔ Renderer messages.
///
/// Bump this when breaking the serialized schema.
pub const RENDERER_PROTOCOL_VERSION: u32 = 1;

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

/// Minimal scroll state information surfaced to the browser/UI process.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScrollStateMinimal {
  /// Current viewport scroll offset in CSS pixels.
  pub viewport_scroll_css: (f32, f32),
  /// Scrollable bounds for the root scroll container in CSS pixels.
  pub bounds_css: ScrollBoundsMinimal,
}

/// Messages sent from the (trusted) browser process to the (sandboxed) renderer process.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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

  /// Close a tab (drop all associated renderer-side state).
  TabClosed { tab_id: u64 },

  /// Terminate the renderer process.
  Shutdown,
}

impl BrowserToRenderer {
  /// Returns the number of file descriptors that must accompany this message.
  pub fn expected_fds(&self) -> usize {
    0
  }
}

/// Messages sent from the renderer process to the browser process.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum RendererToBrowser {
  /// Handshake response from renderer → browser.
  HelloAck {},

  /// A new frame is available.
  ///
  /// The pixel buffer is not serialized; it must be supplied as a single attached FD.
  FrameReady {
    tab_id: u64,
    frame: SharedFrameDescriptor,
    viewport_css: (u32, u32),
    dpr: f32,
    wants_ticks: bool,
    scroll_state_minimal: ScrollStateMinimal,
  },
}

impl RendererToBrowser {
  /// Returns the number of file descriptors that must accompany this message.
  pub fn expected_fds(&self) -> usize {
    match self {
      RendererToBrowser::FrameReady { .. } => 1,
      RendererToBrowser::HelloAck {} => 0,
    }
  }
}

#[cfg(test)]
mod tests {
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
  }

  #[test]
  fn renderer_to_browser_roundtrips() {
    let msg = RendererToBrowser::HelloAck {};
    assert_eq!(msg, roundtrip(&msg));

    let msg = RendererToBrowser::FrameReady {
      tab_id: 7,
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

    let b2r_cases = [
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
      BrowserToRenderer::TabClosed { tab_id: 1 },
      BrowserToRenderer::Shutdown,
    ];

    for msg in b2r_cases {
      assert_eq!(msg.expected_fds(), 0, "{msg:?}");
    }

    let r2b_cases = [
      (RendererToBrowser::HelloAck {}, 0),
      (
        RendererToBrowser::FrameReady {
          tab_id: 1,
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
}
