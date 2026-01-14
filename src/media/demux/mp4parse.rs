use crate::media::track_selection::{
  select_primary_audio_track_id, select_primary_video_track_id, TrackCandidate, TrackFilterMode,
  TrackSelectionPolicy,
};
use crate::media::{
  MediaAudioInfo, MediaCodec, MediaError, MediaPacket, MediaResult, MediaTrackInfo, MediaTrackType,
  MediaVideoInfo,
};
use std::io::{Read, Seek, SeekFrom};

const MAX_MP4_SAMPLE_BYTES: usize = 64 * 1024 * 1024;
const MAX_MP4_SAMPLES_PER_TRACK: usize = 2_000_000;
const MAX_MP4_TOTAL_SAMPLES: usize = 4_000_000;

fn mp4_sample_too_large_error(track_id: u32, len: usize) -> MediaError {
  MediaError::Demux(format!(
    "MP4 sample too large (track {track_id}, size {len} bytes, cap {MAX_MP4_SAMPLE_BYTES} bytes)"
  ))
}

fn check_mp4_sample_size(track_id: u32, len: usize) -> MediaResult<()> {
  if len > MAX_MP4_SAMPLE_BYTES {
    return Err(mp4_sample_too_large_error(track_id, len));
  }
  Ok(())
}

fn mp4_track_too_many_samples_error(track_id: u32, sample_count: usize) -> MediaError {
  MediaError::Demux(format!(
    "MP4 track has too many samples (track {track_id}, sample_count {sample_count}, cap {MAX_MP4_SAMPLES_PER_TRACK})"
  ))
}

fn mp4_too_many_samples_total_error(total_samples: usize) -> MediaError {
  MediaError::Demux(format!(
    "MP4 has too many total samples (total {total_samples}, cap {MAX_MP4_TOTAL_SAMPLES})"
  ))
}

fn check_mp4_track_sample_count(track_id: u32, sample_count: usize) -> MediaResult<()> {
  if sample_count > MAX_MP4_SAMPLES_PER_TRACK {
    return Err(mp4_track_too_many_samples_error(track_id, sample_count));
  }
  Ok(())
}

fn mp4parse_track_sample_count(track: &mp4parse::Track) -> Option<usize> {
  // mp4parse's SampleSizeBox does not expose the `sample_count` field directly; use the
  // `sample_sizes` vector length when present, otherwise fall back to summing stts.
  if let Some(stsz) = track.stsz.as_ref() {
    if stsz.sample_size == 0 && !stsz.sample_sizes.is_empty() {
      return Some(stsz.sample_sizes.len());
    }
  }

  track.stts.as_ref().and_then(|stts| {
    let total: u64 = stts.samples.iter().map(|e| u64::from(e.sample_count)).sum();
    usize::try_from(total).ok()
  })
}

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

#[derive(Debug, Clone)]
enum PtsIndex {
  Monotonic,
  Sorted {
    sample_indices_by_pts: Vec<u32>,
    /// Suffix minima of decode-order indices for fast "first decode sample with pts>=t" seeking.
    ///
    /// `min_sample_index_from_pos[i]` is the smallest decode-order sample index among
    /// `sample_indices_by_pts[i..]`.
    min_sample_index_from_pos: Vec<u32>,
  },
}

#[derive(Debug)]
struct ActiveTrack {
  id: u32,
  samples: Vec<Mp4SampleInfo>,
  pts_index: PtsIndex,
  next_sample: usize,
}

impl ActiveTrack {
  fn seek(&mut self, time_ns: u64) {
    let idx = match &self.pts_index {
      PtsIndex::Monotonic => self.samples.partition_point(|s| s.pts_ns < time_ns),
      PtsIndex::Sorted {
        sample_indices_by_pts,
        min_sample_index_from_pos,
      } => {
        let pos =
          sample_indices_by_pts.partition_point(|&i| self.samples[i as usize].pts_ns < time_ns);
        min_sample_index_from_pos
          .get(pos)
          .map(|&idx| idx as usize)
          .unwrap_or_else(|| self.samples.len())
      }
    };
    self.next_sample = idx;
  }
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
    reject_encrypted_tracks(&ctx)?;

