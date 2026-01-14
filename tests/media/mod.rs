#![cfg(feature = "media")]

use fastrender::media::demuxer::Mp4PacketDemuxer;
use fastrender::media::{DecodedItem, MediaDecodePipeline, MediaError, MediaResult};
use std::fs::File;
use std::io::BufReader;
use std::sync::Arc;

#[derive(Debug, Clone, Copy)]
struct RgbaStats {
  avg_r: f64,
  avg_g: f64,
  avg_b: f64,
  avg_a: f64,
  center: [u8; 4],
}

fn rgba_stats(pixels: &[u8], width: usize, height: usize) -> RgbaStats {
  assert_eq!(
    pixels.len(),
    width * height * 4,
    "expected raw RGBA buffer length to match {}×{}×4, got {} bytes",
    width,
    height,
    pixels.len()
  );

  let mut sum_r: u64 = 0;
  let mut sum_g: u64 = 0;
  let mut sum_b: u64 = 0;
  let mut sum_a: u64 = 0;
  for px in pixels.chunks_exact(4) {
    sum_r += px[0] as u64;
    sum_g += px[1] as u64;
    sum_b += px[2] as u64;
    sum_a += px[3] as u64;
  }

  let denom = (width * height) as f64;
  let avg_r = sum_r as f64 / denom;
  let avg_g = sum_g as f64 / denom;
  let avg_b = sum_b as f64 / denom;
  let avg_a = sum_a as f64 / denom;

  let cx = width / 2;
  let cy = height / 2;
  let idx = (cy * width + cx) * 4;
  let center = [pixels[idx], pixels[idx + 1], pixels[idx + 2], pixels[idx + 3]];

  RgbaStats {
    avg_r,
    avg_g,
    avg_b,
    avg_a,
    center,
  }
}

fn assert_mostly_red(label: &str, stats: RgbaStats) {
  assert!(
    stats.avg_r > 180.0,
    "{label}: expected avg R to be high, got {:.2} (avg G={:.2}, avg B={:.2}, avg A={:.2}, center={:?})",
    stats.avg_r,
    stats.avg_g,
    stats.avg_b,
    stats.avg_a,
    stats.center
  );
  assert!(
    stats.avg_g < 80.0,
    "{label}: expected avg G to be low, got {:.2} (avg R={:.2}, avg B={:.2}, avg A={:.2}, center={:?})",
    stats.avg_g,
    stats.avg_r,
    stats.avg_b,
    stats.avg_a,
    stats.center
  );
  assert!(
    stats.avg_b < 80.0,
    "{label}: expected avg B to be low, got {:.2} (avg R={:.2}, avg G={:.2}, avg A={:.2}, center={:?})",
    stats.avg_b,
    stats.avg_r,
    stats.avg_g,
    stats.avg_a,
    stats.center
  );
  assert!(
    stats.avg_a > 250.0,
    "{label}: expected avg A to be ~255, got {:.2} (avg R={:.2}, avg G={:.2}, avg B={:.2}, center={:?})",
    stats.avg_a,
    stats.avg_r,
    stats.avg_g,
    stats.avg_b,
    stats.center
  );

  // Dominance (helps catch channel swaps).
  assert!(
    stats.avg_r > stats.avg_g + 100.0,
    "{label}: expected avg R to dominate avg G, got avg R={:.2} avg G={:.2}",
    stats.avg_r,
    stats.avg_g
  );
  assert!(
    stats.avg_r > stats.avg_b + 100.0,
    "{label}: expected avg R to dominate avg B, got avg R={:.2} avg B={:.2}",
    stats.avg_r,
    stats.avg_b
  );

  assert!(
    stats.center[0] > 180
      && stats.center[1] < 80
      && stats.center[2] < 80
      && stats.center[3] > 250,
    "{label}: expected center pixel to be red-dominant, got {:?}",
    stats.center
  );
}

