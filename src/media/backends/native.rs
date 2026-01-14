#[cfg(feature = "media_mp4")]
use crate::media::demux::mp4parse::Mp4ParseDemuxer;
use crate::media::demux::webm::WebmDemuxer;
use crate::media::{MediaBackend, MediaDecodePipeline, MediaError, MediaResult, MediaSession};
use std::sync::Arc;

/// Native demux+decode backend using the in-process container parsers + codec libraries.
#[derive(Debug, Default, Clone, Copy)]
pub struct NativeBackend;

impl NativeBackend {
  #[must_use]
  pub fn new() -> Self {
    Self
  }
}

#[cfg(feature = "media_mp4")]
fn try_open_mp4(bytes: Arc<[u8]>) -> MediaResult<MediaDecodePipeline> {
  let demuxer = Mp4ParseDemuxer::open(std::io::Cursor::new(bytes))?;
  MediaDecodePipeline::new(Box::new(demuxer))
}

#[cfg(not(feature = "media_mp4"))]
fn try_open_mp4(_bytes: Arc<[u8]>) -> MediaResult<MediaDecodePipeline> {
  Err(MediaError::Unsupported(
    "`media_mp4` feature disabled (enable Cargo feature `media_mp4` or `media`)".into(),
  ))
}

fn try_open_webm(bytes: Arc<[u8]>) -> MediaResult<MediaDecodePipeline> {
  let demuxer = WebmDemuxer::open(std::io::Cursor::new(bytes))?;
  MediaDecodePipeline::new(Box::new(demuxer))
}

impl MediaBackend for NativeBackend {
  fn name(&self) -> &'static str {
    "native"
  }

  fn available(&self) -> bool {
    true
  }

  fn open(&self, bytes: Arc<[u8]>) -> MediaResult<Box<dyn MediaSession>> {
    let mp4_err = match try_open_mp4(Arc::clone(&bytes)) {
      Ok(p) => return Ok(Box::new(p)),
      Err(err) => err,
    };

    let webm_err = match try_open_webm(bytes) {
      Ok(p) => return Ok(Box::new(p)),
      Err(err) => err,
    };

    match (&mp4_err, &webm_err) {
      (MediaError::Unsupported(_), MediaError::Unsupported(_)) => Err(MediaError::Unsupported(
        "no media container backends enabled (enable Cargo feature `media_mp4`/`media_webm` or `media`)".into(),
      )),
      _ => Err(MediaError::Demux(format!(
        "failed to open media: mp4={mp4_err}; webm={webm_err}"
      ))),
    }
  }
}
