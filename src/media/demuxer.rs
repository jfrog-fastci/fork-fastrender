use super::{
  MediaAudioInfo, MediaCodec, MediaError, MediaPacket, MediaResult, MediaTrackInfo, MediaTrackType,
  MediaVideoInfo,
};
#[cfg(feature = "media_mp4")]
use std::collections::HashMap;
#[cfg(feature = "media_mp4")]
use std::fs::File;
#[cfg(feature = "media_mp4")]
use std::io::BufReader;
use std::io::{Read, Seek};
#[cfg(feature = "media_mp4")]
use std::path::Path;

/// A container demuxer that yields compressed packets in demux order.
pub trait MediaDemuxer: Send {
  fn tracks(&self) -> &[MediaTrackInfo];
  fn next_packet(&mut self) -> MediaResult<Option<MediaPacket>>;
  fn seek(&mut self, time_ns: u64) -> MediaResult<()>;
}

impl<R: Read + Seek + Send> MediaDemuxer for super::demux::webm::WebmDemuxer<R> {
  fn tracks(&self) -> &[MediaTrackInfo] {
    super::demux::webm::WebmDemuxer::tracks(self)
  }

  fn next_packet(&mut self) -> MediaResult<Option<MediaPacket>> {
    super::demux::webm::WebmDemuxer::next_packet(self)
  }

  fn seek(&mut self, time_ns: u64) -> MediaResult<()> {
    super::demux::webm::WebmDemuxer::seek(self, time_ns)
  }
}

// ============================================================================
// MP4 packet demuxer (mp4 crate)
// ============================================================================

#[cfg(feature = "media_mp4")]
struct Mp4TrackCursor {
  id: u32,
  timescale: u32,
  sample_count: u32,
  next_sample: u32,
  peeked: Option<MediaPacket>,
  track_type: MediaTrackType,
}

/// Simple MP4 packet demuxer that yields common video+audio packets in demux order.
///
/// Note: the existing `crate::media::mp4` module focuses on sample tables and efficient seeking,
/// whereas this type focuses on producing compressed packets with codec metadata for decoding.
#[cfg(feature = "media_mp4")]
pub struct Mp4PacketDemuxer<R: Read + Seek + Send> {
  mp4: mp4::Mp4Reader<R>,
  tracks: Vec<MediaTrackInfo>,
  cursors: Vec<Mp4TrackCursor>,
  sample_tables: HashMap<u32, TrackSampleTable>,
}

#[cfg(feature = "media_mp4")]
impl Mp4PacketDemuxer<BufReader<File>> {
  pub fn open(path: impl AsRef<Path>) -> MediaResult<Self> {
    let mut file = File::open(path.as_ref())?;
    let len = file.metadata()?.len();

    let mp4parse_meta = {
      file.rewind()?;
      let meta = match mp4parse_read_meta(&mut file) {
        Ok(meta) => meta,
        // Fail fast for DRM/CENC tracks so we don't proceed to an opaque decode failure later.
        Err(err @ MediaError::Unsupported(_)) => return Err(err),
        // `mp4parse` is used here as a best-effort metadata supplement. If it fails to parse this
        // MP4, we can still proceed using the `mp4` crate (just with less codec introspection).
        Err(_) => Mp4ParseMeta::default(),
      };
      file.rewind()?;
      meta
    };

    file.rewind()?;
    let reader = BufReader::new(file);
    let mp4 = mp4::Mp4Reader::read_header(reader, len)
      .map_err(|e| MediaError::Demux(format!("mp4: failed to read header: {e}")))?;

    Self::from_reader_with_meta(mp4, mp4parse_meta)
  }
}

#[cfg(feature = "media_mp4")]
impl Mp4PacketDemuxer<std::io::Cursor<std::sync::Arc<[u8]>>> {
  pub fn from_bytes(bytes: std::sync::Arc<[u8]>) -> MediaResult<Self> {
    let mut cursor = std::io::Cursor::new(std::sync::Arc::clone(&bytes));
    let mp4parse_meta = match mp4parse_read_meta(&mut cursor) {
      Ok(meta) => meta,
      Err(err @ MediaError::Unsupported(_)) => return Err(err),
      Err(_) => Mp4ParseMeta::default(),
    };
    cursor.set_position(0);

    let len = cursor.get_ref().len() as u64;
    let mp4 = mp4::Mp4Reader::read_header(cursor, len)
      .map_err(|e| MediaError::Demux(format!("mp4: failed to read header: {e}")))?;

    Mp4PacketDemuxer::from_reader_with_meta(mp4, mp4parse_meta)
  }
}