    let mut selection_infos = Vec::new();
    let mut tracks = Vec::new();

    for track in &ctx.tracks {
      let Some(id) = track.track_id else {
        // mp4parse 0.17 represents missing `tkhd.track_id` as `None`.
        // Skip these tracks rather than panicking or synthesizing IDs.
        continue;
      };
      let kind = mp4_track_kind(&track.track_type);

      let (codec, codec_private) = track_codec_and_extradata(track);

      // Extract a conservative enabled flag. If mp4parse didn't parse tkhd, assume enabled.
      let enabled = track.tkhd.as_ref().map(|t| !t.disabled).unwrap_or(true);

      // We currently only surface tracks that we can actually decode in-process.
      let (media_track_type, video, audio, pixel_count) = match kind {
        Mp4TrackKind::Video if matches!(codec, MediaCodec::H264 | MediaCodec::Vp9) => {
          let (width, height) = video_dimensions(track);
          let pixel_count = match (width, height) {
            (Some(w), Some(h)) => u64::from(w).saturating_mul(u64::from(h)),
            _ => 0,
          };
          let video = width.zip(height).map(|(w, h)| MediaVideoInfo {
            width: u32::from(w),
            height: u32::from(h),
          });
          (MediaTrackType::Video, video, None, pixel_count)
        }
        Mp4TrackKind::Audio if matches!(codec, MediaCodec::Aac) => {
          let (sample_rate, channels) = audio_format(track);
          let audio = match (sample_rate, channels) {
            (Some(sr), Some(ch)) => Some(MediaAudioInfo {
              sample_rate: sr,
              channels: ch,
            }),
            _ => None,
          };
          (MediaTrackType::Audio, None, audio, 0)
        }
        _ => continue,
      };

      selection_infos.push(Mp4TrackSelectionInfo {
        id,
        kind,
        enabled,
        pixel_count,
      });

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
    let mut total_samples = 0_usize;
    for id in active_track_ids {
      let Some(track) = ctx.tracks.iter().find(|t| t.track_id == Some(id)) else {
        continue;
      };

      if let Some(sample_count) = mp4parse_track_sample_count(track) {
        check_mp4_track_sample_count(id, sample_count)?;
        total_samples = total_samples.saturating_add(sample_count);
        if total_samples > MAX_MP4_TOTAL_SAMPLES {
          return Err(mp4_too_many_samples_total_error(total_samples));
        }
      }

      let samples = build_sample_list(track)?;
      let pts_index = build_pts_index(&samples);
      active_tracks.push(ActiveTrack {
        id,
        samples,
        pts_index,
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
    let Some(sample) = track.samples.get(track.next_sample) else {
      debug_assert!(false, "mp4parse: selected track index should have a next sample");
      return Err(MediaError::Demux("mp4parse: missing sample for selected track".to_string()));
    };
    track.next_sample += 1;

    let size_usize = usize::try_from(sample.size)
      .map_err(|_| MediaError::Demux("sample size overflows usize".to_string()))?;
    check_mp4_sample_size(track.id, size_usize)?;
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

  pub fn seek(&mut self, time_ns: u64) -> MediaResult<()> {
    for track in &mut self.active_tracks {
      track.seek(time_ns);
    }
    Ok(())
  }
}

fn build_pts_index(samples: &[Mp4SampleInfo]) -> PtsIndex {
  let mut is_monotonic = true;
  for i in 1..samples.len() {
    if samples[i].pts_ns < samples[i - 1].pts_ns {
      is_monotonic = false;
      break;
    }
  }

  if is_monotonic {
    return PtsIndex::Monotonic;
  }

  let mut sample_indices_by_pts = Vec::with_capacity(samples.len());
  for i in 0..samples.len() {
    sample_indices_by_pts.push(i as u32);
  }
  sample_indices_by_pts.sort_unstable_by_key(|&i| (samples[i as usize].pts_ns, i));

  let mut min_sample_index_from_pos = vec![0_u32; sample_indices_by_pts.len()];
  let mut min = u32::MAX;
  for (pos, &idx) in sample_indices_by_pts.iter().enumerate().rev() {
    min = min.min(idx);
    min_sample_index_from_pos[pos] = min;
  }

  PtsIndex::Sorted {
    sample_indices_by_pts,
    min_sample_index_from_pos,
  }
}

fn read_mp4_context<R: Read + Seek>(reader: &mut R) -> MediaResult<mp4parse::MediaContext> {
  reader.seek(SeekFrom::Start(0)).map_err(MediaError::Io)?;

  mp4parse::read_mp4(reader).map_err(|e| MediaError::Demux(format!("mp4parse failed: {e}")))
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Mp4ProtectionInfo {
  scheme_type: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Mp4SampleEntryMeta {
  codec_type: mp4parse::CodecType,
  protection_info: Vec<Mp4ProtectionInfo>,
}

impl Mp4SampleEntryMeta {
  fn from_mp4parse(sample_entry: &mp4parse::SampleEntry) -> Option<Self> {
    match sample_entry {
      mp4parse::SampleEntry::Video(entry) => Some(Self {
        codec_type: entry.codec_type,
        protection_info: entry
          .protection_info
          .iter()
          .map(|info| Mp4ProtectionInfo {
            scheme_type: info
              .scheme_type
              .as_ref()
              .map(|scheme| scheme.scheme_type.to_string()),
          })
          .collect(),
      }),
      mp4parse::SampleEntry::Audio(entry) => Some(Self {
        codec_type: entry.codec_type,
        protection_info: entry
          .protection_info
          .iter()
          .map(|info| Mp4ProtectionInfo {
            scheme_type: info
              .scheme_type
              .as_ref()
              .map(|scheme| scheme.scheme_type.to_string()),
          })
          .collect(),
      }),
      _ => None,
    }
  }
}

fn reject_encrypted_sample_entry(meta: &Mp4SampleEntryMeta) -> MediaResult<()> {
  let has_sinf = !meta.protection_info.is_empty();
  let codec_name = format!("{:?}", meta.codec_type).to_ascii_lowercase();
  let encrypted_codec = matches!(
    meta.codec_type,
    mp4parse::CodecType::EncryptedVideo | mp4parse::CodecType::EncryptedAudio
  );
    || codec_name.contains("encv")
    || codec_name.contains("enca");

  if !has_sinf && !encrypted_codec {
    return Ok(());
  }

  let mut schemes: Vec<String> = meta
    .protection_info
    .iter()
    .filter_map(|info| info.scheme_type.clone())
    .filter(|s| !s.is_empty())
    .collect();
  schemes.sort();
  schemes.dedup();

  let msg = if schemes.is_empty() {
    "encrypted".to_string()
  } else if schemes.len() == 1 {
    format!("encrypted (scheme={})", schemes[0])
  } else {
    format!("encrypted (schemes={})", schemes.join("+"))
  };

  Err(MediaError::Unsupported(msg.into()))
}

/// Reject encrypted/protected tracks in an already-parsed `mp4parse::MediaContext`.
pub(crate) fn reject_encrypted_tracks(ctx: &mp4parse::MediaContext) -> MediaResult<()> {
  for track in &ctx.tracks {
    let Some(stsd) = track.stsd.as_ref() else {
      continue;
    };
    for sample_entry in &stsd.descriptions {
      let Some(meta) = Mp4SampleEntryMeta::from_mp4parse(sample_entry) else {
        continue;
      };
      reject_encrypted_sample_entry(&meta)?;
    }
  }
  Ok(())
}

fn track_codec_and_extradata(track: &mp4parse::Track) -> (MediaCodec, Vec<u8>) {
  fn codec_type_name(codec: &mp4parse::CodecType) -> String {
    format!("{codec:?}")
  }

  // mp4parse exposes a fairly rich codec model. Keep codec detection simple but try to surface
  // decoder-relevant codec_private data for codecs we already support.
  let mut codec = MediaCodec::Unknown("unknown".to_string());
  let mut codec_private = Vec::new();
  let Some(stsd) = track.stsd.as_ref() else {
    return (codec, codec_private);
  };

  // MP4 can have multiple sample entries (`stsd`). The active one is selected via the `stsc`
  // (sample-to-chunk) table. Use the first `stsc` entry's description index when available,
  // falling back to the first `stsd` entry.
  let mut desc_index0: usize = 0;
  if let Some(stsc) = track.stsc.as_ref() {
    if let Some(first) = stsc.samples.first() {
      let idx1 = first.sample_description_index;
      if idx1 > 0 {
        desc_index0 = (idx1 - 1) as usize;
      }
    }
  }

  let Some(entry) = stsd
    .descriptions
    .get(desc_index0)
    .or_else(|| stsd.descriptions.get(0))
  else {
    return (codec, codec_private);
  };

  match entry {
    mp4parse::SampleEntry::Audio(audio) => {
      let name = codec_type_name(&audio.codec_type);
      let lower = name.to_ascii_lowercase();
      codec = if lower.contains("mp4a") || lower.contains("aac") {
        MediaCodec::Aac
      } else if lower.contains("opus") {
        MediaCodec::Opus
      } else {
        MediaCodec::Unknown(name)
      };

      if matches!(codec, MediaCodec::Aac) {
        // mp4parse exposes the MP4 ESDS/ASC bytes via `ES_Descriptor.decoder_specific_data`.
        if let mp4parse::AudioCodecSpecific::ES_Descriptor(esds) = &audio.codec_specific {
          codec_private = esds.decoder_specific_data.iter().copied().collect();
        }
      }
    }
    mp4parse::SampleEntry::Video(video) => {
      let name = codec_type_name(&video.codec_type);
      let lower = name.to_ascii_lowercase();
      codec = if lower.contains("avc1") || lower.contains("avc3") || lower.contains("h264") {
        MediaCodec::H264
      } else if lower.contains("vp09") || lower.contains("vp9") {
        MediaCodec::Vp9
      } else {
        MediaCodec::Unknown(name)
      };

      if matches!(codec, MediaCodec::H264) {
        // mp4parse provides raw avcC bytes (`AVCDecoderConfigurationRecord`) via
        // `VideoCodecSpecific::AVCConfig`. Convert to the small custom format expected by
        // `decoder::H264Decoder`.
        if let mp4parse::VideoCodecSpecific::AVCConfig(avcc) = &video.codec_specific {
          if let Some(out) = parse_avcc_for_h264_codec_private(&avcc[..]) {
            codec_private = out;
          }
        }
      } else if matches!(codec, MediaCodec::Vp9) {
        // Mirror the compact vpcC-derived extradata format used by `Mp4PacketDemuxer`.
        if let mp4parse::VideoCodecSpecific::VPxConfig(vpcc) = &video.codec_specific {
          let codec_init: Vec<u8> = vpcc.codec_init.iter().copied().collect();
          if codec_init.len() <= u16::MAX as usize {
            let mut out = Vec::with_capacity(3 + 2 + codec_init.len());
            out.push(vpcc.bit_depth);
            out.push(vpcc.colour_primaries);
            out.push(vpcc.chroma_subsampling);
            out.extend_from_slice(&(codec_init.len() as u16).to_be_bytes());
            out.extend_from_slice(&codec_init);
            codec_private = out;
          }
        }
      }
    }
    _ => {}
  }

  (codec, codec_private)
}

fn parse_avcc_for_h264_codec_private(avcc: &[u8]) -> Option<Vec<u8>> {
  // AVCDecoderConfigurationRecord (avcC) layout (ISO/IEC 14496-15):
  //
  // 1  configurationVersion (must be 1)
  // 1  AVCProfileIndication
  // 1  profile_compatibility
  // 1  AVCLevelIndication
  // 1  reserved (6 bits = 1) + lengthSizeMinusOne (2 bits)
  // 1  reserved (3 bits = 1) + numOfSequenceParameterSets (5 bits)
  //   [SPS...]
  // 1  numOfPictureParameterSets
  //   [PPS...]
  //
  // We convert this to the small custom format expected by `decoder::H264Decoder`:
  //
  //   u8  nal_length_size
  //   u8  sps_count
  //   [sps_count] { u16be len, [len] bytes }
  //   u8  pps_count
  //   [pps_count] { u16be len, [len] bytes }
  if avcc.len() < 7 {
    return None;
  }
  let mut i = 0usize;

  let configuration_version = avcc[i];
  i += 1;
  if configuration_version != 1 {
    return None;
  }

  // Skip profile/compat/level.
  i += 3;

  let length_size_minus_one = avcc[i] & 0b11;
  i += 1;
  let nal_length_size = length_size_minus_one + 1;
  if nal_length_size == 0 || nal_length_size > 4 {
    return None;
  }

  let num_sps = avcc[i] & 0b1_1111;
  i += 1;
  let sps_count = num_sps as usize;

  let mut out = Vec::new();
  out.push(nal_length_size);
  out.push(num_sps);

  for _ in 0..sps_count {
    if i + 2 > avcc.len() {
      return None;
    }
    let len = u16::from_be_bytes([avcc[i], avcc[i + 1]]) as usize;
    i += 2;
    let end = i.checked_add(len)?;
    if end > avcc.len() {
      return None;
    }
    out.extend_from_slice(&(len as u16).to_be_bytes());
    out.extend_from_slice(&avcc[i..end]);
    i = end;
  }

  if i >= avcc.len() {
    return None;
  }
  let num_pps = avcc[i];
  i += 1;
  let pps_count = num_pps as usize;

  out.push(num_pps);
  for _ in 0..pps_count {
    if i + 2 > avcc.len() {
      return None;
    }
    let len = u16::from_be_bytes([avcc[i], avcc[i + 1]]) as usize;
    i += 2;
    let end = i.checked_add(len)?;
    if end > avcc.len() {
      return None;
    }
    out.extend_from_slice(&(len as u16).to_be_bytes());
    out.extend_from_slice(&avcc[i..end]);
    i = end;
  }

  Some(out)
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
  fn rejects_oversized_mp4_sample() {
    check_mp4_sample_size(7, MAX_MP4_SAMPLE_BYTES).expect("cap-sized sample should be allowed");

    let len = MAX_MP4_SAMPLE_BYTES + 1;
    let err = check_mp4_sample_size(7, len).expect_err("expected sample cap error");
    let MediaError::Demux(msg) = err else {
      panic!("expected demux error, got {err:?}");
    };
    assert!(
      msg.contains("track 7"),
      "expected error mentioning track id, got {msg:?}"
    );
    assert!(
      msg.contains(&format!("size {len} bytes")),
      "expected error mentioning size, got {msg:?}"
    );
    assert!(
      msg.contains(&format!("cap {MAX_MP4_SAMPLE_BYTES} bytes")),
      "expected error mentioning cap, got {msg:?}"
    );
  }

  #[test]
  fn rejects_mp4_track_with_too_many_samples() {
    check_mp4_track_sample_count(7, MAX_MP4_SAMPLES_PER_TRACK)
      .expect("cap-sized sample_count should be allowed");

    let sample_count = MAX_MP4_SAMPLES_PER_TRACK + 1;
    let err = check_mp4_track_sample_count(7, sample_count).expect_err("expected sample_count error");
    let MediaError::Demux(msg) = err else {
      panic!("expected demux error, got {err:?}");
    };
    assert!(
      msg.contains("track 7"),
      "expected error mentioning track id, got {msg:?}"
    );
    assert!(
      msg.contains(&format!("sample_count {sample_count}")),
      "expected error mentioning sample_count, got {msg:?}"
    );
    assert!(
      msg.contains(&format!("cap {MAX_MP4_SAMPLES_PER_TRACK}")),
      "expected error mentioning cap, got {msg:?}"
    );
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

  #[test]
  fn mp4parse_seek_uses_decode_order_when_pts_non_monotonic() {
    // Non-monotonic PTS in decode order (typical B-frame reordering):
    // pts: 0, 3, 1, 2
    // Seeking to pts>=2 should return sample 1 (pts=3), not sample 3 (pts=2).
    let samples = vec![
      Mp4SampleInfo {
        offset: 0,
        size: 1,
        dts_ns: 0,
        pts_ns: 0,
        duration_ns: 1,
        is_sync: true,
      },
      Mp4SampleInfo {
        offset: 1,
        size: 1,
        dts_ns: 1,
        pts_ns: 3,
        duration_ns: 1,
        is_sync: false,
      },
      Mp4SampleInfo {
        offset: 2,
        size: 1,
        dts_ns: 2,
        pts_ns: 1,
        duration_ns: 1,
        is_sync: false,
      },
      Mp4SampleInfo {
        offset: 3,
        size: 1,
        dts_ns: 3,
        pts_ns: 2,
        duration_ns: 1,
        is_sync: false,
      },
    ];

    let pts_index = build_pts_index(&samples);
    let mut track = ActiveTrack {
      id: 1,
      samples,
      pts_index,
      next_sample: 0,
    };

    track.seek(2);
    assert_eq!(track.next_sample, 1);
  }

  #[test]
  fn rejects_sample_entry_with_protection_info_and_scheme() {
    let meta = Mp4SampleEntryMeta {
      codec_type: mp4parse::CodecType::H264,
      protection_info: vec![Mp4ProtectionInfo {
        scheme_type: Some("cenc".to_string()),
      }],
    };

    let err = reject_encrypted_sample_entry(&meta).expect_err("expected unsupported error");
    let MediaError::Unsupported(msg) = err else {
      panic!("expected MediaError::Unsupported, got {err:?}");
    };
    assert!(
      msg.contains("encrypted"),
      "expected error message to mention encryption, got {msg}"
    );
    assert!(
      msg.contains("cenc"),
      "expected error message to include scheme, got {msg}"
    );
  }

  #[test]
  fn rejects_sample_entry_with_encrypted_codec_type() {
    let meta = Mp4SampleEntryMeta {
      codec_type: mp4parse::CodecType::EncryptedVideo,
      protection_info: Vec::new(),
    };
    let err = reject_encrypted_sample_entry(&meta).expect_err("expected unsupported error");
    let MediaError::Unsupported(msg) = err else {
      panic!("expected MediaError::Unsupported, got {err:?}");
    };
    assert!(msg.contains("encrypted"));
  }

  #[test]
  fn accepts_unencrypted_sample_entry() {
    let meta = Mp4SampleEntryMeta {
      codec_type: mp4parse::CodecType::H264,
      protection_info: Vec::new(),
    };
    reject_encrypted_sample_entry(&meta).expect("should be accepted");
  }

  #[test]
  fn mp4_fixture_is_not_rejected() {
    let bytes = include_bytes!("../../../tests/fixtures/media/test_h264_aac.mp4");
    let mut cursor = std::io::Cursor::new(bytes.as_slice());
    let ctx = mp4parse::read_mp4(&mut cursor).expect("fixture should parse");
    reject_encrypted_tracks(&ctx).expect("fixture should not be treated as encrypted");
  }

  #[test]
  fn mp4parse_demuxer_opens_fixture_and_exposes_nonzero_track_ids() {
    use std::fs::File;
    use std::io::BufReader;
    use std::path::PathBuf;

    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
      .join("tests")
      .join("fixtures")
      .join("media")
      .join("test_h264_aac.mp4");

    let file = File::open(&path).expect("fixture should exist");
    let reader = BufReader::new(file);
    let demuxer = Mp4ParseDemuxer::open(reader).expect("mp4parse demuxer should open fixture");
    let tracks = demuxer.tracks();

    assert!(
      tracks.iter().all(|t| t.id > 0),
      "expected all track ids to be non-zero: {tracks:?}"
    );

    assert!(
      tracks
        .iter()
        .any(|t| t.track_type == MediaTrackType::Video && t.codec == MediaCodec::H264),
      "expected at least one H264 video track: {tracks:?}"
    );
    assert!(
      tracks
        .iter()
        .any(|t| t.track_type == MediaTrackType::Audio && t.codec == MediaCodec::Aac),
      "expected at least one AAC audio track: {tracks:?}"
    );
  }
}
