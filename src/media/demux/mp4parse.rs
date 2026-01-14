use crate::media::track_selection::{
  select_primary_audio_track_id, select_primary_video_track_id, TrackCandidate, TrackFilterMode,
  TrackSelectionPolicy,
};
use crate::media::{
  MediaAudioInfo, MediaCodec, MediaError, MediaPacket, MediaResult, MediaTrackInfo, MediaTrackType,
  MediaVideoInfo,
};
use std::io::{Read, Seek, SeekFrom};

#[derive(Debug, Clone, Copy)]
pub struct Mp4ParseDemuxerOptions {
  pub track_selection_policy: TrackSelectionPolicy,
  pub track_filter: TrackFilterMode,
}

impl Default for Mp4ParseDemuxerOptions {
  fn default() -> Self {
    Self {
      track_selection_policy: TrackSelectionPolicy::default(),
      track_filter: TrackFilterMode::PrimaryOnly,
    }
  }
}

#[derive(Debug, Clone, Copy)]
enum Mp4TrackKind {
  Video,
  Audio,
  AuxiliaryVideo,
  Picture,
  Metadata,
  Other,
}

fn mp4_track_kind(track_type: &mp4parse::TrackType) -> Mp4TrackKind {
  match track_type {
    mp4parse::TrackType::Video => Mp4TrackKind::Video,
    mp4parse::TrackType::Audio => Mp4TrackKind::Audio,
    mp4parse::TrackType::AuxiliaryVideo => Mp4TrackKind::AuxiliaryVideo,
    mp4parse::TrackType::Picture => Mp4TrackKind::Picture,
    mp4parse::TrackType::Metadata => Mp4TrackKind::Metadata,
    _ => Mp4TrackKind::Other,
  }
}

#[derive(Debug, Clone, Copy)]
struct Mp4TrackSelectionInfo {
  id: u32,
  kind: Mp4TrackKind,
  enabled: bool,
  pixel_count: u64,
}

fn select_primary_track_ids(
  tracks: &[Mp4TrackSelectionInfo],
  policy: TrackSelectionPolicy,
) -> (Option<u32>, Option<u32>) {
  let mut video_candidates = Vec::new();
  let mut audio_candidates = Vec::new();

  for t in tracks {
    match t.kind {
      Mp4TrackKind::Video => {
        video_candidates.push(TrackCandidate {
          id: t.id,
          enabled: t.enabled,
          // MP4 does not have a single well-defined "default" concept like WebM `FlagDefault`.
          default: false,
          // MP4 track "kind" can express these, but mp4parse does not currently expose them.
          commentary: false,
          hearing_impaired: false,
          pixel_count: t.pixel_count,
        });
      }
      Mp4TrackKind::Audio => {
        audio_candidates.push(TrackCandidate {
          id: t.id,
          enabled: t.enabled,
          default: false,
          commentary: false,
          hearing_impaired: false,
          pixel_count: 0,
        });
      }
      // Ignore auxiliary/picture/metadata/etc for primary track selection.
      _ => {}
    }
  }

  (
    select_primary_video_track_id(&video_candidates, policy),
    select_primary_audio_track_id(&audio_candidates, policy),
  )
}

#[derive(Debug, Clone)]
struct Mp4SampleInfo {
  offset: u64,
  size: u32,
  dts_ns: u64,
  pts_ns: u64,
  duration_ns: u64,
  is_sync: bool,
}

#[derive(Debug)]
struct ActiveTrack {
  id: u32,
  samples: Vec<Mp4SampleInfo>,
  next_sample: usize,
}

pub struct Mp4ParseDemuxer<R: Read + Seek> {
  reader: R,
  options: Mp4ParseDemuxerOptions,
  tracks: Vec<MediaTrackInfo>,
  primary_video_track_id: Option<u32>,
  primary_audio_track_id: Option<u32>,
  active_tracks: Vec<ActiveTrack>,
}