#[cfg(feature = "media_mp4")]
impl<R: Read + Seek + Send> Mp4PacketDemuxer<R> {
  pub fn from_reader(mp4: mp4::Mp4Reader<R>) -> MediaResult<Self> {
    Self::from_reader_with_meta(mp4, Mp4ParseMeta::default())
  }

  fn from_reader_with_meta(mp4: mp4::Mp4Reader<R>, mp4parse_meta: Mp4ParseMeta) -> MediaResult<Self> {
    let Mp4ParseMeta {
      vp9_tracks,
      sample_tables: mut meta_sample_tables,
      aac_asc,
    } = mp4parse_meta;

    let mut tracks = Vec::new();
    let mut cursors = Vec::new();
    let mut sample_tables: HashMap<u32, TrackSampleTable> = HashMap::new();

    for (track_id, track) in mp4.tracks().iter() {
      let timescale = track.trak.mdia.mdhd.timescale;
      let sample_count = track.sample_count();

      let media_type = match track.media_type() {
        Ok(media_type) => Some(media_type),
        Err(err) => {
          // The `mp4` crate doesn't currently expose VP9 via `media_type()`. If mp4parse already
          // identified this track as VP9, ignore the error and let the mp4parse path handle it.
          // Otherwise, treat this as a real demux failure (we don't want to silently drop H.264/AAC
          // tracks if `media_type()` breaks for valid files).
          if vp9_tracks.contains_key(track_id) {
            None
          } else {
            return Err(MediaError::Demux(format!("mp4: failed to get media type: {err}")));
          }
        }
      };

      match media_type {
        Some(mp4::MediaType::H264) => {
          let width = track.width() as u32;
          let height = track.height() as u32;

          let avcc = track
            .trak
            .mdia
            .minf
            .stbl
            .stsd
            .avc1
            .as_ref()
            .ok_or(MediaError::Demux(
              "mp4: H264 track missing avc1 sample entry".into(),
            ))?
            .avcc
            .clone();

          let codec_private = {
            let mut out = Vec::new();
            let nal_length_size = avcc.length_size_minus_one + 1;
            if nal_length_size == 0 || nal_length_size > 4 {
              return Err(MediaError::Decode(format!(
                "mp4: invalid avcC nal length size: {nal_length_size}"
              )));
            }
            out.push(nal_length_size);

            let sps_count = avcc.sequence_parameter_sets.len();
            if sps_count > u8::MAX as usize {
              return Err(MediaError::Decode("mp4: too many SPS entries".into()));
            }
            out.push(sps_count as u8);
            for sps in &avcc.sequence_parameter_sets {
              let bytes = &sps.bytes;
              if bytes.len() > u16::MAX as usize {
                return Err(MediaError::Decode("mp4: SPS too large".into()));
              }
              out.extend_from_slice(&(bytes.len() as u16).to_be_bytes());
              out.extend_from_slice(bytes);
            }

            let pps_count = avcc.picture_parameter_sets.len();
            if pps_count > u8::MAX as usize {
              return Err(MediaError::Decode("mp4: too many PPS entries".into()));
            }
            out.push(pps_count as u8);
            for pps in &avcc.picture_parameter_sets {
              let bytes = &pps.bytes;
              if bytes.len() > u16::MAX as usize {
                return Err(MediaError::Decode("mp4: PPS too large".into()));
              }
              out.extend_from_slice(&(bytes.len() as u16).to_be_bytes());
              out.extend_from_slice(bytes);
            }

            out
          };

          tracks.push(MediaTrackInfo {
            id: u64::from(*track_id),
            track_type: MediaTrackType::Video,
            codec: MediaCodec::H264,
            codec_private,
            codec_delay_ns: 0,
            video: Some(MediaVideoInfo { width, height }),
            audio: None,
          });
        }
        Some(mp4::MediaType::AAC) => {
          let (sample_rate, channels) = mp4_track_audio_params(track)?;
          let asc = build_aac_lc_audio_specific_config(sample_rate, channels)?;

          tracks.push(MediaTrackInfo {
            id: u64::from(*track_id),
            track_type: MediaTrackType::Audio,
            codec: MediaCodec::Aac,
            codec_private: aac_asc.get(track_id).cloned().unwrap_or(asc),
            codec_delay_ns: 0,
            video: None,
            audio: Some(MediaAudioInfo {
              sample_rate,
              channels,
            }),
          });
        }
        _ => {
          if let Some(vp9) = vp9_tracks.get(track_id) {
            tracks.push(MediaTrackInfo {
              id: u64::from(*track_id),
              track_type: MediaTrackType::Video,
              codec: MediaCodec::Vp9,
              codec_private: vp9.codec_private.clone(),
              codec_delay_ns: 0,
              video: Some(MediaVideoInfo {
                width: vp9.width,
                height: vp9.height,
              }),
              audio: None,
            });
          }
        }
      }

      let emit_packets = matches!(media_type, Some(mp4::MediaType::H264 | mp4::MediaType::AAC))
        || vp9_tracks.contains_key(track_id);
      if emit_packets {
        if let Some(table) = meta_sample_tables.remove(track_id) {
          sample_tables.insert(*track_id, table);
        }

        cursors.push(Mp4TrackCursor {
          id: *track_id,
          timescale,
          sample_count,
          next_sample: 1,
          peeked: None,
          track_type: if matches!(media_type, Some(mp4::MediaType::AAC)) {
            MediaTrackType::Audio
          } else {
            MediaTrackType::Video
          },
        });
      }
    }

    tracks.sort_by_key(|t| t.id);
    cursors.sort_by_key(|c| c.id);

    Ok(Self {
      mp4,
      tracks,
      cursors,
      sample_tables,
    })
  }
}

