use crate::media::{
  MediaAudioInfo, MediaCodec, MediaError, MediaPacket, MediaResult, MediaTrackInfo, MediaTrackType,
  MediaVideoInfo,
};
use matroska_demuxer::{DemuxError, Frame, MatroskaFile, TrackType};
use std::collections::{HashMap, VecDeque};
use std::io::{Read, Seek};

#[derive(Debug, Clone, Copy)]
pub struct WebmDemuxerOptions {
  /// If enabled, `next_packet()` yields packets ordered by PTS across tracks (video + audio), using
  /// a small bounded read-ahead buffer per track.
  pub inter_track_reordering: bool,
  /// Maximum number of queued packets per track when inter-track reordering is enabled.
  pub per_track_queue_capacity: usize,
}

impl Default for WebmDemuxerOptions {
  fn default() -> Self {
    Self {
      inter_track_reordering: true,
      per_track_queue_capacity: 8,
    }
  }
}

pub struct WebmDemuxer<R: Read + Seek> {
  mkv: MatroskaFile<R>,
  options: WebmDemuxerOptions,
  tracks: Vec<MediaTrackInfo>,
  timestamp_scale_ns: u64,
  /// Codec delay (nanoseconds) per track.
  codec_delay_ns: HashMap<u64, u64>,
  /// Max codec delay across supported tracks (nanoseconds).
  max_codec_delay_ns: u64,
  /// Track IDs for which we will emit packets (currently VP9 + Opus only), in deterministic order.
  active_track_ids: Vec<u64>,
  /// Bounded per-track packet queues for optional inter-track reordering.
  packet_queues: HashMap<u64, VecDeque<MediaPacket>>,
  frame: Frame,
  reached_eof: bool,
}

impl<R: Read + Seek> WebmDemuxer<R> {
  pub fn open(reader: R) -> MediaResult<Self> {
    Self::open_with_options(reader, WebmDemuxerOptions::default())
  }

  pub fn open_with_options(reader: R, options: WebmDemuxerOptions) -> MediaResult<Self> {
    if options.per_track_queue_capacity == 0 {
      return Err(MediaError::Unsupported(
        "invalid WebM demuxer queue capacity (must be >= 1)",
      ));
    }

    let mkv = MatroskaFile::open(reader).map_err(map_demux_error)?;
    let timestamp_scale_ns = mkv.info().timestamp_scale().get();

    let mut codec_delay_ns = HashMap::new();
    let mut max_codec_delay_ns = 0_u64;
    let mut tracks = Vec::new();

    for track in mkv.tracks() {
      let id = track.track_number().get();
      let codec_private = track.codec_private().unwrap_or(&[]).to_vec();
      let codec_delay = track.codec_delay().unwrap_or(0);

      let (track_type, video, audio) = match track.track_type() {
        TrackType::Video => {
          let video = track.video().map(|v| MediaVideoInfo {
            width: u32::try_from(v.pixel_width().get()).unwrap_or(u32::MAX),
            height: u32::try_from(v.pixel_height().get()).unwrap_or(u32::MAX),
          });
          (MediaTrackType::Video, video, None)
        }
        TrackType::Audio => {
          let audio = track.audio().map(|a| MediaAudioInfo {
            sample_rate: a.sampling_frequency().round() as u32,
            channels: u16::try_from(a.channels().get()).unwrap_or(u16::MAX),
          });
          (MediaTrackType::Audio, None, audio)
        }
        // We only expose audio/video tracks at the moment.
        _ => continue,
      };

      let codec = match track.codec_id() {
        "V_VP9" => MediaCodec::Vp9,
        "A_OPUS" => MediaCodec::Opus,
        other => MediaCodec::Unknown(other.to_string()),
      };

      // Store codec delay only for the codecs we currently emit packets for.
      if matches!(codec, MediaCodec::Vp9 | MediaCodec::Opus) {
        codec_delay_ns.insert(id, codec_delay);
        max_codec_delay_ns = max_codec_delay_ns.max(codec_delay);
      }

      tracks.push(MediaTrackInfo {
        id,
        track_type,
        codec,
        codec_private,
        codec_delay_ns: codec_delay,
        video,
        audio,
      });
    }

    let mut active_track_ids: Vec<u64> = codec_delay_ns.keys().copied().collect();
    active_track_ids.sort_unstable();

    let mut packet_queues = HashMap::new();
    for &track_id in &active_track_ids {
      packet_queues.insert(track_id, VecDeque::with_capacity(options.per_track_queue_capacity));
    }

    Ok(Self {
      mkv,
      options,
      tracks,
      timestamp_scale_ns,
      codec_delay_ns,
      max_codec_delay_ns,
      active_track_ids,
      packet_queues,
      frame: Frame::default(),
      reached_eof: false,
    })
  }