fn assert_mostly_blue(label: &str, stats: RgbaStats) {
  assert!(
    stats.avg_b > 180.0,
    "{label}: expected avg B to be high, got {:.2} (avg R={:.2}, avg G={:.2}, avg A={:.2}, center={:?})",
    stats.avg_b,
    stats.avg_r,
    stats.avg_g,
    stats.avg_a,
    stats.center
  );
  assert!(
    stats.avg_r < 80.0,
    "{label}: expected avg R to be low, got {:.2} (avg B={:.2}, avg G={:.2}, avg A={:.2}, center={:?})",
    stats.avg_r,
    stats.avg_b,
    stats.avg_g,
    stats.avg_a,
    stats.center
  );
  assert!(
    stats.avg_g < 80.0,
    "{label}: expected avg G to be low, got {:.2} (avg B={:.2}, avg R={:.2}, avg A={:.2}, center={:?})",
    stats.avg_g,
    stats.avg_b,
    stats.avg_r,
    stats.avg_a,
    stats.center
  );
  assert!(
    stats.avg_a > 250.0,
    "{label}: expected avg A to be ~255, got {:.2} (avg R={:.2}, avg G={:.2}, avg B={:.2}, center={:?})",
    stats.avg_a,
    stats.avg_r,
    stats.avg_g,
    stats.avg_b,
    stats.center
  );

  // Dominance (helps catch channel swaps).
  assert!(
    stats.avg_b > stats.avg_r + 100.0,
    "{label}: expected avg B to dominate avg R, got avg B={:.2} avg R={:.2}",
    stats.avg_b,
    stats.avg_r
  );
  assert!(
    stats.avg_b > stats.avg_g + 100.0,
    "{label}: expected avg B to dominate avg G, got avg B={:.2} avg G={:.2}",
    stats.avg_b,
    stats.avg_g
  );

  assert!(
    stats.center[2] > 180
      && stats.center[0] < 80
      && stats.center[1] < 80
      && stats.center[3] > 250,
    "{label}: expected center pixel to be blue-dominant, got {:?}",
    stats.center
  );
}

#[test]
fn mp4_h264_aac_decodes_first_video_and_audio() -> MediaResult<()> {
  let demuxer = Mp4PacketDemuxer::open("tests/fixtures/media/test_h264_aac.mp4")?;
  let mut pipeline = MediaDecodePipeline::new(Box::new(demuxer))?;

  let mut got_video = false;
  let mut got_audio = false;

  for _ in 0..128 {
    let Some(item) = pipeline.next_decoded()? else {
      break;
    };

    match item {
      DecodedItem::Video(frame) => {
        assert_eq!((frame.width, frame.height), (64, 64));
        assert_eq!(frame.rgba.len(), (frame.width * frame.height * 4) as usize);
        let stats = rgba_stats(&frame.rgba, frame.width as usize, frame.height as usize);
        assert_mostly_red("mp4/h264 first frame (test_h264_aac.mp4)", stats);
        got_video = true;
      }
      DecodedItem::Audio(chunk) => {
        assert!(chunk.sample_rate_hz > 0);
        assert!(chunk.channels > 0);
        assert!(!chunk.samples.is_empty());
        got_audio = true;
      }
    }

    if got_video && got_audio {
      return Ok(());
    }
  }

  Err(MediaError::Decode(format!(
    "did not decode both video ({got_video}) and audio ({got_audio}) within limit"
  )))
}