#[cfg(feature = "media_mp4")]
#[derive(Debug, Clone)]
struct Mp4Vp9TrackMeta {
  width: u32,
  height: u32,
  codec_private: Vec<u8>,
}

#[cfg(feature = "media_mp4")]
fn mp4parse_vp9_tracks_from_ctx(
  ctx: &mp4parse::MediaContext,
) -> MediaResult<HashMap<u32, Mp4Vp9TrackMeta>> {
  let mut out = HashMap::new();

  for track in &ctx.tracks {
    let Some(track_id) = track.track_id else {
      continue;
    };
    if track.track_type != mp4parse::TrackType::Video {
      continue;
    }

    let Some(stsd) = track.stsd.as_ref() else {
      continue;
    };
    let Some(stsc) = track.stsc.as_ref() else {
      continue;
    };

    let first_desc_idx = stsc
      .samples
      .first()
      .map(|s| s.sample_description_index)
      .unwrap_or(1);
    if first_desc_idx == 0 {
      continue;
    }

    let sample_entry = stsd
      .descriptions
      .get(first_desc_idx as usize - 1)
      .ok_or_else(|| MediaError::Demux("mp4parse: missing sample entry".into()))?;

    let mp4parse::SampleEntry::Video(video) = sample_entry else {
      continue;
    };

    if video.codec_type != mp4parse::CodecType::VP9 {
      continue;
    }

    let mp4parse::VideoCodecSpecific::VPxConfig(vpcc) = &video.codec_specific else {
      return Err(MediaError::Demux(
        "mp4parse: VP9 sample entry missing VPxConfig (vpcC)".into(),
      ));
    };

    // Store a compact subset of VPxConfig that downstream decoders can read without requiring
    // Matroska-specific fields.
    //
    // Layout:
    //   u8  bit_depth
    //   u8  colour_primaries
    //   u8  chroma_subsampling
    //   u16 codec_init_len (big-endian)
    //   [codec_init_len] codec_init bytes
    let codec_init: Vec<u8> = vpcc.codec_init.iter().copied().collect();
    if codec_init.len() > u16::MAX as usize {
      return Err(MediaError::Demux("mp4parse: vp9 codec_init too large".into()));
    }

    let mut codec_private = Vec::with_capacity(3 + 2 + codec_init.len());
    codec_private.push(vpcc.bit_depth);
    codec_private.push(vpcc.colour_primaries);
    codec_private.push(vpcc.chroma_subsampling);
    codec_private.extend_from_slice(&(codec_init.len() as u16).to_be_bytes());
    codec_private.extend_from_slice(&codec_init);

    out.insert(
      track_id,
      Mp4Vp9TrackMeta {
        width: u32::from(video.width),
        height: u32::from(video.height),
        codec_private,
      },
    );
  }

  Ok(out)
}

#[cfg(feature = "media_mp4")]
#[derive(Debug, Default, Clone)]
struct Mp4ParseMeta {
  vp9_tracks: HashMap<u32, Mp4Vp9TrackMeta>,
  sample_tables: HashMap<u32, TrackSampleTable>,
  aac_asc: HashMap<u32, Vec<u8>>,
}

#[cfg(feature = "media_mp4")]
fn mp4parse_read_meta<R: Read + Seek>(reader: &mut R) -> MediaResult<Mp4ParseMeta> {
  use std::io::SeekFrom;

  reader.seek(SeekFrom::Start(0))?;
  let ctx =
    mp4parse::read_mp4(reader).map_err(|e| MediaError::Demux(format!("mp4parse: {e:?}")))?;
  super::demux::mp4parse::reject_encrypted_tracks(&ctx)?;

  let vp9_tracks = mp4parse_vp9_tracks_from_ctx(&ctx).unwrap_or_default();
  let sample_tables = mp4parse_build_sample_tables(&ctx).unwrap_or_default();
  let aac_asc = mp4parse_extract_aac_asc(&ctx).unwrap_or_default();

  Ok(Mp4ParseMeta {
    vp9_tracks,
    sample_tables,
    aac_asc,
  })
}

