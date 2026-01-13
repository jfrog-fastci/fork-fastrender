use crate::media::{DecodedAudioChunk, MediaError, MediaPacket, MediaResult};

use symphonia_core::audio::{Channels, SampleBuffer};
use symphonia_core::codecs::{CodecParameters, Decoder, DecoderOptions, CODEC_TYPE_AAC};
use symphonia_core::formats::Packet as SymphoniaPacket;

pub struct AacDecoder {
  decoder: symphonia_codec_aac::AacDecoder,
}

impl AacDecoder {
  pub fn new(audio_specific_config: &[u8], sample_rate: u32, channels: u16) -> MediaResult<Self> {
    let mut params = CodecParameters::new();
    params.codec = CODEC_TYPE_AAC;
    params.sample_rate = Some(sample_rate);
    params.channels = channels_for_count(channels);
    params.extra_data = Some(audio_specific_config.to_vec().into_boxed_slice());

    let decoder = symphonia_codec_aac::AacDecoder::try_new(&params, &DecoderOptions::default())
      .map_err(|e| MediaError::Decode(format!("failed to create AAC decoder: {e}")))?;

    Ok(Self { decoder })
  }

  pub fn decode(&mut self, packet: &MediaPacket) -> MediaResult<Option<DecodedAudioChunk>> {
    let sym_packet =
      SymphoniaPacket::new_from_slice(0, packet.pts_ns, packet.duration_ns, &packet.data);

    let decoded = self
      .decoder
      .decode(&sym_packet)
      .map_err(|e| MediaError::Decode(format!("AAC decode failed: {e}")))?;

    let spec = *decoded.spec();
    let sample_rate_hz = spec.rate;
    let channels = spec.channels.count() as u16;

    let mut sample_buf = SampleBuffer::<f32>::new(decoded.capacity() as u64, spec);
    sample_buf.copy_interleaved_ref(decoded);
    let samples = sample_buf.samples().to_vec();

    if samples.is_empty() {
      return Ok(None);
    }

    let duration_ns = if packet.duration_ns != 0 {
      packet.duration_ns
    } else if sample_rate_hz == 0 || channels == 0 {
      0
    } else {
      // Best-effort fallback when the container didn't provide an explicit duration.
      let frames = samples.len() / channels as usize;
      ((frames as u128)
        .saturating_mul(1_000_000_000u128)
        .checked_div(sample_rate_hz as u128)
        .unwrap_or(0)
        .min(u128::from(u64::MAX))) as u64
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

  use symphonia_core::formats::FormatReader;
  use symphonia_core::formats::FormatOptions;
  use symphonia_core::io::{MediaSourceStream, MediaSourceStreamOptions};
  use symphonia_format_isomp4::IsoMp4Reader;

  #[test]
  fn decodes_first_packet_from_mp4_fixture() {
    let fixture_path = crate::testing::fixture_path("fixtures/media/aac_stereo_48000.mp4");
    let mp4_bytes = std::fs::read(fixture_path).expect("read mp4 fixture");

    let cursor = std::io::Cursor::new(mp4_bytes);
    let mss = MediaSourceStream::new(Box::new(cursor), MediaSourceStreamOptions::default());

    let mut format =
      IsoMp4Reader::try_new(mss, &FormatOptions::default()).expect("open mp4 demuxer");

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

    let mut decoder = AacDecoder::new(&asc, sample_rate, channels).expect("init decoder");

    let mut first_packet = None;
    for _ in 0..32 {
      let pkt = format.next_packet().expect("next packet");
      if pkt.track_id() == track.id {
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

    let media_packet = MediaPacket {
      track_id: u64::from(pkt.track_id()),
      pts_ns,
      duration_ns,
      data: pkt.data().to_vec(),
      is_keyframe: false,
    };
    let decoded = decoder
      .decode(&media_packet)
      .expect("decode")
      .expect("decoded output");

    assert!(decoded.channels > 0);
    assert!(!decoded.samples.is_empty());
  }
}
