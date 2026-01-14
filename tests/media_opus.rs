#![cfg(all(feature = "codec_opus", feature = "media_webm"))]

use fastrender::media::codecs::opus::{OpusDecoder, OPUS_SAMPLE_RATE_HZ};
use fastrender::media::demux::webm::WebmDemuxer;
use fastrender::media::MediaCodec;
use std::io::Cursor;

#[test]
fn decodes_first_opus_frame_from_webm_fixture() {
  // Use an audio-only WebM fixture to avoid inter-track packet reordering complexities.
  let bytes = include_bytes!("pages/fixtures/media_playback/assets/test_opus.webm");
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

  let mut decoded = None;
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
  assert!(decoded.channels > 0);
  assert!(!decoded.samples.is_empty());
  assert_eq!(decoded.samples.len() % decoded.channels as usize, 0);
}