impl<R: Read + Seek> Mp4ParseDemuxer<R> {
  pub fn open(reader: R) -> MediaResult<Self> {
    Self::open_with_options(reader, Mp4ParseDemuxerOptions::default())
  }

  pub fn open_with_options(mut reader: R, options: Mp4ParseDemuxerOptions) -> MediaResult<Self> {
    let ctx = read_mp4_context(&mut reader)?;

    let mut selection_infos = Vec::new();
    let mut tracks = Vec::new();

    for track in &ctx.tracks {
      let Some(id) = track.track_id else {
        continue;
      };
      let kind = mp4_track_kind(&track.track_type);

      // Extract a conservative enabled flag. If mp4parse didn't parse tkhd, assume enabled.
      let enabled = track.tkhd.as_ref().map(|t| !t.disabled).unwrap_or(true);

      let pixel_count = if matches!(kind, Mp4TrackKind::Video) {
        video_pixel_count(track)
      } else {
        0
      };

      selection_infos.push(Mp4TrackSelectionInfo {
        id,
        kind,
        enabled,
        pixel_count,
      });

      let (media_track_type, video, audio) = match kind {
        Mp4TrackKind::Video => {
          let (width, height) = video_dimensions(track);
          let video = width.zip(height).map(|(w, h)| MediaVideoInfo {
            width: u32::from(w),
            height: u32::from(h),
          });
          (MediaTrackType::Video, video, None)
        }
        Mp4TrackKind::Audio => {
          let (sample_rate, channels) = audio_format(track);
          let audio = match (sample_rate, channels) {
            (Some(sr), Some(ch)) => Some(MediaAudioInfo {
              sample_rate: sr,
              channels: ch,
            }),
            _ => None,
          };
          (MediaTrackType::Audio, None, audio)
        }
        // We only surface audio/video tracks for now.
        _ => continue,
      };

      let (codec, codec_private) = track_codec_and_extradata(track);

      tracks.push(MediaTrackInfo {
        id: u64::from(id),
        track_type: media_track_type,
        codec,
        codec_private,
        codec_delay_ns: 0,
        video,
        audio,
      });
    }

    let (primary_video_track_id, primary_audio_track_id) =
      select_primary_track_ids(&selection_infos, options.track_selection_policy);

    let mut active_track_ids: Vec<u32> = match options.track_filter {
      TrackFilterMode::AllTracks => selection_infos
        .iter()
        .filter_map(|t| match t.kind {
          Mp4TrackKind::Video | Mp4TrackKind::Audio => Some(t.id),
          _ => None,
        })
        .collect(),
      TrackFilterMode::PrimaryOnly => {
        let mut ids = Vec::new();
        if let Some(id) = primary_video_track_id {
          ids.push(id);
        }
        if let Some(id) = primary_audio_track_id {
          if !ids.contains(&id) {
            ids.push(id);
          }
        }
        ids
      }
    };
    active_track_ids.sort_unstable();

    let mut active_tracks = Vec::new();
    for id in active_track_ids {
      let Some(track) = ctx.tracks.iter().find(|t| t.track_id == Some(id)) else {
        continue;
      };
      let samples = build_sample_list(track)?;
      active_tracks.push(ActiveTrack {
        id,
        samples,
        next_sample: 0,
      });
    }

    Ok(Self {
      reader,
      options,
      tracks,
      primary_video_track_id,
      primary_audio_track_id,
      active_tracks,
    })
  }

  pub fn tracks(&self) -> &[MediaTrackInfo] {
    &self.tracks
  }

  pub fn primary_video_track_id(&self) -> Option<u64> {
    self.primary_video_track_id.map(u64::from)
  }

  pub fn primary_audio_track_id(&self) -> Option<u64> {
    self.primary_audio_track_id.map(u64::from)
  }

