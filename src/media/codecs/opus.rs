use crate::media::{DecodedAudioChunk, MediaError, MediaPacket, MediaResult};

/// Opus always decodes at 48kHz internally.
///
/// For WebM/Matroska (`A_OPUS`), the codec private `OpusHead` contains an `input_sample_rate` field,
/// but that is informational (original capture rate) and does not change the decoder sample clock.
pub const OPUS_SAMPLE_RATE_HZ: u32 = 48_000;

/// Parsed `OpusHead` packet as stored in Matroska/WebM `CodecPrivate`.
///
/// Structure: <https://datatracker.ietf.org/doc/html/rfc7845#section-5.1>
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OpusHead {
  pub version: u8,
  pub channel_count: u8,
  /// Number of samples (per channel, at 48kHz) to discard from the decoder output at stream start.
  pub pre_skip: u16,
  pub input_sample_rate: u32,
  pub output_gain: i16,
  pub channel_mapping_family: u8,
}

impl OpusHead {
  pub const MAGIC: &'static [u8; 8] = b"OpusHead";
  pub const BASE_LEN: usize = 19;

  pub fn parse(codec_private: &[u8]) -> MediaResult<Self> {
    if codec_private.len() < Self::BASE_LEN {
      return Err(MediaError::Demux(format!(
        "OpusHead too short: need at least {} bytes, got {}",
        Self::BASE_LEN,
        codec_private.len()
      )));
    }
    if &codec_private[..Self::MAGIC.len()] != Self::MAGIC {
      return Err(MediaError::Demux("invalid OpusHead magic".to_string()));
    }

    let version = codec_private[8];
    let channel_count = codec_private[9];
    let pre_skip = u16::from_le_bytes([codec_private[10], codec_private[11]]);
    let input_sample_rate = u32::from_le_bytes([
      codec_private[12],
      codec_private[13],
      codec_private[14],
      codec_private[15],
    ]);
    let output_gain = i16::from_le_bytes([codec_private[16], codec_private[17]]);
    let channel_mapping_family = codec_private[18];

    if version != 1 {
      return Err(MediaError::Unsupported("unsupported OpusHead version"));
    }

    // WebM only supports mapping family 0 (mono/stereo) today.
    // We implement only that subset for now.
    if channel_mapping_family != 0 {
      return Err(MediaError::Unsupported(
        "unsupported Opus channel mapping family",
      ));
    }

    // Mapping family 0 is defined for mono/stereo only.
    if !(channel_count == 1 || channel_count == 2) {
      return Err(MediaError::Unsupported(
        "unsupported Opus channel count for mapping family 0",
      ));
    }

    Ok(Self {
      version,
      channel_count,
      pre_skip,
      input_sample_rate,
      output_gain,
      channel_mapping_family,
    })
  }
}

pub struct OpusDecoder {
  decoder: opus::Decoder,
  channels: u16,
  /// Remaining samples (per channel) to discard for Opus pre-skip.
  pre_skip_remaining: usize,
}

impl OpusDecoder {
  pub fn new(head: &OpusHead) -> MediaResult<Self> {
    let channels = match head.channel_count {
      1 => opus::Channels::Mono,
      2 => opus::Channels::Stereo,
      _ => return Err(MediaError::Unsupported("unsupported Opus channel count")),
    };

    let decoder = opus::Decoder::new(OPUS_SAMPLE_RATE_HZ, channels)
      .map_err(|e| MediaError::Decode(format!("failed to create Opus decoder: {e}")))?;

    Ok(Self {
      decoder,
      channels: u16::from(head.channel_count),
      pre_skip_remaining: head.pre_skip as usize,
    })
  }

  pub fn from_codec_private(codec_private: &[u8]) -> MediaResult<Self> {
    let head = OpusHead::parse(codec_private)?;
    Self::new(&head)
  }