#[test]
fn webm_vp9_opus_decodes_first_video_and_audio() -> MediaResult<()> {
  let file = File::open("tests/fixtures/media/test_vp9_opus.webm")?;
  let demuxer = fastrender::media::demux::webm::WebmDemuxer::open(BufReader::new(file))?;
  let mut pipeline = MediaDecodePipeline::new(Box::new(demuxer))?;

  let mut video_frames = Vec::new();
  let mut got_audio = false;

  for _ in 0..256 {
    let Some(item) = pipeline.next_decoded()? else {
      break;
    };

    match item {
      DecodedItem::Video(frame) => {
        assert_eq!((frame.width, frame.height), (64, 64));
        assert_eq!(frame.rgba.len(), (frame.width * frame.height * 4) as usize);
        if video_frames.len() < 2 {
          video_frames.push(frame);
        }
      }
      DecodedItem::Audio(chunk) => {
        assert_eq!(chunk.sample_rate_hz, 48_000);
        assert!(chunk.channels > 0);
        assert!(!chunk.samples.is_empty());
        got_audio = true;
      }
    }

    if video_frames.len() >= 2 && got_audio {
      break;
    }
  }

  if !got_audio || video_frames.len() < 2 {
    return Err(MediaError::Decode(format!(
      "did not decode 2 video frames (got {}) and audio ({got_audio}) within limit",
      video_frames.len()
    )));
  }

  // Deterministic fixture: 2 frames, red then blue.
  video_frames.sort_by_key(|f| f.pts_ns);
  let (first, second) = (&video_frames[0], &video_frames[1]);

  let stats0 = rgba_stats(&first.rgba, first.width as usize, first.height as usize);
  assert_mostly_red("webm/vp9 first frame (test_vp9_opus.webm)", stats0);

  let stats1 = rgba_stats(&second.rgba, second.width as usize, second.height as usize);
  assert_mostly_blue("webm/vp9 second frame (test_vp9_opus.webm)", stats1);

  Ok(())
}

#[test]
fn mp4_vp9_first_frame_is_red() -> MediaResult<()> {
  let demuxer = Mp4PacketDemuxer::open("tests/fixtures/media/vp9_in_mp4.mp4")?;
  let mut pipeline = MediaDecodePipeline::new(Box::new(demuxer))?;

  for _ in 0..128 {
    let Some(item) = pipeline.next_decoded()? else {
      break;
    };

    let DecodedItem::Video(frame) = item else {
      continue;
    };

    assert_eq!((frame.width, frame.height), (16, 16));
    assert_eq!(frame.rgba.len(), (frame.width * frame.height * 4) as usize);
    let stats = rgba_stats(&frame.rgba, frame.width as usize, frame.height as usize);
    assert_mostly_red("mp4/vp9 first frame (vp9_in_mp4.mp4)", stats);
    return Ok(());
  }

  Err(MediaError::Decode(
    "did not decode a VP9 video frame within limit".into(),
  ))
}

#[test]
fn native_backend_decodes_webm_vp9_first_frame_is_red() -> MediaResult<()> {
  use fastrender::media::backends::native::NativeBackend;
  use fastrender::media::{MediaBackend as _, MediaSession as _};

  let bytes = std::fs::read("tests/fixtures/media/test_vp9_opus.webm")?;
  let bytes: Arc<[u8]> = Arc::from(bytes);
  let backend = NativeBackend::new();
  let mut session = backend.open(bytes)?;

  for _ in 0..256 {
    let Some(item) = session.next_decoded()? else {
      break;
    };
    let DecodedItem::Video(frame) = item else {
      continue;
    };
    assert_eq!((frame.width, frame.height), (64, 64));
    let stats = rgba_stats(&frame.rgba, frame.width as usize, frame.height as usize);
    assert_mostly_red("native/webm+vp9 first frame (test_vp9_opus.webm)", stats);
    return Ok(());
  }

  Err(MediaError::Decode(
    "did not decode a VP9 video frame via NativeBackend within limit".into(),
  ))
}

#[test]
fn native_backend_decodes_mp4_vp9_first_frame_is_red() -> MediaResult<()> {
  use fastrender::media::backends::native::NativeBackend;
  use fastrender::media::{MediaBackend as _, MediaSession as _};

  let bytes = std::fs::read("tests/fixtures/media/vp9_in_mp4.mp4")?;
  let bytes: Arc<[u8]> = Arc::from(bytes);
  let backend = NativeBackend::new();
  let mut session = backend.open(bytes)?;

  for _ in 0..256 {
    let Some(item) = session.next_decoded()? else {
      break;
    };
    let DecodedItem::Video(frame) = item else {
      continue;
    };
    assert_eq!((frame.width, frame.height), (16, 16));
    let stats = rgba_stats(&frame.rgba, frame.width as usize, frame.height as usize);
    assert_mostly_red("native/mp4+vp9 first frame (vp9_in_mp4.mp4)", stats);
    return Ok(());
  }

  Err(MediaError::Decode(
    "did not decode a VP9 video frame via NativeBackend within limit".into(),
  ))
}

