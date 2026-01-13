use crate::media::{
  MediaAudioInfo, MediaCodec, MediaError, MediaPacket, MediaResult, MediaTrackInfo, MediaTrackType,
  MediaVideoInfo,
};
use matroska_demuxer::{DemuxError, Frame, MatroskaFile, TrackType};
use std::collections::HashMap;
use std::io::{Read, Seek};

pub struct WebmDemuxer<R: Read + Seek> {
  mkv: MatroskaFile<R>,
  tracks: Vec<MediaTrackInfo>,
  timestamp_scale_ns: u64,
  /// Codec delay (nanoseconds) per track.
  codec_delay_ns: HashMap<u64, u64>,
  /// Max codec delay across supported tracks (nanoseconds).
  max_codec_delay_ns: u64,
  frame: Frame,
}

impl<R: Read + Seek> WebmDemuxer<R> {
  pub fn open(reader: R) -> MediaResult<Self> {
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

    Ok(Self {
      mkv,
      tracks,
      timestamp_scale_ns,
      codec_delay_ns,
      max_codec_delay_ns,
      frame: Frame::default(),
    })
  }

  pub fn tracks(&self) -> &[MediaTrackInfo] {
    &self.tracks
  }

  pub fn next_packet(&mut self) -> MediaResult<Option<MediaPacket>> {
    loop {
      let has_frame = self
        .mkv
        .next_frame(&mut self.frame)
        .map_err(map_demux_error)?;
      if !has_frame {
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
}