  pub fn next_packet(&mut self) -> MediaResult<Option<MediaPacket>> {
    let mut best_track_idx: Option<usize> = None;
    let mut best_dts_ns: u64 = 0;
    let mut best_track_id: u32 = 0;

    for (idx, t) in self.active_tracks.iter().enumerate() {
      let Some(sample) = t.samples.get(t.next_sample) else {
        continue;
      };

      match best_track_idx {
        None => {
          best_track_idx = Some(idx);
          best_dts_ns = sample.dts_ns;
          best_track_id = t.id;
        }
        Some(_) => {
          if sample.dts_ns < best_dts_ns || (sample.dts_ns == best_dts_ns && t.id < best_track_id) {
            best_track_idx = Some(idx);
            best_dts_ns = sample.dts_ns;
            best_track_id = t.id;
          }
        }
      }
    }

    let Some(track_idx) = best_track_idx else {
      return Ok(None);
    };

    let track = &mut self.active_tracks[track_idx];
    let sample = track
      .samples
      .get(track.next_sample)
      .expect("sample must exist");
    track.next_sample += 1;

    let size_usize = usize::try_from(sample.size)
      .map_err(|_| MediaError::Demux("sample size overflows usize".to_string()))?;
    let mut data = vec![0u8; size_usize];
    self
      .reader
      .seek(SeekFrom::Start(sample.offset))
      .map_err(MediaError::Io)?;
    self.reader.read_exact(&mut data).map_err(MediaError::Io)?;

    Ok(Some(MediaPacket {
      track_id: u64::from(track.id),
      dts_ns: sample.dts_ns,
      pts_ns: sample.pts_ns,
      duration_ns: sample.duration_ns,
      data: data.into(),
      is_keyframe: sample.is_sync,
    }))
  }
}

fn read_mp4_context<R: Read + Seek>(reader: &mut R) -> MediaResult<mp4parse::MediaContext> {
  reader.seek(SeekFrom::Start(0)).map_err(MediaError::Io)?;

  mp4parse::read_mp4(reader).map_err(|e| MediaError::Demux(format!("mp4parse failed: {e}")))
}

fn track_codec_and_extradata(track: &mp4parse::Track) -> (MediaCodec, Vec<u8>) {
  // mp4parse exposes a fairly rich codec model, but for now we keep this simple and best-effort.
  //
  // - AAC: detect `mp4a` sample entries.
  // - Other codecs: surface the fourcc/name as `Unknown`.
  //
  // Note: We intentionally leave `codec_private` empty for now. mp4parse can expose codec-specific
  // config via `AudioSampleEntry::codec_specific`, but the exact enum variants are not yet wired
  // into our decode pipeline.
  let mut codec = MediaCodec::Unknown("unknown".to_string());

  if let Some(stsd) = track.stsd.as_ref() {
    if let Some(entry) = stsd.descriptions.get(0) {
      match entry {
        mp4parse::SampleEntry::Audio(audio) => {
          let name = format!("{:?}", audio.codec_type);
          let lower = name.to_ascii_lowercase();
          codec = if lower.contains("mp4a") || lower.contains("aac") {
            MediaCodec::Aac
          } else if lower.contains("opus") {
            MediaCodec::Opus
          } else {
            MediaCodec::Unknown(name)
          };
        }
        mp4parse::SampleEntry::Video(video) => {
          let name = format!("{:?}", video.codec_type);
          let lower = name.to_ascii_lowercase();
          codec = if lower.contains("avc1") || lower.contains("avc3") || lower.contains("h264") {
            MediaCodec::H264
          } else if lower.contains("vp09") || lower.contains("vp9") {
            MediaCodec::Vp9
          } else {
            MediaCodec::Unknown(name)
          };
        }
        _ => {}
      }
    }
  }

  (codec, Vec::new())
}

