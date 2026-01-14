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
        "`codec_aac` feature disabled (enable Cargo feature `codec_aac` or `media`)".into(),
      ))
    }

    pub fn decode(&mut self, _packet: &MediaPacket) -> MediaResult<Option<DecodedAudioChunk>> {
      Err(MediaError::Unsupported(
        "`codec_aac` feature disabled (enable Cargo feature `codec_aac` or `media`)".into(),
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

  #[derive(Debug, Clone, Copy, PartialEq, Eq)]
  pub struct OpusHead;

  impl OpusHead {
    pub fn parse(_codec_private: &[u8]) -> MediaResult<Self> {
      Err(MediaError::Unsupported(
        "`codec_opus` feature disabled (enable Cargo feature `codec_opus` or `media`)".into(),
      ))
    }
  }

  #[derive(Debug, Default)]
  pub struct OpusDecoder;

  impl OpusDecoder {
    pub fn new(_opus_head: &OpusHead) -> MediaResult<Self> {
      Err(MediaError::Unsupported(
        "`codec_opus` feature disabled (enable Cargo feature `codec_opus` or `media`)".into(),
      ))
    }

    pub fn from_codec_private(_codec_private: &[u8]) -> MediaResult<Self> {
      Err(MediaError::Unsupported(
        "`codec_opus` feature disabled (enable Cargo feature `codec_opus` or `media`)".into(),
      ))
    }

    pub fn decode(&mut self, _packet: &MediaPacket) -> MediaResult<Option<DecodedAudioChunk>> {
      Err(MediaError::Unsupported(
        "`codec_opus` feature disabled (enable Cargo feature `codec_opus` or `media`)".into(),
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
        "`codec_vp9_libvpx` feature disabled (enable Cargo feature `codec_vp9_libvpx` or `media`)".into(),
      ))
    }

    pub fn decode(&mut self, _packet: &MediaPacket) -> MediaResult<Vec<DecodedVp9Frame>> {
      Err(MediaError::Unsupported(
        "`codec_vp9_libvpx` feature disabled (enable Cargo feature `codec_vp9_libvpx` or `media`)".into(),
      ))
    }
  }
}

#[cfg(test)]
mod tests {
  use crate::media::MediaError;

  #[cfg(not(feature = "codec_opus"))]
  #[test]
  fn opus_feature_disabled_errors_are_stable() {
    const MSG: &str =
      "`codec_opus` feature disabled (enable Cargo feature `codec_opus` or `media`)";

    let err = super::opus::OpusHead::parse(&[]).unwrap_err();
    match err {
      MediaError::Unsupported(msg) => assert_eq!(msg.as_ref(), MSG),
      other => panic!("expected MediaError::Unsupported, got {other:?}"),
    }

    let head = super::opus::OpusHead;
    let err = super::opus::OpusDecoder::new(&head).unwrap_err();
    match err {
      MediaError::Unsupported(msg) => assert_eq!(msg.as_ref(), MSG),
      other => panic!("expected MediaError::Unsupported, got {other:?}"),
    }
  }
}
