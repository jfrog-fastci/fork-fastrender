use crate::media::demux::webm::WebmDemuxer;
#[cfg(feature = "media_mp4")]
use crate::media::demuxer::Mp4PacketDemuxer;
use crate::media::{MediaBackend, MediaDecodePipeline, MediaError, MediaResult, MediaSession};
use std::io::Cursor;
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
  let reader = Cursor::new(bytes);
  let len = reader.get_ref().len() as u64;
  let mp4 = mp4::Mp4Reader::read_header(reader, len)
    .map_err(|e| MediaError::Demux(format!("mp4: failed to read header: {e}")))?;
  let demuxer = Mp4PacketDemuxer::from_reader(mp4)?;
  MediaDecodePipeline::new(Box::new(demuxer))
}

#[cfg(not(feature = "media_mp4"))]
fn try_open_mp4(_bytes: Arc<[u8]>) -> MediaResult<MediaDecodePipeline> {
  Err(MediaError::Unsupported(
    "`media_mp4` feature disabled (enable Cargo feature `media_mp4` or `media`)",
  ))
}

fn try_open_webm(bytes: Arc<[u8]>) -> MediaResult<MediaDecodePipeline> {
  let demuxer = WebmDemuxer::open(Cursor::new(bytes))?;
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
    let mp4_res = try_open_mp4(Arc::clone(&bytes));
    if let Ok(p) = mp4_res {
      return Ok(Box::new(p));
    }
    let mp4_err = match mp4_res {
      Ok(_) => unreachable!("mp4_res handled Ok above"), // fastrender-allow-unwrap
      Err(err) => err,
    };

    let webm_res = try_open_webm(bytes);
    if let Ok(p) = webm_res {
      return Ok(Box::new(p));
    }
    let webm_err = match webm_res {
      Ok(_) => unreachable!("webm_res handled Ok above"), // fastrender-allow-unwrap
      Err(err) => err,
    };

    match (&mp4_err, &webm_err) {
      (MediaError::Unsupported(_), MediaError::Unsupported(_)) => Err(MediaError::Unsupported(
        "no media container backends enabled (enable Cargo feature `media_mp4`/`media_webm` or `media`)",
      )),
      _ => Err(MediaError::Demux(format!(
        "failed to open media: mp4={mp4_err}; webm={webm_err}"
      ))),
    }
  }
}
