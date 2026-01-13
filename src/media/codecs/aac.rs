use crate::media::{DecodedAudioChunk, MediaError, MediaLimits, MediaPacket, MediaResult};

use symphonia_core::audio::{Channels, SampleBuffer};
use symphonia_core::codecs::{CodecParameters, Decoder, DecoderOptions, CODEC_TYPE_AAC};
use symphonia_core::formats::Packet as SymphoniaPacket;

pub struct AacDecoder {
  decoder: symphonia_codec_aac::AacDecoder,
  limits: MediaLimits,
}

impl AacDecoder {
  pub fn new(audio_specific_config: &[u8], sample_rate: u32, channels: u16) -> MediaResult<Self> {
    Self::new_with_limits(
      audio_specific_config,
      sample_rate,
      channels,
      MediaLimits::default(),
    )
  }

  pub fn new_with_limits(
    audio_specific_config: &[u8],
    sample_rate: u32,
    channels: u16,
    limits: MediaLimits,
  ) -> MediaResult<Self> {
    let mut params = CodecParameters::new();
    params.codec = CODEC_TYPE_AAC;
    params.sample_rate = Some(sample_rate);
    params.channels = channels_for_count(channels);
    params.extra_data = Some(audio_specific_config.to_vec().into_boxed_slice());

    let decoder = symphonia_codec_aac::AacDecoder::try_new(&params, &DecoderOptions::default())
      .map_err(|e| MediaError::decode(format!("failed to create AAC decoder: {e}")))?;

    Ok(Self { decoder, limits })
  }

  pub fn decode(&mut self, packet: &MediaPacket) -> MediaResult<Option<DecodedAudioChunk>> {
    if packet.data.len() > self.limits.max_packet_bytes {
      return Err(MediaError::resource_too_large(format!(
        "aac packet size {} exceeds max_packet_bytes {}",
        packet.data.len(),
        self.limits.max_packet_bytes
      )));
    }

    let sym_packet =
      SymphoniaPacket::new_from_slice(0, packet.pts_ns, packet.duration_ns, packet.as_slice());

    let decoded = self
      .decoder
      .decode(&sym_packet)
      .map_err(|e| MediaError::decode(format!("AAC decode failed: {e}")))?;

    let spec = *decoded.spec();
    let sample_rate_hz = spec.rate;
    let channels = spec.channels.count() as u16;
    let samples_per_channel = decoded.frames() as u64;

    let frames = decoded.frames() as usize;
    let total_samples = frames
      .checked_mul(spec.channels.count())
      .ok_or_else(|| MediaError::resource_too_large("aac sample count overflow"))?;
    crate::media::decode::validate_audio_samples_per_packet(total_samples, &self.limits)?;

    // Symphonia's `SampleBuffer` expects a frame count (per channel), not a raw sample count.
    let mut sample_buf = SampleBuffer::<f32>::new(samples_per_channel, spec);
    sample_buf.copy_interleaved_ref(decoded);
    let samples = sample_buf.samples().to_vec();

    if samples.is_empty() {
      return Ok(None);
    }

    // Compute duration from the decoded output. For AAC-LC this is typically 1024 samples/channel,
    // but we always trust the decoder output rather than container metadata for A/V sync.
    let computed_duration_ns = if sample_rate_hz == 0 || samples_per_channel == 0 {
      0
    } else {
      ((u128::from(samples_per_channel))
        .saturating_mul(1_000_000_000u128)
        .checked_div(u128::from(sample_rate_hz))
        .unwrap_or(0)
        .min(u128::from(u64::MAX))) as u64
    };

    // Prefer the decoded duration over any demux-provided duration when they disagree.
    // Fall back to the demux duration only if we cannot compute from decoded output.
    let duration_ns = match (computed_duration_ns, packet.duration_ns) {
      (0, demux_duration_ns) => demux_duration_ns,
      (computed_duration_ns, demux_duration_ns)
        if demux_duration_ns != 0 && demux_duration_ns != computed_duration_ns =>
      {
        computed_duration_ns
      }
      (computed_duration_ns, _) => computed_duration_ns,
    };

    Ok(Some(DecodedAudioChunk {
      samples,
      sample_rate_hz,
      channels,
      pts_ns: packet.pts_ns,
      duration_ns,
    }))
  }
}

