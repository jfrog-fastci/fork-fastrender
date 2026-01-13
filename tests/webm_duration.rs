#[cfg(feature = "media_webm")]
mod webm_duration {
  use fastrender::media::demux::webm::WebmDemuxer;
  use fastrender::media::MediaCodec;
  use std::io::Cursor;
  use std::path::PathBuf;
 
  const MAX_PACKET_DURATION_NS: u64 = 10_000_000_000;
 
  fn webm_fixture_bytes(name: &str) -> Vec<u8> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
      .join("tests/fixtures/media")
      .join(name);
    std::fs::read(&path).expect("read WebM fixture")
  }
 
  #[test]
  fn vp9_packets_have_nonzero_duration() {
    let bytes = webm_fixture_bytes("test_vp9_opus.webm");
    let mut demuxer = WebmDemuxer::open(Cursor::new(bytes.as_slice())).expect("open webm");
    let video_track = demuxer
      .tracks()
      .iter()
      .find(|t| t.codec == MediaCodec::Vp9)
      .map(|t| t.id)
      .expect("VP9 track");
 
    let mut first_video = None;
    while let Some(pkt) = demuxer.next_packet().expect("read packet") {
      if pkt.track_id == video_track {
        first_video = Some(pkt);
        break;
      }
    }
 
    let pkt = first_video.expect("expected VP9 packet");
    assert!(
      pkt.duration_ns > 0,
      "expected VP9 packet duration > 0; got {}",
      pkt.duration_ns
    );
    assert!(
      pkt.duration_ns < MAX_PACKET_DURATION_NS,
      "expected VP9 packet duration to be clamped; got {}",
      pkt.duration_ns
    );
  }
 
  #[cfg(feature = "codec_vp9_libvpx")]
  #[test]
  fn vp9_decoded_frames_propagate_duration() {
    use fastrender::media::decoder::create_video_decoder;
 
    let bytes = webm_fixture_bytes("test_vp9_opus.webm");
    let mut demuxer = WebmDemuxer::open(Cursor::new(bytes.as_slice())).expect("open webm");
    let track = demuxer
      .tracks()
      .iter()
      .find(|t| t.codec == MediaCodec::Vp9)
      .expect("VP9 track");
 
    let mut decoder = create_video_decoder(track).expect("create VP9 decoder");
 
    while let Some(pkt) = demuxer.next_packet().expect("read packet") {
      if pkt.track_id != track.id {
        continue;
      }
      let frames = decoder.decode(&pkt).expect("decode VP9");
      let frame = frames.into_iter().next().expect("decoded VP9 frame");
      assert!(
        frame.duration_ns > 0,
        "expected decoded VP9 frame duration > 0; got {}",
        frame.duration_ns
      );
      return;
    }
 
    panic!("expected VP9 packet");
  }
}

