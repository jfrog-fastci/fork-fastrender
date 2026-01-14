use std::error::Error as StdError;

use thiserror::Error;

use super::DeviceSelector;
use super::AudioStreamConfig;
use super::types::{ChannelLayout, SampleFormat};

/// Result type for the media audio module.
pub type Result<T> = std::result::Result<T, AudioError>;

/// Stable classification for audio errors.
///
/// This exists so backend selection code can branch on *why* initialization failed without relying
/// on string matching.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AudioErrorKind {
  /// No usable audio device was available (common in CI/headless environments).
  DeviceUnavailable,
  /// The audio device exists, but the requested/selected config is not supported.
  ConfigUnsupported,
  /// The backend failed while creating or starting the output stream.
  StreamFailure,
  /// Audio data could not be queued because the sink buffer was full.
  QueueOverflow,
  /// Catch-all bucket for errors that do not fit other categories.
  Other,
}

impl AudioErrorKind {
  #[must_use]
  pub const fn as_str(self) -> &'static str {
    match self {
      Self::DeviceUnavailable => "device_unavailable",
      Self::ConfigUnsupported => "config_unsupported",
      Self::StreamFailure => "stream_failure",
      Self::QueueOverflow => "queue_overflow",
      Self::Other => "other",
    }
  }
}

impl std::fmt::Display for AudioErrorKind {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.write_str(self.as_str())
  }
}

/// Sample formats used by audio backends.
///
/// This is intentionally backend-agnostic (doesn't depend on CPAL types) so `AudioError` stays
/// available even when the `audio_cpal` feature is disabled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AudioSampleFormat {
  I8,
  U8,
  I16,
  U16,
  I32,
  U32,
  I64,
  U64,
  F32,
  F64,
  Unknown,
}

impl AudioSampleFormat {
  #[must_use]
  pub const fn as_str(self) -> &'static str {
    match self {
      Self::I8 => "i8",
      Self::U8 => "u8",
      Self::I16 => "i16",
      Self::U16 => "u16",
      Self::I32 => "i32",
      Self::U32 => "u32",
      Self::I64 => "i64",
      Self::U64 => "u64",
      Self::F32 => "f32",
      Self::F64 => "f64",
      Self::Unknown => "unknown",
    }
  }
}

impl std::fmt::Display for AudioSampleFormat {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.write_str(self.as_str())
  }
}

#[cfg(feature = "audio_cpal")]
impl From<cpal::SampleFormat> for AudioSampleFormat {
  fn from(value: cpal::SampleFormat) -> Self {
    match value {
      cpal::SampleFormat::I8 => Self::I8,
      cpal::SampleFormat::U8 => Self::U8,
      cpal::SampleFormat::I16 => Self::I16,
      cpal::SampleFormat::U16 => Self::U16,
      cpal::SampleFormat::I32 => Self::I32,
      cpal::SampleFormat::U32 => Self::U32,
      cpal::SampleFormat::I64 => Self::I64,
      cpal::SampleFormat::U64 => Self::U64,
      cpal::SampleFormat::F32 => Self::F32,
      cpal::SampleFormat::F64 => Self::F64,
      _ => Self::Unknown,
    }
  }
}

type SourceError = Box<dyn StdError + Send + Sync>;

#[derive(Error, Debug)]
pub enum AudioError {
  #[error("failed to enumerate output audio devices: {source}")]
  OutputDeviceEnumerationFailed {
    #[source]
    source: SourceError,
  },

  #[error("no default output audio device is available")]
  NoOutputDevice,

  #[error("output audio device not found for selector {selector:?}")]
  OutputDeviceNotFound { selector: DeviceSelector },

  #[error("failed to enumerate output audio configs for device '{device_name}': {source}")]
  OutputConfigEnumerationFailed {
    device_name: String,
    #[source]
    source: SourceError,
  },

  #[error("failed to load default output audio config for device '{device_name}': {source}")]
  DefaultOutputConfigFailed {
    device_name: String,
    #[source]
    source: SourceError,
  },

