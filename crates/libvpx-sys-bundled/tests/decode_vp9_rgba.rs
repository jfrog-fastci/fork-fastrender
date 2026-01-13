use libvpx_sys_bundled::Vp9Decoder;
use matroska_demuxer::{Frame, MatroskaFile, TrackType};
use std::io::Cursor;
use std::path::PathBuf;

#[test]
fn decodes_vp9_frame_to_rgba8_from_webm_fixture() {
  // Reuse the workspace's deterministic CC0 fixture.
  let fixture = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
    .join("../../tests/fixtures/media/test_vp9_opus.webm");
  let bytes = std::fs::read(&fixture).expect("read test_vp9_opus.webm fixture");

  let mut mkv = MatroskaFile::open(Cursor::new(bytes)).expect("open Matroska/WebM");
  let video_track = mkv
    .tracks()
    .iter()
    .find(|t| t.track_type() == TrackType::Video && t.codec_id() == "V_VP9")
    .map(|t| t.track_number().get())
    .expect("VP9 track not found in fixture");

  let mut frame = Frame::default();
  loop {
    let has_frame = mkv.next_frame(&mut frame).expect("read Matroska frame");
    assert!(has_frame, "fixture contained no frames");
    if frame.track == video_track {
      break;
    }
  }

  let mut decoder = Vp9Decoder::new(1).expect("init VP9 decoder");
  let frames = decoder.decode(&frame.data).expect("decode VP9 packet");
  assert!(!frames.is_empty(), "expected at least one output frame");
  for out in frames {
    assert_eq!((out.width, out.height), (64, 64));
    assert_eq!(out.rgba8.len(), 64 * 64 * 4);
  }

  // Flushing (decode with NULL/0) should not error.
  let _ = decoder.decode(&[]).expect("flush");
}
