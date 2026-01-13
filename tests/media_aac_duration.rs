#![cfg(feature = "codec_aac")]

use fastrender::media::codecs::aac::AacDecoder;
use fastrender::media::MediaPacket;
use symphonia_core::codecs::CODEC_TYPE_AAC;
use symphonia_core::formats::{FormatOptions, FormatReader};
use symphonia_core::io::{MediaSourceStream, MediaSourceStreamOptions};
use symphonia_format_isomp4::IsoMp4Reader;

#[test]
fn aac_packet_duration_matches_decoded_samples() {
  let fixture_path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
    .join("tests/fixtures/media/aac_stereo_48000.mp4");
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
  let sample_rate_hz = track.codec_params.sample_rate.expect("sample rate");
  let channels = track
    .codec_params
    .channels
    .expect("channels")
    .count() as u16;

  let mut decoder = AacDecoder::new(&asc, sample_rate_hz, channels).expect("init decoder");

  let mut first_packet = None;
  for _ in 0..32 {
    let pkt = format.next_packet().expect("next packet");
    if pkt.track_id() == track.id {
      first_packet = Some(pkt);
      break;
    }
  }
  let pkt = first_packet.expect("aac packet");

  let expected_ns = (1024u64 * 1_000_000_000u64) / u64::from(sample_rate_hz);

  // Intentionally pass a wrong demux duration to ensure the decoder prefers the duration implied
  // by the decoded output.
  let media_packet = MediaPacket {
    track_id: u64::from(pkt.track_id()),
    dts_ns: 0,
    pts_ns: 0,
    duration_ns: expected_ns.saturating_add(123),
    data: pkt.data().to_vec().into(),
    is_keyframe: false,
  };

  let chunk = decoder
    .decode(&media_packet)
    .expect("decode")
    .expect("decoded output");

  assert_eq!(chunk.sample_rate_hz, sample_rate_hz);
  assert_eq!(chunk.channels, 2);

  let frames = chunk.samples.len() / chunk.channels as usize;
  assert_eq!(frames, 1024, "expected AAC-LC frame size");

  let diff = chunk.duration_ns.abs_diff(expected_ns);
  assert!(
    diff <= 1,
    "duration_ns={} expected={} diff={}",
    chunk.duration_ns,
    expected_ns,
    diff
  );
}