#[cfg(feature = "media_mp4")]
#[derive(Debug, Clone, Copy)]
struct SampleTiming {
  dts_ns: u64,
  pts_ns: u64,
  duration_ns: u64,
  is_sync: bool,
}

#[cfg(feature = "media_mp4")]
#[derive(Debug, Clone)]
struct TrackSampleTable {
  samples: Vec<SampleTiming>,
  pts_ns_by_sample: Vec<u64>,
  pts_index: PtsIndex,
  sync_sample_indices: Vec<u32>,
}

#[cfg(feature = "media_mp4")]
impl TrackSampleTable {
  fn sample_index_at_or_after(&self, time_ns: u64) -> usize {
    match &self.pts_index {
      PtsIndex::Monotonic => self.pts_ns_by_sample.partition_point(|&pts| pts < time_ns),
      PtsIndex::Sorted {
        sample_indices_by_pts,
        min_sample_index_from_pos,
      } => {
        let pos =
          sample_indices_by_pts.partition_point(|&i| self.pts_ns_by_sample[i as usize] < time_ns);
        min_sample_index_from_pos
          .get(pos)
          .map(|&idx| idx as usize)
          .unwrap_or_else(|| self.pts_ns_by_sample.len())
      }
    }
  }

  fn sync_sample_at_or_before(&self, sample_idx0: usize) -> usize {
    if self.sync_sample_indices.is_empty() {
      return sample_idx0;
    }
    let capped = sample_idx0.min(self.samples.len().saturating_sub(1));
    let pos = self
      .sync_sample_indices
      .partition_point(|&idx| idx as usize <= capped);
    if pos == 0 {
      0
    } else {
      self.sync_sample_indices[pos - 1] as usize
    }
  }
}

#[cfg(feature = "media_mp4")]
#[derive(Debug, Clone)]
enum PtsIndex {
  Monotonic,
  Sorted {
    sample_indices_by_pts: Vec<u32>,
    min_sample_index_from_pos: Vec<u32>,
  },
}

#[cfg(feature = "media_mp4")]
fn build_pts_index(pts_ns_by_sample: &[u64]) -> PtsIndex {
  let mut is_monotonic = true;
  for i in 1..pts_ns_by_sample.len() {
    if pts_ns_by_sample[i] < pts_ns_by_sample[i - 1] {
      is_monotonic = false;
      break;
    }
  }
  if is_monotonic {
    return PtsIndex::Monotonic;
  }

  let mut sample_indices_by_pts = Vec::with_capacity(pts_ns_by_sample.len());
  for i in 0..pts_ns_by_sample.len() {
    sample_indices_by_pts.push(i as u32);
  }
  sample_indices_by_pts.sort_unstable_by_key(|&i| (pts_ns_by_sample[i as usize], i));

  let mut min_sample_index_from_pos = vec![0_u32; sample_indices_by_pts.len()];
  let mut min = u32::MAX;
  for (dst, &idx) in min_sample_index_from_pos
    .iter_mut()
    .rev()
    .zip(sample_indices_by_pts.iter().rev())
  {
    min = min.min(idx);
    *dst = min;
  }

  PtsIndex::Sorted {
    sample_indices_by_pts,
    min_sample_index_from_pos,
  }
}

#[cfg(feature = "media_mp4")]
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

#[cfg(feature = "media_mp4")]
fn non_negative_i64_to_u64(v: i64) -> u64 {
  if v <= 0 {
    0
  } else {
    v as u64
  }
}

#[cfg(feature = "media_mp4")]
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