  #[error(
    "unsupported output audio sample format '{sample_format}' for device '{device_name}'"
  )]
  UnsupportedSampleFormat {
    device_name: String,
    sample_format: AudioSampleFormat,
  },

  #[error(
    "failed to build audio output stream for device '{device_name}' ({config}, format {sample_format}): {source}"
  )]
  StreamBuildFailed {
    device_name: String,
    config: AudioStreamConfig,
    sample_format: AudioSampleFormat,
    #[source]
    source: SourceError,
  },

  #[error("failed to start output audio stream for device '{device_name}': {source}")]
  StreamPlayFailed {
    device_name: String,
    #[source]
    source: SourceError,
  },

  #[error("audio backend '{backend}' terminated unexpectedly")]
  BackendThreadTerminated { backend: &'static str },

  #[error(
    "audio queue overflow: accepted {accepted_samples} of {attempted_samples} samples (capacity {capacity_samples})"
  )]
  QueueOverflow {
    attempted_samples: usize,
    accepted_samples: usize,
    capacity_samples: usize,
  },

  // --------------------------------------------------------------------------
  // Decoder-facing buffer validation/conversion errors
  // --------------------------------------------------------------------------

  #[error("invalid channel count {channels}")]
  InvalidChannels { channels: usize },

  #[error("invalid sample rate {sample_rate}")]
  InvalidSampleRate { sample_rate: u32 },

  #[error("invalid audio spec: {reason}")]
  InvalidSpec { reason: String },

  #[error("invalid audio buffer: {reason}")]
  InvalidBuffer { reason: String },

  #[error(
    "audio buffer format/layout mismatch with data: format={format:?} data_format={data_format:?} layout={layout:?} data_layout={data_layout:?}"
  )]
  BufferMetadataMismatch {
    format: SampleFormat,
    data_format: SampleFormat,
    layout: ChannelLayout,
    data_layout: ChannelLayout,
  },

  #[error(
    "interleaved buffer has {len_samples} samples which is not divisible by channel count {channels}"
  )]
  InvalidInterleavedLength { len_samples: usize, channels: usize },

  #[error("planar buffer expected {channels} planes but got {planes}")]
  InvalidPlaneCount { channels: usize, planes: usize },

  #[error(
    "planar buffer plane {plane} has {len_samples} samples but expected {expected_samples}"
  )]
  InvalidPlaneLength {
    plane: usize,
    len_samples: usize,
    expected_samples: usize,
  },

  #[error(
    "audio buffer config mismatch: expected {expected_channels}ch@{expected_sample_rate_hz}Hz but got {channels}ch@{sample_rate_hz}Hz"
  )]
  StreamConfigMismatch {
    expected_channels: usize,
    expected_sample_rate_hz: u32,
    channels: usize,
    sample_rate_hz: u32,
  },
}

impl AudioError {
  #[must_use]
  pub fn invalid_spec(reason: impl Into<String>) -> Self {
    Self::InvalidSpec {
      reason: reason.into(),
    }
  }

  #[must_use]
  pub fn invalid_buffer(reason: impl Into<String>) -> Self {
    Self::InvalidBuffer {
      reason: reason.into(),
    }
  }

  #[must_use]
  pub fn kind(&self) -> AudioErrorKind {
    match self {
      Self::OutputDeviceEnumerationFailed { .. }
      | Self::NoOutputDevice
      | Self::OutputDeviceNotFound { .. } => AudioErrorKind::DeviceUnavailable,
      Self::OutputConfigEnumerationFailed { .. }
      | Self::DefaultOutputConfigFailed { .. }
      | Self::UnsupportedSampleFormat { .. } => AudioErrorKind::ConfigUnsupported,
      Self::StreamBuildFailed { .. }
      | Self::StreamPlayFailed { .. }
      | Self::BackendThreadTerminated { .. } => AudioErrorKind::StreamFailure,
      Self::QueueOverflow { .. } => AudioErrorKind::QueueOverflow,
      _ => AudioErrorKind::Other,
    }
  }
}

impl From<AudioError> for crate::error::Error {
  fn from(err: AudioError) -> Self {
    let kind = err.kind();
    crate::error::Error::Other(format!("audio ({kind}): {err}"))
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn audio_error_display_includes_device_and_config() {
    let err = AudioError::StreamBuildFailed {
      device_name: "Test Device".to_string(),
      config: AudioStreamConfig::new(48_000, 2),
      sample_format: AudioSampleFormat::F32,
      source: Box::new(std::io::Error::new(std::io::ErrorKind::Other, "boom")),
    };
    let msg = err.to_string();
    assert!(msg.contains("Test Device"));
    assert!(msg.contains("48000Hz"));
    assert!(msg.contains("2ch"));
    assert!(msg.contains("f32"));
  }

  #[test]
  fn audio_error_display_includes_format() {
    let err = AudioError::UnsupportedSampleFormat {
      device_name: "Speakers".to_string(),
      sample_format: AudioSampleFormat::F64,
    };
    let msg = err.to_string();
    assert!(msg.contains("Speakers"));
    assert!(msg.contains("f64"));
  }

  #[test]
  fn audio_error_kind_is_stable() {
    let err = AudioError::NoOutputDevice;
    assert_eq!(err.kind(), AudioErrorKind::DeviceUnavailable);
  }

  #[test]
  fn audio_error_display_includes_device_name_for_output_device_not_found() {
    let err = AudioError::OutputDeviceNotFound {
      selector: DeviceSelector::Device(super::super::AudioDeviceId {
        name: "Headphones".to_string(),
        ordinal: 0,
      }),
    };
    let msg = err.to_string();
    assert!(msg.contains("Headphones"));
  }

  #[test]
  fn audio_error_converts_to_crate_error_with_kind() {
    let err: crate::error::Error = AudioError::NoOutputDevice.into();
    let msg = err.to_string();
    assert!(msg.contains("device_unavailable"));
    assert!(msg.contains("no default output audio device"));
  }
}
