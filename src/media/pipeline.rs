use super::decoder::{create_audio_decoder, create_video_decoder, AudioDecoder, VideoDecoder};
use super::demuxer::MediaDemuxer;
use super::{DecodedItem, MediaResult, MediaTrackInfo, MediaTrackType};
use std::collections::VecDeque;

/// A minimal decode pipeline that wires a container demuxer to codec decoders and yields decoded
/// output in demux order.
pub struct MediaDecodePipeline {
  demuxer: Box<dyn MediaDemuxer>,
  video_track: Option<MediaTrackInfo>,
  audio_track: Option<MediaTrackInfo>,
  video_decoder: Option<Box<dyn VideoDecoder>>,
  audio_decoder: Option<Box<dyn AudioDecoder>>,
  pending: VecDeque<DecodedItem>,
  preroll_drop_until_ns: Option<u64>,
}

impl MediaDecodePipeline {
  pub fn new(demuxer: Box<dyn MediaDemuxer>) -> MediaResult<Self> {
    let mut pipeline = Self {
      demuxer,
      video_track: None,
      audio_track: None,
      video_decoder: None,
      audio_decoder: None,
      pending: VecDeque::new(),
      preroll_drop_until_ns: None,
    };
    pipeline.init_decoders()?;
    Ok(pipeline)
  }

  fn init_decoders(&mut self) -> MediaResult<()> {
    self.video_track = None;
    self.audio_track = None;
    self.video_decoder = None;
    self.audio_decoder = None;

    for t in self.demuxer.tracks() {
      match t.track_type {
        MediaTrackType::Video if self.video_track.is_none() => {
          self.video_decoder = Some(create_video_decoder(t)?);
          self.video_track = Some(t.clone());
        }
        MediaTrackType::Audio if self.audio_track.is_none() => {
          self.audio_decoder = Some(create_audio_decoder(t)?);
          self.audio_track = Some(t.clone());
        }
        _ => {}
      }
    }

    Ok(())
  }

  /// Fetches the next decoded item in demux order.
  pub fn next_decoded(&mut self) -> MediaResult<Option<DecodedItem>> {
    loop {
      while let Some(item) = self.pending.pop_front() {
        if let Some(target_ns) = self.preroll_drop_until_ns {
          let pts_ns = match &item {
            DecodedItem::Video(frame) => frame.pts_ns,
            DecodedItem::Audio(chunk) => chunk.pts_ns,
          };

          if pts_ns < target_ns {
            continue;
          }

          // Stop preroll dropping once we've produced a "post-seek" video frame. If there is no
          // video track, stop once we produce any item at-or-after the target.
          if matches!(item, DecodedItem::Video(_)) || self.video_track.is_none() {
            self.preroll_drop_until_ns = None;
          }
        }

        return Ok(Some(item));
      }

      let Some(pkt) = self.demuxer.next_packet()? else {
        return Ok(None);
      };

      if let (Some(v), Some(dec)) = (self.video_track.as_ref(), self.video_decoder.as_mut()) {
        if pkt.track_id == v.id {
          let frames = dec.decode(&pkt)?;
          self
            .pending
            .extend(frames.into_iter().map(DecodedItem::Video));
        }
      }

      if let (Some(a), Some(dec)) = (self.audio_track.as_ref(), self.audio_decoder.as_mut()) {
        if pkt.track_id == a.id {
          let chunks = dec.decode(&pkt)?;
          self
            .pending
            .extend(chunks.into_iter().map(DecodedItem::Audio));
        }
      }

      // Loop back around to drain `pending` (with preroll-drop filtering).
    }
  }

  pub fn seek(&mut self, time_ns: u64) -> MediaResult<()> {
    self.demuxer.seek(time_ns)?;
    self.pending.clear();
    self.init_decoders()?;
    self.preroll_drop_until_ns = if time_ns == 0 { None } else { Some(time_ns) };
    Ok(())
  }
}