fn video_dimensions(track: &mp4parse::Track) -> (Option<u16>, Option<u16>) {
  if let Some(stsd) = track.stsd.as_ref() {
    let mut best: Option<(u16, u16, u64)> = None;
    for desc in &stsd.descriptions {
      let mp4parse::SampleEntry::Video(video) = desc else {
        continue;
      };
      let pixels = u64::from(video.width).saturating_mul(u64::from(video.height));
      match best {
        None => best = Some((video.width, video.height, pixels)),
        Some((_, _, best_pixels)) if pixels > best_pixels => {
          best = Some((video.width, video.height, pixels));
        }
        _ => {}
      }
    }

    if let Some((w, h, _)) = best {
      return (Some(w), Some(h));
    }
  }
  (None, None)
}

fn video_pixel_count(track: &mp4parse::Track) -> u64 {
  let (w, h) = video_dimensions(track);
  match (w, h) {
    (Some(w), Some(h)) => u64::from(w).saturating_mul(u64::from(h)),
    _ => 0,
  }
}

fn audio_format(track: &mp4parse::Track) -> (Option<u32>, Option<u16>) {
  if let Some(stsd) = track.stsd.as_ref() {
    for desc in &stsd.descriptions {
      if let mp4parse::SampleEntry::Audio(audio) = desc {
        let sample_rate = if audio.samplerate.is_finite() && audio.samplerate > 0.0 {
          Some(audio.samplerate.round() as u32)
        } else {
          None
        };
        let channels = if audio.channelcount > 0 {
          Some(u16::try_from(audio.channelcount).unwrap_or(u16::MAX))
        } else {
          None
        };
        return (sample_rate, channels);
      }
    }
  }
  (None, None)
}

fn ticks_to_ns(ticks: u64, timescale: u64) -> u64 {
  if ticks == 0 {
    return 0;
  }
  let den = u128::from(timescale);
  if den == 0 {
    return u64::MAX;
  }
  let ns = u128::from(ticks)
    .saturating_mul(1_000_000_000u128)
    .saturating_add(den / 2)
    / den;
  ns.min(u128::from(u64::MAX)) as u64
}

fn build_sample_list(track: &mp4parse::Track) -> MediaResult<Vec<Mp4SampleInfo>> {
  let timescale = track.timescale.map(|t| t.0).unwrap_or(0).max(1);

  let base_offset = mp4parse::unstable::CheckedInteger(0i64);
  let Some(table) = mp4parse::unstable::create_sample_table(track, base_offset) else {
    return Ok(Vec::new());
  };

  build_sample_list_from_table(&table, timescale)
}

fn build_sample_list_from_table(
  table: &[mp4parse::unstable::Indice],
  timescale: u64,
) -> MediaResult<Vec<Mp4SampleInfo>> {
  // MP4 `ctts` offsets can be negative (version 1), producing negative composition times (PTS).
  // `MediaPacket` stores timestamps as `u64` nanoseconds, so preserve distinct negative timestamps
  // by shifting the entire track so the minimum PTS becomes 0.
  let mut min_pts_ticks: i64 = i64::MAX;
  for s in table.iter() {
    min_pts_ticks = min_pts_ticks.min(s.start_composition.0);
  }
  if min_pts_ticks == i64::MAX {
    min_pts_ticks = 0;
  }
  let pts_offset_ticks: i128 = if min_pts_ticks < 0 {
    -(min_pts_ticks as i128)
  } else {
    0
  };

  let mut samples = Vec::with_capacity(table.len());
  for s in table.iter() {
    let offset = s.start_offset.0;
    let size_u64 = s.end_offset.0.saturating_sub(s.start_offset.0);
    let size =
      u32::try_from(size_u64).map_err(|_| MediaError::Demux("sample too large".to_string()))?;

    let dts_ticks = non_negative_i64_to_u64(s.start_decode.0);
    let pts_ticks = shift_ticks_to_non_negative_u64(s.start_composition.0, pts_offset_ticks);
    let duration_ticks_i64 = s.end_composition.0.saturating_sub(s.start_composition.0);
    let duration_ticks = non_negative_i64_to_u64(duration_ticks_i64);

    samples.push(Mp4SampleInfo {
      offset,
      size,
      dts_ns: ticks_to_ns(dts_ticks, timescale),
      pts_ns: ticks_to_ns(pts_ticks, timescale),
      duration_ns: ticks_to_ns(duration_ticks, timescale),
      is_sync: s.sync,
    });
  }

  Ok(samples)
}