#[test]
fn webm_vp9_first_frame_has_nonzero_alpha() -> MediaResult<()> {
  let file = File::open("tests/fixtures/media/test_vp9_opus.webm")?;
  let demuxer = fastrender::media::demux::webm::WebmDemuxer::open(BufReader::new(file))?;
  let mut pipeline = MediaDecodePipeline::new(Box::new(demuxer))?;

  for _ in 0..128 {
    let Some(item) = pipeline.next_decoded()? else {
      break;
    };
    if let DecodedItem::Video(frame) = item {
      assert!(frame.width > 0);
      assert!(frame.height > 0);
      assert_eq!(frame.rgba.len(), (frame.width * frame.height * 4) as usize);
      assert!(
        frame.rgba[3..].iter().step_by(4).any(|a| *a != 0),
        "expected VP9 decoder to emit non-zero alpha values; got all-zero alpha channel"
      );
      return Ok(());
    }
  }

  Err(MediaError::Decode(
    "did not decode a VP9 video frame within limit".into(),
  ))
}

#[cfg(feature = "media_ffmpeg_cli")]
mod ffmpeg_cli {
  use super::*;
  use fastrender::media::backends::ffmpeg_cli::{ffmpeg_available, ffprobe_available, FfmpegCliBackend};
  use fastrender::media::{MediaBackend as _, MediaSession as _};
  use std::sync::Arc;

  #[test]
  fn ffmpeg_cli_decodes_mp4_fixture() -> MediaResult<()> {
    if !ffmpeg_available() || !ffprobe_available() {
      eprintln!("skipping: ffmpeg/ffprobe not available on PATH");
      return Ok(());
    }

    let bytes = std::fs::read("tests/fixtures/media/test_h264_aac.mp4")?;
    let bytes: Arc<[u8]> = Arc::from(bytes.into_boxed_slice());
    let backend = FfmpegCliBackend::new();
    let mut session = backend.open(bytes)?;

    let mut got_video = false;
    let mut got_audio = false;

    for _ in 0..256 {
      let Some(item) = session.next_decoded()? else {
        break;
      };
      match item {
        DecodedItem::Video(frame) => {
          assert!(frame.width > 0);
          assert!(frame.height > 0);
          assert_eq!(frame.rgba.len(), (frame.width * frame.height * 4) as usize);
          got_video = true;
        }
        DecodedItem::Audio(chunk) => {
          assert!(chunk.sample_rate_hz > 0);
          assert!(chunk.channels > 0);
          assert!(!chunk.samples.is_empty());
          got_audio = true;
        }
      }
      if got_video && got_audio {
        return Ok(());
      }
    }

    Err(MediaError::Decode(format!(
      "did not decode both video ({got_video}) and audio ({got_audio}) within limit"
    )))
  }

  #[test]
  fn ffmpeg_cli_decodes_webm_fixture() -> MediaResult<()> {
    if !ffmpeg_available() || !ffprobe_available() {
      eprintln!("skipping: ffmpeg/ffprobe not available on PATH");
      return Ok(());
    }

    let bytes = std::fs::read("tests/fixtures/media/test_vp9_opus.webm")?;
    let bytes: Arc<[u8]> = Arc::from(bytes.into_boxed_slice());
    let backend = FfmpegCliBackend::new();
    let mut session = backend.open(bytes)?;

    let mut got_video = false;
    let mut got_audio = false;

    for _ in 0..256 {
      let Some(item) = session.next_decoded()? else {
        break;
      };
      match item {
        DecodedItem::Video(frame) => {
          assert!(frame.width > 0);
          assert!(frame.height > 0);
          assert_eq!(frame.rgba.len(), (frame.width * frame.height * 4) as usize);
          got_video = true;
        }
        DecodedItem::Audio(chunk) => {
          assert!(chunk.sample_rate_hz > 0);
          assert!(chunk.channels > 0);
          assert!(!chunk.samples.is_empty());
          got_audio = true;
        }
      }
      if got_video && got_audio {
        return Ok(());
      }
    }

    Err(MediaError::Decode(format!(
      "did not decode both video ({got_video}) and audio ({got_audio}) within limit"
    )))
  }
}