fn channels_for_count(channels: u16) -> Option<Channels> {
  match channels {
    1 => Some(Channels::FRONT_CENTRE),
    2 => Some(Channels::FRONT_LEFT | Channels::FRONT_RIGHT),
    _ => None,
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  use symphonia_core::formats::{FormatOptions, FormatReader};
  use symphonia_core::io::{MediaSourceStream, MediaSourceStreamOptions};
  use symphonia_format_isomp4::IsoMp4Reader;

  fn fixture_first_aac_packet() -> (Vec<u8>, u32, u16, MediaPacket) {
    let fixture_path = crate::testing::fixture_path("fixtures/media/aac_stereo_48000.mp4");
    let mp4_bytes = std::fs::read(fixture_path).expect("read mp4 fixture");

    let cursor = std::io::Cursor::new(mp4_bytes);
    let mss = MediaSourceStream::new(Box::new(cursor), MediaSourceStreamOptions::default());

    let mut format =
      IsoMp4Reader::try_new(mss, &FormatOptions::default()).expect("open mp4 demuxer");

    // Extract track metadata up-front so we don't hold an immutable borrow of `format` while
    // iterating packets (the demuxer API requires `&mut self` for `next_packet`).
    let (track_id, asc, sample_rate, channels) = {
      let track = format
        .tracks()
        .iter()
        .find(|t| t.codec_params.codec == CODEC_TYPE_AAC)
        .expect("aac track");
      let asc = track
        .codec_params
        .extra_data
        .as_ref()
        .expect("aac extradata")
        .clone();
      let sample_rate = track.codec_params.sample_rate.expect("sample rate");
      let channels = track
        .codec_params
        .channels
        .expect("channels")
        .count() as u16;
      (track.id, asc, sample_rate, channels)
    };

    let mut first_packet = None;
    for _ in 0..32 {
      let pkt = format.next_packet().expect("next packet");
      if pkt.track_id() == track_id {
        first_packet = Some(pkt);
        break;
      }
    }
    let pkt = first_packet.expect("aac packet");

    let pts_ns = if sample_rate > 0 {
      ((pkt.ts() as u128)
        .saturating_mul(1_000_000_000u128)
        .checked_div(sample_rate as u128)
        .unwrap_or(0)
        .min(u128::from(u64::MAX))) as u64
    } else {
      0
    };
    let duration_ns = if sample_rate > 0 {
      ((pkt.dur() as u128)
        .saturating_mul(1_000_000_000u128)
        .checked_div(sample_rate as u128)
        .unwrap_or(0)
        .min(u128::from(u64::MAX))) as u64
    } else {
      0
    };

    // `symphonia_core::formats::Packet` stores its payload in a field (not an accessor method) in
    // this repo's pinned Symphonia version; take the pieces we need before moving it.
    let track_id = u64::from(pkt.track_id());
    let data = pkt.data.to_vec();

    let media_packet = MediaPacket {
      track_id,
      dts_ns: pts_ns,
      pts_ns,
      duration_ns,
      data: data.into(),
      is_keyframe: false,
    };

    (asc.to_vec(), sample_rate, channels, media_packet)
  }

  #[test]
  fn decodes_first_packet_from_mp4_fixture() {
    let (asc, sample_rate, channels, media_packet) = fixture_first_aac_packet();
    let mut decoder = AacDecoder::new(&asc, sample_rate, channels).expect("init decoder");

    let decoded = decoder
      .decode(&media_packet)
      .expect("decode")
      .expect("decoded output");

    assert!(decoded.channels > 0);
    assert!(!decoded.samples.is_empty());
  }

  #[test]
  fn rejects_large_output_buffers() {
    let (asc, sample_rate, channels, media_packet) = fixture_first_aac_packet();
    let mut limits = MediaLimits::default();
    limits.max_audio_samples_per_packet = 10;
    let mut decoder =
      AacDecoder::new_with_limits(&asc, sample_rate, channels, limits).expect("init decoder");

    let err = decoder.decode(&media_packet).unwrap_err();
    assert!(
      matches!(err, MediaError::ResourceTooLarge(_)),
      "unexpected error: {err}"
    );
  }
}