#[cfg(feature = "media_mp4")]
fn mp4parse_build_sample_tables(
  ctx: &mp4parse::MediaContext,
) -> MediaResult<HashMap<u32, TrackSampleTable>> {
  use mp4parse::unstable::{create_sample_table, CheckedInteger};

  // Hard cap so a corrupted MP4 can't make us allocate an unbounded sample table.
  const MAX_SAMPLES_PER_TRACK: usize = 2_000_000;
  const MAX_TOTAL_SAMPLES: usize = 4_000_000;

  let mut total_samples = 0_usize;
  let mut out = HashMap::new();

  for track in &ctx.tracks {
    if !matches!(
      track.track_type,
      mp4parse::TrackType::Video | mp4parse::TrackType::Audio
    ) {
      continue;
    }

    let Some(track_id) = track.track_id else {
      continue;
    };

    let Some(sample_count) = mp4parse_track_sample_count(track) else {
      continue;
    };
    if sample_count > MAX_SAMPLES_PER_TRACK {
      continue;
    }

    let new_total = total_samples.saturating_add(sample_count);
    if new_total > MAX_TOTAL_SAMPLES {
      continue;
    }
    total_samples = new_total;

    let timescale = track.timescale.map(|t| t.0).unwrap_or(0).max(1);
    let base_offset = CheckedInteger(0i64);
    let Some(table) = create_sample_table(track, base_offset) else {
      continue;
    };
    if table.len() != sample_count {
      continue;
    }

    // MP4 timestamps can be negative (e.g. DTS shifted earlier so CTTS offsets are non-negative in
    // version-0 `ctts`). Our pipeline uses unsigned nanosecond timestamps, so shift each timeline
    // independently so the first DTS/PTS starts at 0.
    let mut min_decode_ticks = i64::MAX;
    let mut min_composition_ticks = i64::MAX;
    for s in &table {
      min_decode_ticks = min_decode_ticks.min(s.start_decode.0);
      min_composition_ticks = min_composition_ticks.min(s.start_composition.0);
    }
    let dts_shift = if min_decode_ticks < 0 {
      match min_decode_ticks.checked_neg() {
        Some(v) => v,
        None => continue,
      }
    } else {
      0
    };
    let pts_shift = if min_composition_ticks < 0 {
      match min_composition_ticks.checked_neg() {
        Some(v) => v,
        None => continue,
      }
    } else {
      0
    };

    let mut samples = Vec::with_capacity(table.len());
    let mut pts_ns_by_sample = Vec::with_capacity(table.len());
    let mut sync_sample_indices = Vec::new();

    for (i, s) in table.iter().enumerate() {
      let dts_ticks_i64 = s.start_decode.0.saturating_add(dts_shift);
      let pts_ticks_i64 = s.start_composition.0.saturating_add(pts_shift);
      let duration_ticks_i64 = s.end_composition.0.saturating_sub(s.start_composition.0);

      let dts_ticks = non_negative_i64_to_u64(dts_ticks_i64);
      let pts_ticks = non_negative_i64_to_u64(pts_ticks_i64);
      let duration_ticks = non_negative_i64_to_u64(duration_ticks_i64);

      let dts_ns = ticks_to_ns(dts_ticks, timescale);
      let pts_ns = ticks_to_ns(pts_ticks, timescale);
      let duration_ns = ticks_to_ns(duration_ticks, timescale);

      if s.sync {
        sync_sample_indices.push(i as u32);
      }

      samples.push(SampleTiming {
        dts_ns,
        pts_ns,
        duration_ns,
        is_sync: s.sync,
      });
      pts_ns_by_sample.push(pts_ns);
    }

    let pts_index = build_pts_index(&pts_ns_by_sample);
    out.insert(
      track_id,
      TrackSampleTable {
        samples,
        pts_ns_by_sample,
        pts_index,
        sync_sample_indices,
      },
    );
  }

  Ok(out)
}

#[cfg(feature = "media_mp4")]
fn mp4parse_extract_aac_asc(ctx: &mp4parse::MediaContext) -> MediaResult<HashMap<u32, Vec<u8>>> {
  let mut out = HashMap::new();

  for track in &ctx.tracks {
    if track.track_type != mp4parse::TrackType::Audio {
      continue;
    }
    let Some(track_id) = track.track_id else {
      continue;
    };

    let Some(stsd) = track.stsd.as_ref() else {
      continue;
    };
    let Some(entry) = stsd.descriptions.first() else {
      continue;
    };
    let mp4parse::SampleEntry::Audio(audio) = entry else {
      continue;
    };

    let asc = match &audio.codec_specific {
      mp4parse::AudioCodecSpecific::ES_Descriptor(esds) => {
        esds.decoder_specific_data.iter().copied().collect()
      }
      _ => Vec::new(),
    };

    if !asc.is_empty() {
      out.insert(track_id, asc);
    }
  }

  Ok(out)
}

#[cfg(all(test, feature = "media_mp4", feature = "codec_vp9_libvpx"))]
mod tests {
  use super::MediaDemuxer;
  use super::Mp4PacketDemuxer;
  use crate::media::decoder::create_video_decoder;
  use crate::media::{MediaCodec, MediaTrackType};
  use std::path::PathBuf;

  #[derive(Debug, Clone, Copy)]
  struct RgbaStats {
    avg_r: f64,
    avg_g: f64,
    avg_b: f64,
    avg_a: f64,
    center: [u8; 4],
  }

