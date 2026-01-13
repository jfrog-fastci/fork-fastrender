#[cfg(feature = "codec_aac")]
pub mod aac;

#[cfg(not(feature = "codec_aac"))]
pub mod aac {
  use crate::media::{DecodedAudioChunk, MediaError, MediaPacket, MediaResult};

  #[derive(Debug, Default)]
  pub struct AacDecoder;

  impl AacDecoder {
    pub fn new(
      _audio_specific_config: &[u8],
      _sample_rate: u32,
      _channels: u16,
    ) -> MediaResult<Self> {
      Err(MediaError::Unsupported(
        "`codec_aac` feature disabled (enable Cargo feature `codec_aac` or `media`)",
      ))
    }

    pub fn decode(&mut self, _packet: &MediaPacket) -> MediaResult<Option<DecodedAudioChunk>> {
      Err(MediaError::Unsupported(
        "`codec_aac` feature disabled (enable Cargo feature `codec_aac` or `media`)",
      ))
    }
  }
}

#[cfg(feature = "codec_opus")]
pub mod opus;

#[cfg(not(feature = "codec_opus"))]
pub mod opus {
  use crate::media::{DecodedAudioChunk, MediaError, MediaPacket, MediaResult};

  /// Opus always decodes at 48kHz internally.
  pub const OPUS_SAMPLE_RATE_HZ: u32 = 48_000;

  #[derive(Debug, Default)]
  pub struct OpusDecoder;

  impl OpusDecoder {
    pub fn new(_codec_private: &[u8]) -> MediaResult<Self> {
      Err(MediaError::Unsupported(
        "`codec_opus` feature disabled (enable Cargo feature `codec_opus` or `media`)",
      ))
    }

    pub fn decode(&mut self, _packet: &MediaPacket) -> MediaResult<Option<DecodedAudioChunk>> {
      Err(MediaError::Unsupported(
        "`codec_opus` feature disabled (enable Cargo feature `codec_opus` or `media`)",
      ))
    }
  }
}

#[cfg(feature = "codec_vp9_libvpx")]
pub mod vp9;

#[cfg(not(feature = "codec_vp9_libvpx"))]
pub mod vp9 {
  use crate::media::{MediaError, MediaPacket, MediaResult};

  /// Decoded VP9 frame in RGBA8 format.
  #[derive(Debug, Clone)]
  pub struct DecodedVp9Frame {
    pub pts_ns: u64,
    pub width: u32,
    pub height: u32,
    pub rgba8: Vec<u8>,
  }

  #[derive(Debug, Default)]
  pub struct Vp9Decoder;

  impl Vp9Decoder {
    pub fn new(_threads: u32) -> MediaResult<Self> {
      Err(MediaError::Unsupported(
        "`codec_vp9_libvpx` feature disabled (enable Cargo feature `codec_vp9_libvpx` or `media`)",
      ))
    }

    pub fn decode(&mut self, _packet: &MediaPacket) -> MediaResult<Vec<DecodedVp9Frame>> {
      Err(MediaError::Unsupported(
        "`codec_vp9_libvpx` feature disabled (enable Cargo feature `codec_vp9_libvpx` or `media`)",
      ))
    }
  }
}