  pub fn tracks(&self) -> &[MediaTrackInfo] {
    &self.tracks
  }

  fn read_next_supported_packet(&mut self) -> MediaResult<Option<MediaPacket>> {
    loop {
      let has_frame = self
        .mkv
        .next_frame(&mut self.frame)
        .map_err(map_demux_error)?;
      if !has_frame {
        self.reached_eof = true;
        return Ok(None);
      }

      let codec_delay_ns = match self.codec_delay_ns.get(&self.frame.track) {
        Some(delay) => *delay,
        None => continue,
      };

      let pts_ns = (self.frame.timestamp as u128)
        .saturating_mul(self.timestamp_scale_ns as u128)
        .min(u128::from(u64::MAX)) as u64;
      let pts_ns = pts_ns.saturating_sub(codec_delay_ns);

      let duration_ns = self
        .frame
        .duration
        .map(|duration| {
          (duration as u128)
            .saturating_mul(self.timestamp_scale_ns as u128)
            .min(u128::from(u64::MAX)) as u64
        })
        .unwrap_or(0);

      let data = std::mem::take(&mut self.frame.data);
      let is_keyframe = self.frame.is_keyframe.unwrap_or(false);

      return Ok(Some(MediaPacket {
        track_id: self.frame.track,
        pts_ns,
        duration_ns,
        data,
        is_keyframe,
      }));
    }
  }

  fn fill_reorder_queues(&mut self) -> MediaResult<()> {
    if self.reached_eof {
      return Ok(());
    }

    while self
      .active_track_ids
      .iter()
      .any(|id| self.packet_queues.get(id).is_some_and(|q| q.is_empty()))
    {
      let Some(pkt) = self.read_next_supported_packet()? else {
        break;
      };

      let q = self
        .packet_queues
        .get_mut(&pkt.track_id)
        .expect("queue must exist for supported track");
      if q.len() >= self.options.per_track_queue_capacity {
        return Err(MediaError::Demux(format!(
          "WebM inter-track reorder buffer overflow (track {}, cap {})",
          pkt.track_id, self.options.per_track_queue_capacity
        )));
      }
      q.push_back(pkt);
    }

    Ok(())
  }

  fn pop_next_reordered_packet(&mut self) -> Option<MediaPacket> {
    let mut best_track: Option<u64> = None;
    let mut best_pts_ns: u64 = 0;

    for &track_id in &self.active_track_ids {
      let Some(front) = self.packet_queues.get(&track_id).and_then(|q| q.front()) else {
        continue;
      };

      match best_track {
        None => {
          best_track = Some(track_id);
          best_pts_ns = front.pts_ns;
        }
        Some(best_id) => {
          if front.pts_ns < best_pts_ns || (front.pts_ns == best_pts_ns && track_id < best_id) {
            best_track = Some(track_id);
            best_pts_ns = front.pts_ns;
          }
        }
      }
    }

    best_track.and_then(|track_id| self.packet_queues.get_mut(&track_id)?.pop_front())
  }

  pub fn next_packet(&mut self) -> MediaResult<Option<MediaPacket>> {
    if !self.options.inter_track_reordering || self.active_track_ids.len() <= 1 {
      return self.read_next_supported_packet();
    }

    self.fill_reorder_queues()?;
    Ok(self.pop_next_reordered_packet())
  }