  fn rgba_stats(pixels: &[u8], width: usize, height: usize) -> RgbaStats {
    assert_eq!(pixels.len(), width * height * 4);

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
  }

  #[test]
  fn mp4_demuxer_detects_vp9_track() {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
      .join("tests")
      .join("fixtures")
      .join("media")
      .join("vp9_in_mp4.mp4");

    let mut demuxer = Mp4PacketDemuxer::open(&path).expect("mp4 demuxer should open");
    let tracks = demuxer.tracks().to_vec();

    let track = tracks
      .iter()
      .find(|t| t.track_type == MediaTrackType::Video && t.codec == MediaCodec::Vp9)
      .expect("expected VP9 video track");

    assert_eq!(track.video.as_ref().map(|v| (v.width, v.height)), Some((16, 16)));
    assert!(
      track.codec_private.len() >= 5,
      "expected VP9 vpcC-derived extradata"
    );

    let mut decoder = create_video_decoder(track).expect("vp9 decoder should be constructible");

    // Decode packets until libvpx yields a frame (some streams may delay output).
    let mut first = None;
    for _ in 0..64 {
      let Some(pkt) = demuxer.next_packet().expect("demux should succeed") else {
        break;
      };
      if pkt.track_id != track.id {
        continue;
      }
      assert!(!pkt.as_slice().is_empty());

      let frames = decoder.decode(&pkt).expect("vp9 decode should succeed");
      if let Some(f) = frames.into_iter().next() {
        first = Some(f);
        break;
      }
    }

    let first = first.expect("expected at least one decoded VP9 frame");
    assert_eq!((first.width, first.height), (16, 16));
    assert_eq!(first.rgba.len(), (first.width * first.height * 4) as usize);
    let stats = rgba_stats(&first.rgba, first.width as usize, first.height as usize);
    assert_mostly_red("mp4/vp9 first frame (vp9_in_mp4.mp4)", stats);
  }
}

#[cfg(all(test, feature = "media_mp4"))]
mod mp4_timestamp_tests {
  use super::MediaDemuxer;
  use super::Mp4PacketDemuxer;
  use crate::media::MediaCodec;
  use std::sync::Arc;

  #[test]
  fn mp4_packets_have_non_zero_duration() {
    let fixture_path = crate::testing::fixture_path("fixtures/media/test_h264_aac.mp4");
    let bytes = std::fs::read(fixture_path).expect("read mp4 fixture");

    let mut demuxer = Mp4PacketDemuxer::from_bytes(Arc::from(bytes)).expect("open demuxer");
    let pkt = demuxer
      .next_packet()
      .expect("next packet")
      .expect("expected packet");
    assert!(
      pkt.duration_ns > 0,
      "expected non-zero duration_ns, got {}",
      pkt.duration_ns
    );
  }

  #[test]
  fn mp4_b_frames_have_non_monotonic_pts_but_monotonic_dts() {
    let fixture_path = crate::testing::fixture_path("fixtures/media/test_h264_b_frames_aac.mp4");
    let bytes = std::fs::read(fixture_path).expect("read mp4 fixture");

    let mut demuxer = Mp4PacketDemuxer::from_bytes(Arc::from(bytes)).expect("open demuxer");
    let video_track_id = demuxer
      .tracks()
      .iter()
      .find(|t| t.codec == MediaCodec::H264)
      .map(|t| t.id)
      .expect("H264 track");

    let mut video_pts = Vec::new();
    let mut video_dts = Vec::new();
    for _ in 0..64 {
      let Some(pkt) = demuxer.next_packet().expect("read packet") else {
        break;
      };
      if pkt.track_id == video_track_id {
        video_pts.push(pkt.pts_ns);
        video_dts.push(pkt.dts_ns);
      }
    }

    assert!(
      video_pts.len() >= 4,
      "expected at least 4 video packets, got {}",
      video_pts.len()
    );

    assert!(
      video_dts.windows(2).all(|w| w[1] >= w[0]),
      "expected monotonic DTS: {video_dts:?}"
    );

    assert!(
      video_pts.windows(2).any(|w| w[1] < w[0]),
      "expected non-monotonic PTS due to B-frames, got: {video_pts:?}"
    );
  }

  #[test]
  fn mp4_b_frames_packets_are_emitted_in_global_decode_order_by_dts() {
    let fixture_path = crate::testing::fixture_path("fixtures/media/test_h264_b_frames_aac.mp4");
    let bytes = std::fs::read(fixture_path).expect("read mp4 fixture");

    let mut demuxer = Mp4PacketDemuxer::from_bytes(Arc::from(bytes)).expect("open demuxer");

    let mut last_dts: Option<u64> = None;
    for _ in 0..256 {
      let Some(pkt) = demuxer.next_packet().expect("read packet") else {
        break;
      };
      if let Some(prev) = last_dts {
        assert!(
          pkt.dts_ns >= prev,
          "expected demuxer to emit packets ordered by DTS (decode order); DTS decreased ({} -> {})",
          prev,
          pkt.dts_ns
        );
      }
      last_dts = Some(pkt.dts_ns);
    }
  }
}

