//! Minimal media module for `renderer_tools` builds.
//!
//! The full `crate::media` subsystem (demuxers/codecs/audio clocks/etc.) is intentionally omitted
//! for offline renderer tooling builds (fixture rendering/diffing). These tools only need the
//! paint-facing `MediaFrameProvider` trait so `<video>` elements can request decoded frames.
//! In `renderer_tools` builds we provide a no-op implementation that always returns `None`.

use crate::error::RenderError;
use crate::geometry::Size;
use crate::paint::display_list::ImageData;
use std::borrow::Cow;
use std::sync::Arc;
use thiserror::Error;

/// Size information that can help a [`MediaFrameProvider`] choose an appropriate decode/scale
/// strategy.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MediaFrameSizeHint {
  /// The element's content box size in CSS pixels.
  pub css_size: Size,
  /// The device pixel ratio that the frame will be rasterized at.
  pub device_pixel_ratio: f32,
}

impl MediaFrameSizeHint {
  /// Creates a new size hint.
  pub const fn new(css_size: Size, device_pixel_ratio: f32) -> Self {
    Self {
      css_size,
      device_pixel_ratio,
    }
  }

  /// Returns the approximate desired size in device pixels.
  pub fn device_pixel_size(self) -> Size {
    self.css_size.scale(self.device_pixel_ratio)
  }
}

/// A paint-facing provider of decoded media frames.
///
/// In `renderer_tools` builds this exists only so the paint pipeline can compile. The default
/// [`NullMediaFrameProvider`] never returns frames.
pub trait MediaFrameProvider: Send + Sync + 'static {
  /// Returns the current decoded video frame for the `<video>` element identified by (`box_id`,
  /// `src`), if available.
  fn video_frame(
    &self,
    box_id: Option<usize>,
    src: &str,
    size_hint: Option<MediaFrameSizeHint>,
  ) -> Option<Arc<ImageData>>;

  /// Placeholder data model for an audio frame.
  ///
  /// Audio is not wired into the render pipeline yet, but the trait keeps this forward-looking
  /// method for API compatibility with full builds.
  fn audio_frame(&self, _box_id: Option<usize>, _src: &str) -> Option<AudioFrame> {
    None
  }
}

/// Placeholder data model for an audio frame.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AudioFrame;

/// A no-op [`MediaFrameProvider`] implementation that never returns frames.
#[derive(Debug, Clone, Copy, Default)]
pub struct NullMediaFrameProvider;

impl MediaFrameProvider for NullMediaFrameProvider {
  fn video_frame(
    &self,
    _box_id: Option<usize>,
    _src: &str,
    _size_hint: Option<MediaFrameSizeHint>,
  ) -> Option<Arc<ImageData>> {
    None
  }
}

// Keep the error surface compatible with the full `media` module so `crate::error` can convert
// `MediaError` into the top-level `Error` without additional cfg-gating.
pub type MediaResult<T> = std::result::Result<T, MediaError>;

#[derive(Debug, Error)]
pub enum MediaError {
  #[error("failed to load media from '{url}': {reason}")]
  LoadFailed { url: String, reason: String },

  #[error("i/o error: {0}")]
  Io(#[from] std::io::Error),

  #[error("render error: {0}")]
  Render(#[from] RenderError),

  #[error("unsupported: {0}")]
  Unsupported(Cow<'static, str>),

  #[error("demux error: {0}")]
  Demux(String),

  #[error("decode error: {0}")]
  Decode(String),
}

