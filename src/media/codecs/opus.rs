use crate::media::{DecodedAudioChunk, MediaError, MediaPacket, MediaResult};

/// Opus always decodes at 48kHz internally.
///
/// For WebM/Matroska (`A_OPUS`), the codec private `OpusHead` contains an `input_sample_rate` field,
/// but that is informational (original capture rate) and does not change the decoder sample clock.
pub const OPUS_SAMPLE_RATE_HZ: u32 = 48_000;

#[derive(Debug, Clone, Copy)]
struct OpusHead {
  channels: u16,
  pre_skip: u16,
}

fn parse_opus_head(codec_private: &[u8]) -> MediaResult<OpusHead> {
  // https://datatracker.ietf.org/doc/html/rfc7845#section-5.1
  //
  //   0..8  "OpusHead"
  //   8     version
  //   9     channel count
  //   10..12 pre_skip (LE u16)
  //   12..16 input sample rate (LE u32)
  //   16..18 output gain (LE i16)
  //   18    channel mapping family
  const OPUS_HEAD_LEN: usize = 19;
  if codec_private.len() < OPUS_HEAD_LEN {
    return Err(MediaError::Decode(format!(
      "OpusHead too short: need at least {OPUS_HEAD_LEN} bytes, got {}",
      codec_private.len()
    )));
  }

  if &codec_private[0..8] != b"OpusHead" {
    return Err(MediaError::Decode(
      "OpusHead signature missing in codec_private".to_string(),
    ));
  }

  let version = codec_private[8];
  if version == 0 {
    return Err(MediaError::Decode(
      "OpusHead version 0 is invalid".to_string(),
    ));
  }

  let channels = codec_private[9] as u16;
  let pre_skip = u16::from_le_bytes([codec_private[10], codec_private[11]]);
  let mapping_family = codec_private[18];

  // WebM only supports mapping family 0 (mono/stereo) today.
  // We implement only that subset for now.
  if mapping_family != 0 {
    return Err(MediaError::Unsupported(
      "Opus channel mapping family is not supported",
    ));
  }

  if !(channels == 1 || channels == 2) {
    return Err(MediaError::Unsupported(
      "only mono/stereo Opus is currently supported",
    ));
  }

  Ok(OpusHead { channels, pre_skip })
}

pub struct OpusDecoder {
  decoder: *mut audiopus_sys::OpusDecoder,
  channels: u16,
  remaining_pre_skip: usize,
}

unsafe impl Send for OpusDecoder {}

impl Drop for OpusDecoder {
  fn drop(&mut self) {
    if !self.decoder.is_null() {
      unsafe {
        audiopus_sys::opus_decoder_destroy(self.decoder);
      }
      self.decoder = std::ptr::null_mut();
    }
  }
}

impl OpusDecoder {
  pub fn new(codec_private: &[u8]) -> MediaResult<Self> {
    let head = parse_opus_head(codec_private)?;

    let mut err = 0i32;
    // SAFETY: FFI call; pointer lifetime handled by Drop.
    let decoder = unsafe {
      audiopus_sys::opus_decoder_create(OPUS_SAMPLE_RATE_HZ as i32, head.channels as i32, &mut err)
    };
    if decoder.is_null() || err != audiopus_sys::OPUS_OK {
      if !decoder.is_null() {
        unsafe {
          audiopus_sys::opus_decoder_destroy(decoder);
        }
      }
      return Err(MediaError::Decode(format!(
        "failed to create Opus decoder (channels {}, err {err})",
        head.channels
      )));
    }

    Ok(Self {
      decoder,
      channels: head.channels,
      remaining_pre_skip: head.pre_skip as usize,
    })
  }