#[cfg(all(
  test,
  feature = "media_mp4",
  feature = "codec_h264_openh264",
  feature = "codec_aac"
))]
mod mp4_seek_preroll_tests {
  use super::Mp4PacketDemuxer;
  use crate::media::{DecodedItem, MediaDecodePipeline};
  use std::sync::Arc;

  #[test]
  fn mp4_seek_to_mid_stream_decodes_from_keyframe_and_prerolls() {
    let fixture_path = crate::testing::fixture_path("fixtures/media/test_h264_b_frames_aac.mp4");
    let bytes = std::fs::read(fixture_path).expect("read mp4 fixture");

    let demuxer = Mp4PacketDemuxer::from_bytes(Arc::from(bytes)).expect("open demuxer");
    let mut pipeline = MediaDecodePipeline::new(Box::new(demuxer)).expect("pipeline");

    let target_ns = 1_500_000_000_u64;
    pipeline.seek(target_ns).expect("seek");

    // Decode until we see a video frame at or after the seek target.
    for _ in 0..256 {
      let Some(item) = pipeline.next_decoded().expect("next_decoded") else {
        break;
      };
      if let DecodedItem::Video(frame) = item {
        assert!(
          frame.pts_ns >= target_ns,
          "expected preroll-drop to suppress frames before target ({}), got {}",
          target_ns,
          frame.pts_ns
        );
        return;
      }
    }

    panic!("expected to decode a video frame after seek");
  }
}

#[cfg(feature = "media_mp4")]
impl<R: Read + Seek + Send> MediaDemuxer for Mp4PacketDemuxer<R> {
  fn tracks(&self) -> &[MediaTrackInfo] {
    &self.tracks
  }

  fn next_packet(&mut self) -> MediaResult<Option<MediaPacket>> {
    for cursor in &mut self.cursors {
      mp4_fill_peeked(&mut self.mp4, &self.sample_tables, cursor)?;
    }

    let Some((best_idx, _, _)) = self
      .cursors
      .iter()
      .enumerate()
      .filter_map(|(i, c)| c.peeked.as_ref().map(|p| (i, p.dts_ns, p.track_id)))
      .min_by_key(|(_, dts_ns, track_id)| (*dts_ns, *track_id))
    else {
      return Ok(None);
    };

    Ok(self.cursors[best_idx].peeked.take())
  }

  fn seek(&mut self, time_ns: u64) -> MediaResult<()> {
    for cursor in &mut self.cursors {
      cursor.peeked = None;

      if time_ns == 0 {
        cursor.next_sample = 1;
        continue;
      }

      let Some(track_table) = self.sample_tables.get(&cursor.id) else {
        // Fallback: scan forward until the first sample with PTS >= time.
        cursor.next_sample = 1;
        loop {
          mp4_fill_peeked(&mut self.mp4, &self.sample_tables, cursor)?;
          match cursor.peeked.as_ref() {
            None => break,
            Some(pkt) if pkt.pts_ns < time_ns => {
              cursor.peeked = None;
              continue;
            }
            Some(_) => break,
          }
        }
        continue;
      };

      let sample_idx0 = track_table.sample_index_at_or_after(time_ns);
      if sample_idx0 >= cursor.sample_count as usize {
        cursor.next_sample = cursor.sample_count.saturating_add(1);
        continue;
      }

      let seek_idx0 = match cursor.track_type {
        MediaTrackType::Video => track_table.sync_sample_at_or_before(sample_idx0),
        MediaTrackType::Audio => sample_idx0,
      };

      cursor.next_sample = (seek_idx0 as u32).saturating_add(1);
    }
    Ok(())
  }
}

