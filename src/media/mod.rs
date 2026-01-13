//! Media utilities and shared primitives.
//!
//! This module currently provides:
//! - timestamp/timebase helpers used by media playback work
//! - media clock helpers
//! - paint-facing plumbing for supplying decoded media frames (video; audio is currently a stub)
//! - container demux primitives (track metadata + compressed packets)
//! - codec backends for decoding compressed packets into PCM / frames
//!
//! For the intended A/V clocking model (audio master clock, UI tick as wake-up only), see
//! `docs/media_clocking.md`.

use crate::geometry::Size;
use crate::error::RenderError;
use crate::paint::display_list::ImageData;
use std::sync::Arc;
use thiserror::Error;

pub mod av_sync;
pub mod audio;
pub mod audio_clock;
pub mod audio_engine;
pub mod backends;
pub mod clock;
pub mod codecs;
pub mod decoder;
pub mod demux;
pub mod demuxer;
pub mod loader;
pub mod master_clock;
pub mod mp4;
pub mod packet;
pub mod pipeline;
pub mod timestamp;
pub mod timebase;
pub mod yuv;

pub use audio_clock::InterpolatedAudioClock;
pub use av_sync::AvSyncConfig;
pub use clock::{AudioDeviceClock, AudioStreamClock, MediaClock, PlaybackClock, RealAudioDeviceClock};
pub use master_clock::{ClockSource, MasterClock};
pub use mp4::{Mp4Demuxer, Mp4Sample, Mp4Track, SeekMethod};
pub use packet::{MediaData, MediaPacket};
pub use pipeline::MediaDecodePipeline;
pub use timestamp::MediaTimestamp;
pub use timebase::{
  duration_to_ticks,
  ticks_to_duration,
  ticks_to_timestamp,
  timestamp_to_ticks,
  Timebase,
};

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
  /// Returns the current decoded video frame for the `<video>` element identified by (`box_id`,
  /// `src`), if available.
  fn video_frame(
    &self,
    box_id: Option<usize>,
    src: &str,
    size_hint: Option<MediaFrameSizeHint>,
  ) -> Option<Arc<ImageData>>;

  /// Returns the current decoded audio frame for the `<audio>` element identified by (`box_id`,
  /// `src`), if available.
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

// ============================================================================
// Demux primitives
// ============================================================================

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
  Unsupported(&'static str),

  #[error("demux error: {0}")]
  Demux(String),

  #[error("decode error: {0}")]
  Decode(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaTrackType {
  Video,
  Audio,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MediaCodec {
  H264,
  Vp9,
  Opus,
  H264,
  Aac,
  Unknown(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MediaVideoInfo {
  pub width: u32,
  pub height: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MediaAudioInfo {
  pub sample_rate: u32,
  pub channels: u16,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MediaTrackInfo {
  pub id: u64,
  pub track_type: MediaTrackType,
  pub codec: MediaCodec,
  /// Codec-private data ("extradata").
  pub codec_private: Vec<u8>,
  pub codec_delay_ns: u64,
  pub video: Option<MediaVideoInfo>,
  pub audio: Option<MediaAudioInfo>,
}

/// Interleaved PCM audio decoded from a compressed packet.
#[derive(Debug, Clone)]
pub struct DecodedAudioChunk {
  /// Presentation timestamp of the first sample (nanoseconds).
  pub pts_ns: u64,
  /// Duration covered by this chunk (nanoseconds).
  pub duration_ns: u64,
  pub sample_rate_hz: u32,
  pub channels: u16,
  /// Interleaved f32 samples in the range `[-1.0, 1.0]`.
  pub samples: Vec<f32>,
}

/// A decoded video frame in RGBA8 format.
#[derive(Debug, Clone)]
pub struct DecodedVideoFrame {
  pub pts_ns: u64,
  pub width: u32,
  pub height: u32,
  /// RGBA8 pixels, row-major, tightly packed.
  pub rgba: Vec<u8>,
}

/// A decoded media output item (audio or video).
#[derive(Debug, Clone)]
pub enum DecodedItem {
  Video(DecodedVideoFrame),
  Audio(DecodedAudioChunk),
}

/// Blocking decode session that yields decoded items in (best-effort) timestamp order.
///
/// This is intended for background decode work (not paint-thread access). Paint should instead use
/// [`MediaFrameProvider`] implementations that cache decoded frames.
pub trait MediaSession: Send {
  fn next_decoded(&mut self) -> MediaResult<Option<DecodedItem>>;
  fn seek(&mut self, time_ns: u64) -> MediaResult<()>;
}

impl MediaSession for MediaDecodePipeline {
  fn next_decoded(&mut self) -> MediaResult<Option<DecodedItem>> {
    MediaDecodePipeline::next_decoded(self)
  }

  fn seek(&mut self, time_ns: u64) -> MediaResult<()> {
    MediaDecodePipeline::seek(self, time_ns)
  }
}

/// Selectable media backend (native pipeline vs CLI fallback).
pub trait MediaBackend: Send + Sync {
  fn name(&self) -> &'static str;
  fn available(&self) -> bool;
  fn open(&self, bytes: Arc<[u8]>) -> MediaResult<Box<dyn MediaSession>>;
}