  /// Decode a single Opus packet into interleaved `f32` PCM.
  ///
  /// `DecodedAudioChunk.duration_ns` is computed from the number of decoded samples per channel
  /// (after any `pre_skip` trimming), using the Opus sample clock (48kHz):
  ///
  /// `duration_ns = (samples_per_channel / 48_000) * 1e9`.
  pub fn decode(&mut self, packet: &MediaPacket) -> MediaResult<Option<DecodedAudioChunk>> {
    const MAX_FRAME_SIZE_SAMPLES_PER_CHANNEL: usize = 5760; // 120ms @ 48kHz (Opus max)

    let mut pcm = vec![0.0f32; MAX_FRAME_SIZE_SAMPLES_PER_CHANNEL * self.channels as usize];

    let data = packet.as_slice();
    let (data_ptr, data_len) = if data.is_empty() {
      (std::ptr::null(), 0i32)
    } else {
      (data.as_ptr(), data.len() as i32)
    };

    // SAFETY:
    // - `self.decoder` is valid for the lifetime of `self` (created in `new`, destroyed in Drop).
    // - `data_ptr` is valid for `data_len` (or NULL for 0-length packets).
    // - `pcm` has space for `frame_size * channels` samples.
    let decoded = unsafe {
      audiopus_sys::opus_decode_float(
        self.decoder,
        data_ptr,
        data_len,
        pcm.as_mut_ptr(),
        MAX_FRAME_SIZE_SAMPLES_PER_CHANNEL as i32,
        0,
      )
    };
    if decoded < 0 {
      return Err(MediaError::Decode(format!("Opus decode failed: error {decoded}")));
    }

    let decoded_samples_per_channel = decoded as usize;
    let total_samples = decoded_samples_per_channel.saturating_mul(self.channels as usize);
    pcm.truncate(total_samples);

    if self.remaining_pre_skip > 0 && decoded_samples_per_channel > 0 {
      let skip_frames = self.remaining_pre_skip.min(decoded_samples_per_channel);
      let skip_samples = skip_frames.saturating_mul(self.channels as usize);
      if skip_samples > 0 {
        pcm.copy_within(skip_samples.., 0);
        pcm.truncate(total_samples.saturating_sub(skip_samples));
      }
      self.remaining_pre_skip = self.remaining_pre_skip.saturating_sub(skip_frames);
    }

    if pcm.is_empty() {
      return Ok(None);
    }

    let samples_per_channel = pcm.len() / self.channels as usize;
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

  #[test]
  fn opus_head_rejects_invalid_magic() {
    let mut bad = [0u8; 19];
    bad[..8].copy_from_slice(b"NotOpus!");
    assert!(parse_opus_head(&bad).is_err());
  }

  fn build_opus_head(channels: u8, pre_skip: u16) -> Vec<u8> {
    let mut out = Vec::with_capacity(19);
    out.extend_from_slice(b"OpusHead");
    out.push(1); // version
    out.push(channels);
    out.extend_from_slice(&pre_skip.to_le_bytes());
    out.extend_from_slice(&OPUS_SAMPLE_RATE_HZ.to_le_bytes()); // input sample rate (informational)
    out.extend_from_slice(&0i16.to_le_bytes()); // output gain
    out.push(0); // mapping family 0
    out
  }

  fn encode_silence_packet(samples_per_channel: usize, channels: u16) -> Vec<u8> {
    let mut err = 0i32;
    // SAFETY: FFI call; encoder destroyed before returning.
    let encoder = unsafe {
      audiopus_sys::opus_encoder_create(
        OPUS_SAMPLE_RATE_HZ as i32,
        channels as i32,
        audiopus_sys::OPUS_APPLICATION_AUDIO,
        &mut err,
      )
    };
    assert!(!encoder.is_null());
    assert_eq!(err, audiopus_sys::OPUS_OK);

    let pcm = vec![0.0f32; samples_per_channel * channels as usize];
    let mut out = vec![0u8; 4000];
    // SAFETY: pointers are valid and output buffer has sufficient space.
    let len = unsafe {
      audiopus_sys::opus_encode_float(
        encoder,
        pcm.as_ptr(),
        samples_per_channel as i32,
        out.as_mut_ptr(),
        out.len() as i32,
      )
    };
    unsafe {
      audiopus_sys::opus_encoder_destroy(encoder);
    }
    assert!(len > 0, "opus_encode_float returned {len}");
    out.truncate(len as usize);
    out
  }

  #[test]
  fn opus_packet_duration_matches_decoded_sample_count() {
    // 20ms @ 48kHz.
    let samples_per_channel = 960usize;
    let packet_data = encode_silence_packet(samples_per_channel, 1);

    let opus_head = build_opus_head(1, 0);
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

    let opus_head = build_opus_head(1, pre_skip);
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

    let mut decoder = OpusDecoder::new(&audio_track.codec_private).expect("create Opus decoder");

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
