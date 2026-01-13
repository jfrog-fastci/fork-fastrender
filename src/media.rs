//! Media frame plumbing for the paint pipeline.
//!
//! This module provides a small, paint-facing interface for supplying decoded media frames
//! (currently video; audio is a stub). The intent is to allow embedding applications to plug in
//! their own decoding/playback stack without pulling media decoding logic into FastRender.

use crate::geometry::Size;
use crate::paint::display_list::ImageData;
use std::sync::Arc;

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
/// Paint may call into this trait from multiple threads (e.g. rayon workers) while building and
/// rasterizing frames. Implementations **must** therefore be `Send + Sync`.
///
/// Implementations are expected to be **non-blocking**: do not perform I/O, decode work, or waits
/// inside these methods. Instead, decode in the background and return the most recent cached frame.
///
/// Returning `None` indicates that no decoded frame is currently available; the paint pipeline will
/// fall back to other rendering (poster image, placeholders, etc).
pub trait MediaFrameProvider: Send + Sync + 'static {
  /// Returns the current decoded video frame for the `<video>` element identified by
  /// (`box_id`, `src`), if available.
  fn video_frame(
    &self,
    box_id: Option<usize>,
    src: &str,
    size_hint: Option<MediaFrameSizeHint>,
  ) -> Option<Arc<ImageData>>;

  /// Returns the current decoded audio frame for the `<audio>` element identified by
  /// (`box_id`, `src`), if available.
  ///
  /// Audio plumbing is not yet integrated into the paint pipeline; this exists as a forward-looking
  /// stub and currently defaults to `None`.
  fn audio_frame(&self, _box_id: Option<usize>, _src: &str) -> Option<AudioFrame> {
    None
  }
}

/// Placeholder data model for an audio frame.
///
/// This will be expanded once the rendering pipeline has an audio consumer.
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

