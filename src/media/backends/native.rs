use crate::media::demux::webm::WebmDemuxer;
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

fn try_open_mp4(bytes: Arc<[u8]>) -> MediaResult<MediaDecodePipeline> {
  let reader = Cursor::new(bytes);
  let len = reader.get_ref().len() as u64;
  let mp4 = mp4::Mp4Reader::read_header(reader, len)
    .map_err(|e| MediaError::Demux(format!("mp4: failed to read header: {e}")))?;
  let demuxer = Mp4PacketDemuxer::from_reader(mp4)?;
  MediaDecodePipeline::new(Box::new(demuxer))
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
    match try_open_mp4(Arc::clone(&bytes)) {
      Ok(p) => return Ok(Box::new(p)),
      Err(mp4_err) => {
        match try_open_webm(bytes) {
          Ok(p) => Ok(Box::new(p)),
          Err(webm_err) => Err(MediaError::Demux(format!(
            "failed to open media: mp4={mp4_err}; webm={webm_err}"
          ))),
        }
      }
    }
  }
}