#[cfg(feature = "media_mp4")]
fn mp4_fill_peeked<R: Read + Seek>(
  mp4: &mut mp4::Mp4Reader<R>,
  sample_tables: &HashMap<u32, TrackSampleTable>,
  cursor: &mut Mp4TrackCursor,
) -> MediaResult<()> {
  if cursor.peeked.is_some() {
    return Ok(());
  }

  while cursor.next_sample <= cursor.sample_count {
    let sample_idx = cursor.next_sample;
    cursor.next_sample += 1;

    let Some(sample) = mp4
      .read_sample(cursor.id, sample_idx)
      .map_err(|e| MediaError::Demux(format!("mp4: failed to read sample: {e}")))?
    else {
      continue;
    };

    let timing = sample_tables
      .get(&cursor.id)
      .and_then(|t| t.samples.get(sample_idx as usize - 1))
      .copied();

    let (dts_ns, pts_ns, duration_ns, is_keyframe) = match timing {
      Some(t) => (t.dts_ns, t.pts_ns, t.duration_ns, t.is_sync),
      None => {
        // Fallback: treat the mp4 crate's sample time as both PTS+DTS (no CTTS support).
        let start_time = sample.start_time;
        let pts_ns = if cursor.timescale == 0 {
          0
        } else {
          (start_time.saturating_mul(1_000_000_000)).saturating_div(u64::from(cursor.timescale))
        };
        (pts_ns, pts_ns, 0, sample.is_sync)
      }
    };

    cursor.peeked = Some(MediaPacket {
      track_id: u64::from(cursor.id),
      dts_ns,
      pts_ns,
      duration_ns,
      data: sample.bytes.to_vec().into(),
      is_keyframe,
    });
    return Ok(());
  }

  Ok(())
}

#[cfg(feature = "media_mp4")]
fn mp4_track_audio_params(track: &mp4::Mp4Track) -> MediaResult<(u32, u16)> {
  let mp4a = track
    .trak
    .mdia
    .minf
    .stbl
    .stsd
    .mp4a
    .as_ref()
    .ok_or_else(|| MediaError::Demux("mp4: AAC track missing mp4a sample entry".into()))?;

  // For MP4 audio tracks, `mdhd.timescale` is typically the sample rate.
  let sample_rate = track.trak.mdia.mdhd.timescale;
  let channels = mp4a.channelcount;

  Ok((sample_rate, channels))
}

#[cfg(feature = "media_mp4")]
fn build_aac_lc_audio_specific_config(sample_rate: u32, channels: u16) -> MediaResult<Vec<u8>> {
  // AAC LC.
  let audio_object_type: u8 = 2;

  // Table from ISO/IEC 14496-3.
  let sampling_frequency_index: u8 = match sample_rate {
    96_000 => 0,
    88_200 => 1,
    64_000 => 2,
    48_000 => 3,
    44_100 => 4,
    32_000 => 5,
    24_000 => 6,
    22_050 => 7,
    16_000 => 8,
    12_000 => 9,
    11_025 => 10,
    8_000 => 11,
    7_350 => 12,
    _ => {
      return Err(MediaError::Decode(format!(
        "unsupported AAC sample rate for ASC: {sample_rate}"
      )))
    }
  };

  if channels == 0 || channels > 7 {
    return Err(MediaError::Decode(format!(
      "unsupported AAC channel count for ASC: {channels}"
    )));
  }

  // AudioSpecificConfig (ASC) layout (most common 2-byte form):
  // - audioObjectType (5)
  // - samplingFrequencyIndex (4)
  // - channelConfiguration (4)
  // - frameLengthFlag (1) = 0
  // - dependsOnCoreCoder (1) = 0
  // - extensionFlag (1) = 0
  let byte0 = (audio_object_type << 3) | (sampling_frequency_index >> 1);
  let byte1 = ((sampling_frequency_index & 0b1) << 7) | ((channels as u8) << 3);
  Ok(vec![byte0, byte1])
}

// ============================================================================
// MP4 packet demuxer (feature-disabled stub)
// ============================================================================

#[cfg(not(feature = "media_mp4"))]
pub struct Mp4PacketDemuxer<R: Read + Seek + Send> {
  _phantom: std::marker::PhantomData<R>,
}

#[cfg(not(feature = "media_mp4"))]
impl Mp4PacketDemuxer<std::io::BufReader<std::fs::File>> {
  pub fn open(_path: impl AsRef<std::path::Path>) -> MediaResult<Self> {
    Err(MediaError::Unsupported(
      "`media_mp4` feature disabled (enable Cargo feature `media_mp4` or `media`)".into(),
    ))
  }
}

#[cfg(not(feature = "media_mp4"))]
impl<R: Read + Seek + Send> MediaDemuxer for Mp4PacketDemuxer<R> {
  fn tracks(&self) -> &[MediaTrackInfo] {
    &[]
  }

  fn next_packet(&mut self) -> MediaResult<Option<MediaPacket>> {
    Err(MediaError::Unsupported(
      "`media_mp4` feature disabled (enable Cargo feature `media_mp4` or `media`)".into(),
    ))
  }

  fn seek(&mut self, _time_ns: u64) -> MediaResult<()> {
    Err(MediaError::Unsupported(
      "`media_mp4` feature disabled (enable Cargo feature `media_mp4` or `media`)".into(),
    ))
  }
}
