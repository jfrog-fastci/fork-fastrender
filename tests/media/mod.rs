#![cfg(feature = "media")]

use fastrender::media::demuxer::Mp4PacketDemuxer;
use fastrender::media::{DecodedItem, MediaDecodePipeline, MediaError, MediaResult};
use std::fs::File;
use std::io::BufReader;

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
        assert!(
          frame.rgba.iter().any(|&b| b != 0),
          "expected decoded H264 frame to contain non-zero pixel data"
        );
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
fn mp4_vp9_decodes_first_video() -> MediaResult<()> {
  let demuxer = Mp4PacketDemuxer::open("tests/fixtures/media/vp9_in_mp4.mp4")?;
  let mut pipeline = MediaDecodePipeline::new(Box::new(demuxer))?;

  for _ in 0..32 {
    let Some(item) = pipeline.next_decoded()? else {
      break;
    };

    if let DecodedItem::Video(frame) = item {
      assert!(frame.width > 0);
      assert!(frame.height > 0);
      assert_eq!(frame.rgba.len(), (frame.width * frame.height * 4) as usize);
      return Ok(());
    }
  }

  Err(MediaError::Decode(
    "did not decode a VP9 video frame within limit".into(),
  ))
}

#[test]
fn webm_vp9_opus_decodes_first_video_and_audio() -> MediaResult<()> {
  let file = File::open("tests/fixtures/media/test_vp9_opus.webm")?;
  let demuxer = fastrender::media::demux::webm::WebmDemuxer::open(BufReader::new(file))?;
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
        assert!(
          frame.rgba.iter().any(|&b| b != 0),
          "expected decoded VP9 frame to contain non-zero pixel data"
        );
        got_video = true;
      }
      DecodedItem::Audio(chunk) => {
        assert_eq!(chunk.sample_rate_hz, 48_000);
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
fn mp4_vp9_decodes_first_video() -> MediaResult<()> {
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
    assert!(
      frame.rgba.iter().any(|&b| b != 0),
      "expected decoded VP9-in-MP4 frame to contain non-zero pixel data"
    );
    return Ok(());
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