  pub fn seek(&mut self, time_ns: u64) -> MediaResult<()> {
    if self.timestamp_scale_ns == 0 {
      return Err(MediaError::Unsupported("invalid Matroska timestamp scale"));
    }

    // `codec_delay` must be subtracted from timestamps to get the actual PTS (see Matroska spec).
    // For a best-effort seek that guarantees `pts_ns >= time_ns` even after applying per-track
    // codec delay, we seek to `time_ns + max(codec_delay)` and then output PTS adjusted per track.
    let target_ns = time_ns.saturating_add(self.max_codec_delay_ns);

    // Convert nanoseconds to Matroska timecode units (inverse of timestamp_scale).
    // `MatroskaFile::seek()` places the cursor on the first frame with timestamp >= seek_timestamp.
    let seek_timestamp = target_ns
      .saturating_add(self.timestamp_scale_ns.saturating_sub(1))
      / self.timestamp_scale_ns;

    // Seeking invalidates any queued packets from the old position.
    for q in self.packet_queues.values_mut() {
      q.clear();
    }
    self.reached_eof = false;

    self.mkv.seek(seek_timestamp).map_err(|err| match err {
      // When seeking in damaged/unindexed files, the demuxer may not be able to locate clusters.
      DemuxError::CantFindCluster => {
        MediaError::Unsupported("Matroska seek unsupported (no cluster index)")
      }
      other => map_demux_error(other),
    })
  }
}

fn map_demux_error(err: DemuxError) -> MediaError {
  match err {
    DemuxError::IoError(err) => MediaError::Io(err),
    other => MediaError::Demux(other.to_string()),
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use std::io::Cursor;
  use std::path::PathBuf;

  fn webm_fixture_bytes(name: &str) -> Vec<u8> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
      .join("tests/fixtures/media")
      .join(name);
    std::fs::read(&path).expect("read WebM fixture")
  }

  #[test]
  fn demuxes_vp9_opus_and_seeks() {
    let bytes = webm_fixture_bytes("vp9_opus.webm");
    let mut demuxer = WebmDemuxer::open(Cursor::new(bytes.as_slice())).expect("open webm");

    let video_track = demuxer
      .tracks()
      .iter()
      .find(|t| t.codec == MediaCodec::Vp9)
      .map(|t| t.id)
      .expect("VP9 track");
    let audio_track = demuxer
      .tracks()
      .iter()
      .find(|t| t.codec == MediaCodec::Opus)
      .map(|t| t.id)
      .expect("Opus track");

    let mut saw_video = false;
    let mut saw_audio = false;
    while let Some(pkt) = demuxer.next_packet().expect("read packet") {
      if pkt.track_id == video_track {
        saw_video = true;
      }
      if pkt.track_id == audio_track {
        saw_audio = true;
      }
      if saw_video && saw_audio {
        break;
      }
    }
    assert!(saw_video, "expected at least one VP9 packet");
    assert!(saw_audio, "expected at least one Opus packet");

    // Seek to ~0.5s.
    let seek_target_ns = 500_000_000_u64;
    demuxer.seek(seek_target_ns).expect("seek");

    // Verify packets after seek are at/after the target (in nanoseconds, after codec delay
    // adjustment).
    let mut post_seek_video = false;
    let mut post_seek_audio = false;
    for _ in 0..1000 {
      let Some(pkt) = demuxer.next_packet().expect("read packet") else {
        break;
      };
      assert!(
        pkt.pts_ns >= seek_target_ns,
        "packet PTS {}ns is before seek target {}ns",
        pkt.pts_ns,
        seek_target_ns
      );
      if pkt.track_id == video_track {
        post_seek_video = true;
      }
      if pkt.track_id == audio_track {
        post_seek_audio = true;
      }
      if post_seek_video && post_seek_audio {
        break;
      }
    }
    assert!(post_seek_video, "expected VP9 packet after seek");
    assert!(post_seek_audio, "expected Opus packet after seek");
  }

  #[test]
  fn next_packet_pts_are_non_decreasing_across_tracks() {
    let bytes = webm_fixture_bytes("vp9_opus.webm");
    let mut demuxer = WebmDemuxer::open_with_options(
      Cursor::new(bytes.as_slice()),
      WebmDemuxerOptions {
        inter_track_reordering: true,
        per_track_queue_capacity: 8,
      },
    )
    .expect("open webm");

    let mut last_pts_ns = None::<u64>;
    for _ in 0..500 {
      let Some(pkt) = demuxer.next_packet().expect("read packet") else {
        break;
      };
      if let Some(prev) = last_pts_ns {
        assert!(
          pkt.pts_ns >= prev,
          "expected non-decreasing PTS, got {}ns then {}ns (track {})",
          prev,
          pkt.pts_ns,
          pkt.track_id
        );
      }
      last_pts_ns = Some(pkt.pts_ns);
    }
  }
}