fn non_negative_i64_to_u64(v: i64) -> u64 {
  if v <= 0 {
    0
  } else {
    v as u64
  }
}

fn shift_ticks_to_non_negative_u64(v: i64, offset: i128) -> u64 {
  let shifted = (v as i128).saturating_add(offset);
  if shifted <= 0 {
    0
  } else if shifted > u64::MAX as i128 {
    u64::MAX
  } else {
    shifted as u64
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn mp4_selection_ignores_auxiliary_and_picks_highest_res_video() {
    let policy = TrackSelectionPolicy::default();
    let tracks = vec![
      Mp4TrackSelectionInfo {
        id: 1,
        kind: Mp4TrackKind::AuxiliaryVideo,
        enabled: true,
        pixel_count: 4_000_000,
      },
      Mp4TrackSelectionInfo {
        id: 2,
        kind: Mp4TrackKind::Video,
        enabled: true,
        pixel_count: 640 * 360,
      },
      Mp4TrackSelectionInfo {
        id: 3,
        kind: Mp4TrackKind::Video,
        enabled: true,
        pixel_count: 1920 * 1080,
      },
    ];

    let (video, audio) = select_primary_track_ids(&tracks, policy);
    assert_eq!(video, Some(3));
    assert_eq!(audio, None);
  }

  #[test]
  fn mp4_selection_prefers_enabled_tracks() {
    let policy = TrackSelectionPolicy::default();
    let tracks = vec![
      Mp4TrackSelectionInfo {
        id: 1,
        kind: Mp4TrackKind::Audio,
        enabled: false,
        pixel_count: 0,
      },
      Mp4TrackSelectionInfo {
        id: 2,
        kind: Mp4TrackKind::Audio,
        enabled: true,
        pixel_count: 0,
      },
    ];

    let (_, audio) = select_primary_track_ids(&tracks, policy);
    assert_eq!(audio, Some(2));
  }

  #[test]
  fn mp4_selection_tie_breaks_audio_to_first_track() {
    let policy = TrackSelectionPolicy::default();
    let tracks = vec![
      Mp4TrackSelectionInfo {
        id: 10,
        kind: Mp4TrackKind::Audio,
        enabled: true,
        pixel_count: 0,
      },
      Mp4TrackSelectionInfo {
        id: 11,
        kind: Mp4TrackKind::Audio,
        enabled: true,
        pixel_count: 0,
      },
    ];

    let (_, audio) = select_primary_track_ids(&tracks, policy);
    assert_eq!(audio, Some(10));
  }

  #[test]
  fn mp4parse_build_sample_list_normalizes_negative_composition_ticks() {
    use mp4parse::unstable::CheckedInteger;

    let table = vec![
      mp4parse::unstable::Indice {
        start_offset: CheckedInteger(0u64),
        end_offset: CheckedInteger(1u64),
        start_decode: CheckedInteger(0i64),
        start_composition: CheckedInteger(-1i64),
        end_composition: CheckedInteger(0i64),
        sync: true,
        ..Default::default()
      },
      mp4parse::unstable::Indice {
        start_offset: CheckedInteger(1u64),
        end_offset: CheckedInteger(2u64),
        start_decode: CheckedInteger(1i64),
        start_composition: CheckedInteger(0i64),
        end_composition: CheckedInteger(1i64),
        sync: true,
        ..Default::default()
      },
    ];

    let samples = build_sample_list_from_table(&table, 1).expect("sample list");
    assert_eq!(samples.len(), 2);
    assert_eq!(samples[0].pts_ns, 0);
    assert_eq!(samples[1].pts_ns, 1_000_000_000);
  }
}