  /// Decode a single Opus packet into interleaved `f32` PCM.
  ///
  /// `DecodedAudioChunk.duration_ns` is computed from the number of decoded samples per channel
  /// (after any `pre_skip` trimming), using the Opus sample clock (48kHz):
  ///
  /// `duration_ns = (samples_per_channel / 48_000) * 1e9`.
  pub fn decode(&mut self, packet: &MediaPacket) -> MediaResult<Option<DecodedAudioChunk>> {
    const MAX_FRAME_SIZE_SAMPLES_PER_CHANNEL: usize = 5760; // 120ms @ 48kHz (Opus max)

    let channels = self.channels as usize;
    let mut pcm = vec![0.0f32; MAX_FRAME_SIZE_SAMPLES_PER_CHANNEL * channels];

    let decoded = self
      .decoder
      .decode_float(packet.as_slice(), &mut pcm, false)
      .map_err(|e| MediaError::Decode(format!("Opus decode failed: {e}")))?;
    let decoded_samples_per_channel: usize = decoded
      .try_into()
      .map_err(|_| MediaError::Decode("Opus decode returned invalid sample count".to_string()))?;

    let total_samples = decoded_samples_per_channel.saturating_mul(channels);
    pcm.truncate(total_samples);

    if self.pre_skip_remaining > 0 && decoded_samples_per_channel > 0 {
      let skip_frames = self.pre_skip_remaining.min(decoded_samples_per_channel);
      let skip_samples = skip_frames.saturating_mul(channels);
      if skip_samples > 0 {
        pcm.copy_within(skip_samples.., 0);
        pcm.truncate(total_samples.saturating_sub(skip_samples));
      }
      self.pre_skip_remaining = self.pre_skip_remaining.saturating_sub(skip_frames);
    }

    if pcm.is_empty() {
      return Ok(None);
    }

    let samples_per_channel = pcm.len() / channels;
    let duration_ns = ((samples_per_channel as u128)
      .saturating_mul(1_000_000_000u128)
      .checked_div(OPUS_SAMPLE_RATE_HZ as u128)
      .unwrap_or(0)
      .min(u128::from(u64::MAX))) as u64;

    Ok(Some(DecodedAudioChunk {
      pts_ns: packet.pts_ns,
      duration_ns,
      sample_rate_hz: OPUS_SAMPLE_RATE_HZ,
      channels: self.channels,
      samples: pcm,
    }))
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  fn build_opus_head(channels: u8, pre_skip: u16) -> Vec<u8> {
    let mut out = Vec::with_capacity(OpusHead::BASE_LEN);
    out.extend_from_slice(OpusHead::MAGIC);
    out.push(1); // version
    out.push(channels);
    out.extend_from_slice(&pre_skip.to_le_bytes());
    out.extend_from_slice(&OPUS_SAMPLE_RATE_HZ.to_le_bytes()); // input sample rate (informational)
    out.extend_from_slice(&0i16.to_le_bytes()); // output gain
    out.push(0); // mapping family 0
    out
  }

  fn encode_silence_packet(samples_per_channel: usize, channels: u16) -> Vec<u8> {
    let channel_count = channels as usize;
    let channels = match channels {
      1 => opus::Channels::Mono,
      2 => opus::Channels::Stereo,
      _ => panic!("unsupported channel count for test"),
    };
    let mut encoder = opus::Encoder::new(OPUS_SAMPLE_RATE_HZ, channels, opus::Application::Audio)
      .expect("create Opus encoder");
    let pcm = vec![0.0f32; samples_per_channel * channel_count];
    let mut out = vec![0u8; 4000];
    let len = encoder
      .encode_float(&pcm, samples_per_channel, &mut out)
      .expect("encode silence packet");
    let len: usize = len
      .try_into()
      .unwrap_or_else(|_| panic!("encode returned invalid length: {len:?}"));
    out.truncate(len);
    out
  }

  #[test]
  fn opus_head_rejects_invalid_magic() {
    let mut bad = [0u8; OpusHead::BASE_LEN];
    bad[..8].copy_from_slice(b"NotOpus!");
    assert!(OpusHead::parse(&bad).is_err());
  }

  #[test]
  fn opus_packet_duration_matches_decoded_sample_count() {
    // 20ms @ 48kHz.
    let samples_per_channel = 960usize;
    let packet_data = encode_silence_packet(samples_per_channel, 1);

    let opus_head = OpusHead::parse(&build_opus_head(1, 0)).expect("parse OpusHead");
    let mut decoder = OpusDecoder::new(&opus_head).expect("create decoder");

    let packet = MediaPacket {
      track_id: 1,
      dts_ns: 0,
      pts_ns: 0,
      // WebM Opus blocks often have no explicit duration.
      duration_ns: 0,
      data: packet_data.into(),
      is_keyframe: false,
    };

    let chunk = decoder
      .decode(&packet)
      .expect("decode")
      .expect("decoded audio");

    let frames = chunk.samples.len() / chunk.channels as usize;
    assert_eq!(frames, samples_per_channel);
    assert_eq!(chunk.sample_rate_hz, OPUS_SAMPLE_RATE_HZ);
    assert_eq!(
      chunk.duration_ns,
      (frames as u64 * 1_000_000_000u64) / OPUS_SAMPLE_RATE_HZ as u64
    );
  }

  #[test]
  fn opus_preskip_trimming_updates_duration() {
    let samples_per_channel = 960usize;
    let pre_skip = 312u16;
    let packet_data = encode_silence_packet(samples_per_channel, 1);

    let opus_head = OpusHead::parse(&build_opus_head(1, pre_skip)).expect("parse OpusHead");
    let mut decoder = OpusDecoder::new(&opus_head).expect("create decoder");

    let packet = MediaPacket {
      track_id: 1,
      dts_ns: 0,
      pts_ns: 0,
      duration_ns: 0,
      data: packet_data.into(),
      is_keyframe: false,
    };

    let chunk = decoder
      .decode(&packet)
      .expect("decode")
      .expect("decoded audio");

    let frames = chunk.samples.len() / chunk.channels as usize;
    let expected_frames = samples_per_channel - pre_skip as usize;
    assert_eq!(frames, expected_frames);
    assert_eq!(
      chunk.duration_ns,
      (expected_frames as u64 * 1_000_000_000u64) / OPUS_SAMPLE_RATE_HZ as u64
    );
  }

  #[cfg(feature = "media_webm")]
  #[test]
  fn decode_first_opus_frame_from_webm_fixture() {
    use crate::media::demux::webm::WebmDemuxer;
    use crate::media::MediaCodec;
    use std::io::Cursor;

    let bytes = include_bytes!("../../../tests/fixtures/media/test_vp9_opus.webm");
    let cursor = Cursor::new(bytes.as_slice());
    let mut demuxer = WebmDemuxer::open(cursor).expect("open webm fixture");

    let audio_track = demuxer
      .tracks()
      .iter()
      .find(|t| t.codec == MediaCodec::Opus)
      .expect("webm fixture should contain an Opus audio track");
    let track_id = audio_track.id;

    let mut decoder =
      OpusDecoder::from_codec_private(&audio_track.codec_private).expect("create Opus decoder");

    let mut decoded: Option<DecodedAudioChunk> = None;
    while let Some(pkt) = demuxer.next_packet().expect("read packet") {
      if pkt.track_id != track_id {
        continue;
      }
      if let Some(chunk) = decoder.decode(&pkt).expect("decode opus packet") {
        if !chunk.samples.is_empty() {
          decoded = Some(chunk);
          break;
        }
      }
    }

    let decoded = decoded.expect("expected at least one decoded Opus chunk");
    assert_eq!(decoded.sample_rate_hz, OPUS_SAMPLE_RATE_HZ);
    assert_eq!(decoded.samples.len() % decoded.channels as usize, 0);
  }
}
