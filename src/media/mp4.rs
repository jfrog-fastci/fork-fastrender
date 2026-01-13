//! Minimal MP4 (ISO BMFF) demuxer utilities.
//!
//! The initial focus is *time-based seeking* by presentation timestamp (PTS).
//! MP4 sample tables can be large (millions of samples), so `seek(time_ns)` must avoid linear
//! scans for common files.
//!
//! This module intentionally implements only the subset of ISO BMFF needed for unit tests and
//! basic MP4 playback plumbing (non-fragmented `moov`/`mdat` files).

use mp4parse::unstable::Indice;
use std::io::{Read, Seek, SeekFrom};
use std::ops::Range;
use std::sync::Arc;
use thiserror::Error;

use crate::error::{RenderError, RenderStage};
use crate::render_control::{check_root, check_root_periodic};

use super::{MediaData, MediaError, MediaLimits, MediaPacket, MediaResult, MediaTrackType};

const MP4_PARSE_DEADLINE_STRIDE: usize = 1024;
// MP4 sample tables can express very large `sample_count` values compactly (e.g. a single `stts`
// run with `sample_count = 0xFFFF_FFFF`). Building per-sample `Vec`s for such inputs would attempt
// to allocate tens of gigabytes and can trivially OOM the process. Use `MediaLimits` to keep this
// bounded.

// Hard cap on MP4 sample table entry counts (stts/ctts/stsc/stco/co64/stss).
//
// These boxes include an `entry_count` field that can be set to absurd values even when the box
// payload is tiny (e.g. a truncated file). We cap the count *before* allocating a `Vec` with that
// capacity.
const MAX_TABLE_ENTRIES: u32 = 1_000_000;

/// Hard cap on per-sample packet bytes in the `mp4parse`-based demuxer.
///
/// This demuxer reads each sample into a fresh `Vec<u8>`. Without a cap, an attacker-controlled
/// sample table can request an allocation of arbitrary size via `end_offset - start_offset`.
const MAX_MP4_PACKET_BYTES: u64 = 64 * 1024 * 1024; // 64 MiB

#[derive(Debug, Error)]
pub enum Mp4Error {
  #[error("unexpected end of file")]
  UnexpectedEof,
  #[error("invalid mp4 box size")]
  InvalidBoxSize,
  #[error("render error: {0}")]
  Render(#[from] RenderError),
  #[error("invalid mp4: {0}")]
  Invalid(&'static str),
  #[error(
    "mp4 sample out of bounds (track {track_index}, sample {sample_index}): {range:?} (file len {file_len})"
  )]
  SampleOutOfBounds {
    track_index: usize,
    sample_index: usize,
    range: Range<usize>,
    file_len: usize,
  },
  #[error("unsupported mp4 box version {version} for {box_name}")]
  UnsupportedBoxVersion { box_name: &'static str, version: u8 },
  #[error("missing required mp4 box: {0}")]
  MissingBox(&'static str),
  #[error("mp4 has too many tracks: {track_count} (max {max})")]
  TooManyTracks { track_count: usize, max: usize },
  #[error("mp4 track has too many samples: {sample_count} (max {max})")]
  TooManySamples { sample_count: u64, max: u64 },
  #[error("mp4 box {box_name} has too many entries: {entry_count} (max {max})")]
  TooManyTableEntries {
    box_name: &'static str,
    entry_count: u64,
    max: u64,
  },
  #[error("mp4 sample size {size} exceeds max_packet_bytes {max}")]
  SampleTooLarge { size: u32, max: usize },
}

type Result<T> = std::result::Result<T, Mp4Error>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SeekMethod {
  /// Binary search directly over a monotonic `pts_ns_by_sample` vector.
  MonotonicBinarySearch,
  /// Binary search over a `(pts, sample_index)` table sorted by PTS (used when PTS is non-monotonic
  /// in decode order due to `ctts` reordering).
  SortedBinarySearch,
  /// Fallback linear scan.
  LinearScan,
}

#[derive(Debug, Clone)]
pub struct Mp4Sample {
  pub offset: u64,
  pub size: u32,
  pub dts_ticks: u64,
  pub duration_ticks: u32,
  pub is_sync: bool,
}

#[derive(Debug, Clone)]
pub struct Mp4Track {
  id: u32,
  timescale: u32,
  samples: Vec<Mp4Sample>,
  pts_ns_by_sample: Vec<u64>,
  pts_index: PtsIndex,
  /// Next sample index in *decode order*.
  next_sample: usize,
  last_seek_method: Option<SeekMethod>,
}

impl Mp4Track {
  #[must_use]
  pub fn id(&self) -> u32 {
    self.id
  }

  #[must_use]
  pub fn timescale(&self) -> u32 {
    self.timescale
  }

  #[must_use]
  pub fn samples(&self) -> &[Mp4Sample] {
    &self.samples
  }

  #[must_use]
  pub fn pts_ns_by_sample(&self) -> &[u64] {
    &self.pts_ns_by_sample
  }

  #[must_use]
  pub fn next_sample(&self) -> usize {
    self.next_sample
  }

  #[must_use]
  pub fn last_seek_method(&self) -> Option<SeekMethod> {
    self.last_seek_method
  }

  fn seek(&mut self, time_ns: u64) {
    let (idx, method) = self.find_sample_for_time_ns(time_ns);
    self.next_sample = idx;
    self.last_seek_method = Some(method);
  }

  fn find_sample_for_time_ns(&self, time_ns: u64) -> (usize, SeekMethod) {
    match &self.pts_index {
      PtsIndex::Monotonic => {
        let idx = self.pts_ns_by_sample.partition_point(|&pts| pts < time_ns);
        (idx, SeekMethod::MonotonicBinarySearch)
      }
      PtsIndex::Sorted {
        sample_indices_by_pts,
        min_sample_index_from_pos,
      } => {
        let pos =
          sample_indices_by_pts.partition_point(|&i| self.pts_ns_by_sample[i as usize] < time_ns);
        let idx = min_sample_index_from_pos
          .get(pos)
          .map(|&idx| idx as usize)
          .unwrap_or_else(|| self.samples.len());
        (idx, SeekMethod::SortedBinarySearch)
      }
    }
  }
}

#[derive(Debug, Clone)]
pub struct Mp4Demuxer {
  bytes: Arc<[u8]>,
  tracks: Vec<Mp4Track>,
}

impl Mp4Demuxer {
  /// Parses an MP4 from in-memory bytes.
  ///
  /// Note: This convenience overload clones `bytes` into an `Arc<[u8]>`. Call
  /// [`Mp4Demuxer::from_arc`] to avoid the copy when you already have an `Arc` (e.g. from a memory
  /// map or `Vec<u8>`).
  pub fn new(bytes: &[u8]) -> Result<Self> {
    let limits = MediaLimits::default();
    Self::from_arc_with_limits(Arc::from(bytes), &limits)
  }

  pub fn from_bytes(bytes: &[u8]) -> Result<Self> {
    let limits = MediaLimits::default();
    Self::from_arc_with_limits(Arc::from(bytes), &limits)
  }

  pub fn from_arc(bytes: Arc<[u8]>) -> Result<Self> {
    let limits = MediaLimits::default();
    Self::from_arc_with_limits(bytes, &limits)
  }

  pub fn from_bytes_with_limits(bytes: &[u8], limits: &MediaLimits) -> Result<Self> {
    Self::from_arc_with_limits(Arc::from(bytes), limits)
  }

  pub fn from_arc_with_limits(bytes: Arc<[u8]>, limits: &MediaLimits) -> Result<Self> {
    check_root(RenderStage::Paint)?;
    let moov =
      find_top_level_box(bytes.as_ref(), fourcc(b"moov"))?.ok_or(Mp4Error::MissingBox("moov"))?;

    let file_len = bytes.len();
    let tracks = parse_moov(bytes.as_ref(), moov, limits)?
      .into_iter()
      .enumerate()
      .map(|(track_index, t)| build_track(track_index, t, file_len, limits))
      .collect::<Result<Vec<_>>>()?;

    if tracks.is_empty() {
      return Err(Mp4Error::MissingBox("trak"));
    }

    Ok(Self { bytes, tracks })
  }

  #[must_use]
  pub fn tracks(&self) -> &[Mp4Track] {
    &self.tracks
  }

  /// Returns demuxed packets for `track_index` (0-based, in decode order).
  ///
  /// Packet bytes are returned as [`MediaData::Shared`] ranges into the original `Arc<[u8]>`.
  pub fn packets_for_track(&self, track_index: usize) -> Result<Vec<MediaPacket>> {
    let Some(track) = self.tracks.get(track_index) else {
      return Err(Mp4Error::Invalid("mp4 track index out of range"));
    };

    let mut packets = Vec::with_capacity(track.samples.len());

    for (sample_index, sample) in track.samples.iter().enumerate() {
      let start = usize::try_from(sample.offset).unwrap_or(usize::MAX);
      let end = start
        .checked_add(sample.size as usize)
        .unwrap_or(usize::MAX);
      let range = start..end;

      if range.start >= range.end || range.end > self.bytes.len() {
        return Err(Mp4Error::SampleOutOfBounds {
          track_index,
          sample_index,
          range,
          file_len: self.bytes.len(),
        });
      }

      let pts_ns = *track
        .pts_ns_by_sample
        .get(sample_index)
        .ok_or(Mp4Error::Invalid("mp4 pts index out of range"))?;
      let dts_ns = ticks_to_ns(
        i64::try_from(sample.dts_ticks).unwrap_or(i64::MAX),
        track.timescale,
      );
      let duration_ns = ticks_to_ns(i64::from(sample.duration_ticks), track.timescale);

      packets.push(MediaPacket {
        track_id: u64::from(track.id),
        dts_ns,
        pts_ns,
        duration_ns,
        data: MediaData::Shared {
          bytes: Arc::clone(&self.bytes),
          range,
        },
        is_keyframe: sample.is_sync,
      });
    }

    Ok(packets)
  }

  /// Seeks all tracks to the first sample with `pts_ns >= time_ns`.
  ///
  /// For tracks where PTS is monotonic in sample (decode) order, this is O(log n) via binary search
  /// on the per-track `pts_ns_by_sample: Vec<u64>`.
  ///
  /// When `ctts` reorders composition times (PTS becomes non-monotonic in decode order), the track
  /// uses a sorted sample-index table keyed by PTS (still O(log n)).
  pub fn seek(&mut self, time_ns: u64) {
    for track in &mut self.tracks {
      track.seek(time_ns);
    }
  }
}

/// A lightweight, seek-only MP4 index that maps `(track_id, pts_ns)` → sample index.
///
/// This is intended for container demuxers that already know how to read sample bytes (e.g. via the
/// external `mp4` crate) but want efficient timestamp-based seeking without scanning packets.
#[derive(Debug, Clone)]
pub struct Mp4SeekIndex {
  tracks: Vec<Mp4SeekTrack>,
}

impl Mp4SeekIndex {
  pub fn from_bytes(bytes: &[u8]) -> Result<Self> {
    let limits = MediaLimits::default();
    Self::from_bytes_with_limits(bytes, &limits)
  }

  pub fn from_bytes_with_limits(bytes: &[u8], limits: &MediaLimits) -> Result<Self> {
    check_root(RenderStage::Paint)?;
    let moov = find_top_level_box(bytes, fourcc(b"moov"))?.ok_or(Mp4Error::MissingBox("moov"))?;

    let tracks = parse_moov_seek(bytes, moov, limits)?
      .into_iter()
      .map(|t| build_seek_track(t, limits))
      .collect::<Result<Vec<_>>>()?;

    if tracks.is_empty() {
      return Err(Mp4Error::MissingBox("trak"));
    }

    Ok(Self { tracks })
  }

  #[must_use]
  pub fn tracks(&self) -> &[Mp4SeekTrack] {
    &self.tracks
  }

  #[must_use]
  pub fn track(&self, track_id: u32) -> Option<&Mp4SeekTrack> {
    self.tracks.iter().find(|t| t.id == track_id)
  }
}

#[derive(Debug, Clone)]
pub struct Mp4SeekTrack {
  id: u32,
  timescale: u32,
  pts_ns_by_sample: Vec<u64>,
  pts_index: PtsIndex,
}

impl Mp4SeekTrack {
  #[must_use]
  pub fn id(&self) -> u32 {
    self.id
  }

  #[must_use]
  pub fn timescale(&self) -> u32 {
    self.timescale
  }

  #[must_use]
  pub fn sample_count(&self) -> usize {
    self.pts_ns_by_sample.len()
  }

  /// Returns the first sample index in decode order with `pts_ns >= time_ns`.
  #[must_use]
  pub fn sample_index_at_or_after(&self, time_ns: u64) -> usize {
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
}

#[derive(Debug, Clone)]
enum PtsIndex {
  /// PTS values are monotonic in sample order (common for audio and baseline-profile H.264).
  Monotonic,
  /// PTS values are non-monotonic in sample order (e.g. B-frame reordering via `ctts`).
  ///
  /// To keep seeking fast while still returning a decode-order sample index:
  /// - `sample_indices_by_pts` is sorted by PTS (then by index as a tiebreaker).
  /// - `min_sample_index_from_pos[i]` stores the minimum decode-order sample index among
  ///   `sample_indices_by_pts[i..]`.
  ///
  /// Seeking uses binary search to find the first position `pos` where `pts >= target`, then picks
  /// `min_sample_index_from_pos[pos]`. This matches the semantics of a linear scan in decode order
  /// ("first sample with pts>=target") without jumping directly to a B-frame that depends on earlier
  /// reference frames.
  Sorted {
    sample_indices_by_pts: Vec<u32>,
    min_sample_index_from_pos: Vec<u32>,
  },
}

#[derive(Debug, Clone, Copy)]
struct SttsEntry {
  sample_count: u32,
  sample_delta: u32,
}

#[derive(Debug, Clone, Copy)]
struct CttsEntry {
  sample_count: u32,
  sample_offset: i64,
}

#[derive(Debug, Clone, Copy)]
struct StscEntry {
  first_chunk: u32,
  samples_per_chunk: u32,
}

#[derive(Debug, Clone)]
struct StszBox {
  sample_size: u32,
  sample_sizes: Vec<u32>,
  sample_count: u32,
}

#[derive(Debug, Default)]
struct TrackBoxes {
  id: Option<u32>,
  timescale: Option<u32>,
  stts: Option<Vec<SttsEntry>>,
  ctts: Option<Vec<CttsEntry>>,
  stsc: Option<Vec<StscEntry>>,
  stsz: Option<StszBox>,
  chunk_offsets: Option<Vec<u64>>,
  stss: Option<Vec<u32>>,
}

#[derive(Debug, Default)]
struct SeekTrackBoxes {
  id: Option<u32>,
  timescale: Option<u32>,
  stts: Option<Vec<SttsEntry>>,
  ctts: Option<Vec<CttsEntry>>,
}

fn build_track(track_index: usize, t: TrackBoxes, file_len: usize, limits: &MediaLimits) -> Result<Mp4Track> {
  let id = t.id.ok_or(Mp4Error::MissingBox("tkhd"))?;
  let timescale = t.timescale.ok_or(Mp4Error::MissingBox("mdhd"))?;
  if timescale == 0 {
    return Err(Mp4Error::Invalid("mdhd timescale must be > 0"));
  }
  let stts = t.stts.ok_or(Mp4Error::MissingBox("stts"))?;
  let stsc = t.stsc.ok_or(Mp4Error::MissingBox("stsc"))?;
  let stsz = t.stsz.ok_or(Mp4Error::MissingBox("stsz"))?;
  let chunk_offsets = t.chunk_offsets.ok_or(Mp4Error::MissingBox("stco/co64"))?;
  let ctts = t.ctts.unwrap_or_default();

  let sample_count = stsz.sample_count as usize;
  if sample_count == 0 {
    return Ok(Mp4Track {
      id,
      timescale,
      samples: Vec::new(),
      pts_ns_by_sample: Vec::new(),
      pts_index: PtsIndex::Monotonic,
      next_sample: 0,
      last_seek_method: None,
    });
  }
  if u64::from(stsz.sample_count) > limits.max_samples_per_track as u64 {
    return Err(Mp4Error::TooManySamples {
      sample_count: u64::from(stsz.sample_count),
      max: limits.max_samples_per_track as u64,
    });
  }

  let stts_total: u64 = stts.iter().map(|e| u64::from(e.sample_count)).sum();
  if stts_total != sample_count as u64 {
    return Err(Mp4Error::Invalid("stts sample_count sum mismatch"));
  }
  if !ctts.is_empty() {
    let ctts_total: u64 = ctts.iter().map(|e| u64::from(e.sample_count)).sum();
    if ctts_total != sample_count as u64 {
      return Err(Mp4Error::Invalid("ctts sample_count sum mismatch"));
    }
  }

  // Build sync flags.
  let mut sync_flags = vec![false; sample_count];
  match t.stss {
    None => {
      // No sync table means all samples are sync samples.
      sync_flags.fill(true);
    }
    Some(stss) => {
      for sample_num_1_based in stss {
        let idx = sample_num_1_based
          .checked_sub(1)
          .ok_or(Mp4Error::Invalid("stss sample number must be >= 1"))? as usize;
        if idx >= sample_count {
          return Err(Mp4Error::Invalid("stss sample number out of range"));
        }
        sync_flags[idx] = true;
      }
    }
  }

  // Build sample offsets/sizes in decode order using stsc + stco/co64 + stsz.
  let mut samples = Vec::with_capacity(sample_count);
  let mut sample_idx = 0usize;
  let mut stsc_idx = 0usize;
  let mut deadline_counter = 0usize;

  for (chunk_idx0, &chunk_base) in chunk_offsets.iter().enumerate() {
    check_root_periodic(
      &mut deadline_counter,
      MP4_PARSE_DEADLINE_STRIDE,
      RenderStage::Paint,
    )?;
    if sample_idx >= sample_count {
      break;
    }

    let chunk_num = (chunk_idx0 + 1) as u32; // stsc is 1-based.
    while stsc_idx + 1 < stsc.len() && chunk_num >= stsc[stsc_idx + 1].first_chunk {
      stsc_idx += 1;
    }
    let samples_per_chunk = stsc[stsc_idx].samples_per_chunk as usize;

    let mut offset = chunk_base;
    for _ in 0..samples_per_chunk {
      check_root_periodic(
        &mut deadline_counter,
        MP4_PARSE_DEADLINE_STRIDE,
        RenderStage::Paint,
      )?;
      if sample_idx >= sample_count {
        break;
      }

      let size = if stsz.sample_size != 0 {
        stsz.sample_size
      } else {
        *stsz
          .sample_sizes
          .get(sample_idx)
          .ok_or(Mp4Error::Invalid("stsz sample_sizes underrun"))?
      };

      let start = offset;
      if size as usize > limits.max_packet_bytes {
        return Err(Mp4Error::SampleTooLarge {
          size,
          max: limits.max_packet_bytes,
        });
      }
      let end = offset
        .checked_add(u64::from(size))
        .ok_or(Mp4Error::Invalid("sample offset overflow"))?;

      let start_usize = usize::try_from(start).unwrap_or(usize::MAX);
      let end_usize = usize::try_from(end).unwrap_or(usize::MAX);
      let range = start_usize..end_usize;
      if range.start >= range.end || range.end > file_len {
        return Err(Mp4Error::SampleOutOfBounds {
          track_index,
          sample_index: sample_idx,
          range,
          file_len,
        });
      }
      offset = end;

      samples.push(Mp4Sample {
        offset: start,
        size,
        dts_ticks: 0,
        duration_ticks: 0,
        is_sync: sync_flags[sample_idx],
      });

      sample_idx += 1;
    }
  }

  if sample_idx != sample_count {
    return Err(Mp4Error::Invalid(
      "sample table construction did not yield expected sample_count",
    ));
  }

  // Fill decode timestamps/durations and compute the minimum PTS tick value.
  //
  // MP4 `ctts` offsets can be negative (version 1), producing negative PTS values (e.g. decode
  // offsets). `MediaPacket` stores timestamps as `u64` nanoseconds, so we normalize PTS by shifting
  // the entire track so the minimum PTS becomes 0.
  let mut stts_iter = TableRunIter::new_stts(&stts);
  let mut ctts_iter = TableRunIter::new_ctts(&ctts);

  let mut dts_ticks: i64 = 0;
  let mut min_pts_ticks: i64 = i64::MAX;
  let mut deadline_counter = 0usize;

  for sample in &mut samples {
    check_root_periodic(
      &mut deadline_counter,
      MP4_PARSE_DEADLINE_STRIDE,
      RenderStage::Paint,
    )?;
    let dur = stts_iter
      .next_u32()
      .ok_or(Mp4Error::Invalid("stts shorter than sample_count"))?;
    let ctts_off = ctts_iter
      .next_i64()
      .ok_or(Mp4Error::Invalid("ctts shorter than sample_count"))?;

    sample.dts_ticks = dts_ticks.max(0) as u64;
    sample.duration_ticks = dur;

    let pts_ticks = dts_ticks.saturating_add(ctts_off);
    min_pts_ticks = min_pts_ticks.min(pts_ticks);

    dts_ticks = dts_ticks.saturating_add(i64::from(dur));
  }

  let pts_offset_ticks: i128 = if min_pts_ticks < 0 {
    -(min_pts_ticks as i128)
  } else {
    0
  };

  // Build a nanosecond PTS table for seeking, applying the normalization offset.
  let mut pts_ns_by_sample = Vec::with_capacity(sample_count);
  let mut pts_is_monotonic = true;
  let mut prev_pts_ns = 0_u64;
  let mut saw_prev_pts = false;

  let mut dts_ticks: i64 = 0;
  let mut ctts_iter = TableRunIter::new_ctts(&ctts);
  let mut deadline_counter = 0usize;
  for sample in &samples {
    check_root_periodic(
      &mut deadline_counter,
      MP4_PARSE_DEADLINE_STRIDE,
      RenderStage::Paint,
    )?;
    let ctts_off = ctts_iter
      .next_i64()
      .ok_or(Mp4Error::Invalid("ctts shorter than sample_count"))?;

    let pts_ticks = dts_ticks.saturating_add(ctts_off);
    let shifted = (pts_ticks as i128).saturating_add(pts_offset_ticks);
    let shifted = if shifted > i64::MAX as i128 {
      i64::MAX
    } else if shifted <= 0 {
      0
    } else {
      shifted as i64
    };

    let pts_ns = ticks_to_ns(shifted, timescale);
    if saw_prev_pts && pts_ns < prev_pts_ns {
      pts_is_monotonic = false;
    }
    prev_pts_ns = pts_ns;
    saw_prev_pts = true;
    pts_ns_by_sample.push(pts_ns);

    dts_ticks = dts_ticks.saturating_add(i64::from(sample.duration_ticks));
  }

  let pts_index = if pts_is_monotonic {
    PtsIndex::Monotonic
  } else {
    build_sorted_pts_index(&pts_ns_by_sample)?
  };

  Ok(Mp4Track {
    id,
    timescale,
    samples,
    pts_ns_by_sample,
    pts_index,
    next_sample: 0,
    last_seek_method: None,
  })
}

fn build_seek_track(t: SeekTrackBoxes, limits: &MediaLimits) -> Result<Mp4SeekTrack> {
  let id = t.id.ok_or(Mp4Error::MissingBox("tkhd"))?;
  let timescale = t.timescale.ok_or(Mp4Error::MissingBox("mdhd"))?;
  if timescale == 0 {
    return Err(Mp4Error::Invalid("mdhd timescale must be > 0"));
  }

  let stts = t.stts.ok_or(Mp4Error::MissingBox("stts"))?;
  let ctts = t.ctts.unwrap_or_default();
  let has_ctts = !ctts.is_empty();

  let stts_total: u64 = stts.iter().map(|e| u64::from(e.sample_count)).sum();
  if stts_total == 0 {
    return Ok(Mp4SeekTrack {
      id,
      timescale,
      pts_ns_by_sample: Vec::new(),
      pts_index: PtsIndex::Monotonic,
    });
  }
  if stts_total > limits.max_samples_per_track as u64 {
    return Err(Mp4Error::TooManySamples {
      sample_count: stts_total,
      max: limits.max_samples_per_track as u64,
    });
  }

  if has_ctts {
    let ctts_total: u64 = ctts.iter().map(|e| u64::from(e.sample_count)).sum();
    if ctts_total != stts_total {
      return Err(Mp4Error::Invalid("ctts sample_count sum mismatch"));
    }
  }

  let sample_count =
    usize::try_from(stts_total).map_err(|_| Mp4Error::Invalid("sample_count overflow"))?;

  let mut stts_iter = TableRunIter::new_stts(&stts);
  let mut ctts_iter = TableRunIter::new_ctts(&ctts);

  // First pass: compute the minimum (possibly negative) PTS tick value so we can normalize.
  let mut dts_ticks: i64 = 0;
  let mut min_pts_ticks: i64 = i64::MAX;

  let mut deadline_counter = 0usize;
  for _ in 0..sample_count {
    check_root_periodic(
      &mut deadline_counter,
      MP4_PARSE_DEADLINE_STRIDE,
      RenderStage::Paint,
    )?;
    let dur = stts_iter
      .next_u32()
      .ok_or(Mp4Error::Invalid("stts shorter than sample_count"))?;
    let ctts_off = ctts_iter
      .next_i64()
      .ok_or(Mp4Error::Invalid("ctts shorter than sample_count"))?;

    let pts_ticks = dts_ticks.saturating_add(ctts_off);
    min_pts_ticks = min_pts_ticks.min(pts_ticks);

    dts_ticks = dts_ticks.saturating_add(i64::from(dur));
  }

  let pts_offset_ticks: i128 = if min_pts_ticks < 0 {
    -(min_pts_ticks as i128)
  } else {
    0
  };

  // Second pass: build a normalized nanosecond PTS table (and monotonicity metadata).
  let mut stts_iter = TableRunIter::new_stts(&stts);
  let mut ctts_iter = TableRunIter::new_ctts(&ctts);
  let mut dts_ticks: i64 = 0;
  let mut pts_ns_by_sample = Vec::with_capacity(sample_count);
  let mut pts_is_monotonic = true;
  let mut prev_pts_ns = 0_u64;
  let mut saw_prev_pts = false;
  let mut deadline_counter = 0usize;

  for _ in 0..sample_count {
    check_root_periodic(
      &mut deadline_counter,
      MP4_PARSE_DEADLINE_STRIDE,
      RenderStage::Paint,
    )?;
    let dur = stts_iter
      .next_u32()
      .ok_or(Mp4Error::Invalid("stts shorter than sample_count"))?;
    let ctts_off = ctts_iter
      .next_i64()
      .ok_or(Mp4Error::Invalid("ctts shorter than sample_count"))?;

    let pts_ticks = dts_ticks.saturating_add(ctts_off);
    let shifted = (pts_ticks as i128).saturating_add(pts_offset_ticks);
    let shifted = if shifted > i64::MAX as i128 {
      i64::MAX
    } else if shifted <= 0 {
      0
    } else {
      shifted as i64
    };

    let pts_ns = ticks_to_ns(shifted, timescale);
    if saw_prev_pts && pts_ns < prev_pts_ns {
      pts_is_monotonic = false;
    }
    prev_pts_ns = pts_ns;
    saw_prev_pts = true;
    pts_ns_by_sample.push(pts_ns);

    dts_ticks = dts_ticks.saturating_add(i64::from(dur));
  }

  let pts_index = if pts_is_monotonic {
    PtsIndex::Monotonic
  } else {
    build_sorted_pts_index(&pts_ns_by_sample)?
  };

  Ok(Mp4SeekTrack {
    id,
    timescale,
    pts_ns_by_sample,
    pts_index,
  })
}

fn build_pts_index(pts_ns_by_sample: &[u64]) -> Result<PtsIndex> {
  // Helper used in tests; production code does the monotonic check while building the table.
  if pts_ns_by_sample.windows(2).all(|pair| pair[0] <= pair[1]) {
    return Ok(PtsIndex::Monotonic);
  }
  build_sorted_pts_index(pts_ns_by_sample)
}

fn build_sorted_pts_index(pts_ns_by_sample: &[u64]) -> Result<PtsIndex> {
  // Non-monotonic (e.g. B-frames / CTTS reordering). Build a sorted index.
  if pts_ns_by_sample.len() > u32::MAX as usize {
    return Err(Mp4Error::Invalid("pts index sample table too large"));
  }
  let mut deadline_counter = 0usize;
  let mut sample_indices_by_pts = Vec::with_capacity(pts_ns_by_sample.len());
  for i in 0..pts_ns_by_sample.len() {
    check_root_periodic(
      &mut deadline_counter,
      MP4_PARSE_DEADLINE_STRIDE,
      RenderStage::Paint,
    )?;
    // `stsz.sample_count` is a u32, so sample tables should always fit.
    sample_indices_by_pts.push(i as u32);
  }
  // We don't require stable ordering because we include the sample index as a tiebreaker (unique
  // key), so `sort_unstable_by_key` avoids the extra scratch allocations of the stable sort.
  check_root_periodic(
    &mut deadline_counter,
    MP4_PARSE_DEADLINE_STRIDE,
    RenderStage::Paint,
  )?;
  sample_indices_by_pts.sort_unstable_by_key(|&i| (pts_ns_by_sample[i as usize], i));
  check_root_periodic(
    &mut deadline_counter,
    MP4_PARSE_DEADLINE_STRIDE,
    RenderStage::Paint,
  )?;

  // Precompute suffix minima so seeking can return the first decode-order sample index with PTS >=
  // target without scanning the remainder of the list.
  let mut min_sample_index_from_pos = vec![0_u32; sample_indices_by_pts.len()];
  let mut min = u32::MAX;
  for (dst, &idx) in min_sample_index_from_pos
    .iter_mut()
    .rev()
    .zip(sample_indices_by_pts.iter().rev())
  {
    check_root_periodic(
      &mut deadline_counter,
      MP4_PARSE_DEADLINE_STRIDE,
      RenderStage::Paint,
    )?;
    min = min.min(idx);
    *dst = min;
  }

  Ok(PtsIndex::Sorted {
    sample_indices_by_pts,
    min_sample_index_from_pos,
  })
}

fn ticks_to_ns(ticks: i64, timescale: u32) -> u64 {
  // Match `crate::media::timebase::ticks_to_duration(Timebase { num: 1, den: timescale })`
  // semantics, but avoid constructing a `Duration` for each sample when building large MP4 tables.
  if ticks <= 0 {
    return 0;
  }

  let den = u128::from(timescale);
  if den == 0 {
    // Infinite seconds-per-tick -> unrepresentably large duration.
    return u64::MAX;
  }

  let ticks = ticks as u128;
  let ns = ticks
    .saturating_mul(1_000_000_000u128)
    .saturating_add(den / 2)
    / den;

  ns.min(u128::from(u64::MAX)) as u64
}

/// Demuxes a single MP4 track using an `mp4parse` sample table.
///
/// # Ordering (important!)
///
/// This demuxer **always emits packets in sample index order**, which corresponds to *decode
/// order*.
///
/// Do **not** reorder packets by PTS:
/// - Video samples may have **non-monotonic** PTS due to B-frames (`ctts` reordering).
/// - Decode order is not necessarily presentation order.
pub struct Mp4TrackDemuxer<R> {
  reader: R,
  sample_table: Vec<Indice>,
  timescale: u32,
  track_id: u64,
  track_type: MediaTrackType,
  next_sample_idx: usize,
  last_dts_ns: Option<u64>,
  pts_offset_ticks: i128,
}

impl<R: Read + Seek> Mp4TrackDemuxer<R> {
  pub fn new(
    reader: R,
    sample_table: Vec<Indice>,
    timescale: u32,
    track_id: u64,
    track_type: MediaTrackType,
  ) -> Self {
    let mut min_pts_ticks: i64 = i64::MAX;
    for indice in &sample_table {
      min_pts_ticks = min_pts_ticks.min(indice.start_composition.0);
    }
    if min_pts_ticks == i64::MAX {
      min_pts_ticks = 0;
    }
    let pts_offset_ticks: i128 = if min_pts_ticks < 0 {
      -(min_pts_ticks as i128)
    } else {
      0
    };
    Self {
      reader,
      sample_table,
      timescale,
      track_id,
      track_type,
      next_sample_idx: 0,
      last_dts_ns: None,
      pts_offset_ticks,
    }
  }

  pub fn next_packet(&mut self) -> MediaResult<Option<MediaPacket>> {
    let Some(indice) = self.sample_table.get(self.next_sample_idx) else {
      return Ok(None);
    };

    let start_offset = indice.start_offset.0;
    let end_offset = indice.end_offset.0;
    let sample_len = end_offset.checked_sub(start_offset).ok_or_else(|| {
      MediaError::Demux("mp4 sample table contains end_offset < start_offset".to_string())
    })?;
    if sample_len > MAX_MP4_PACKET_BYTES {
      return Err(MediaError::Demux(format!(
        "mp4 sample is too large: {sample_len} bytes (max {MAX_MP4_PACKET_BYTES})",
      )));
    }
    let sample_len_usize = usize::try_from(sample_len)
      .map_err(|_| MediaError::Demux("mp4 sample length too large to fit in memory".to_string()))?;

    self.reader.seek(SeekFrom::Start(start_offset))?;
    let mut data = vec![0u8; sample_len_usize];
    self.reader.read_exact(&mut data)?;

    // mp4parse `Indice` times are in track ticks; convert using the track timescale.
    let dts_ns = ticks_to_ns(indice.start_decode.0, self.timescale);
    let pts_ticks = (indice.start_composition.0 as i128).saturating_add(self.pts_offset_ticks);
    let pts_ticks = if pts_ticks > i64::MAX as i128 {
      i64::MAX
    } else if pts_ticks <= 0 {
      0
    } else {
      pts_ticks as i64
    };
    let pts_ns = ticks_to_ns(pts_ticks, self.timescale);
    let duration_ticks = indice
      .end_composition
      .0
      .saturating_sub(indice.start_composition.0);
    let duration_ns = ticks_to_ns(duration_ticks, self.timescale);

    // The demuxer is required to emit video packets in decode order (sample index order), even
    // when PTS is non-monotonic (B-frames). This debug assertion catches accidental PTS-based
    // reordering inside the demuxer.
    if self.track_type == MediaTrackType::Video {
      if let Some(prev_dts_ns) = self.last_dts_ns {
        debug_assert!(
          dts_ns >= prev_dts_ns,
          "MP4 demuxer must emit video packets in decode order (sample index order). \
           DTS decreased ({} -> {}); do not reorder by PTS (video PTS may be non-monotonic due to \
           B-frames).",
          prev_dts_ns,
          dts_ns
        );
      }
    }

    self.last_dts_ns = Some(dts_ns);
    self.next_sample_idx += 1;

    Ok(Some(MediaPacket {
      track_id: self.track_id,
      dts_ns,
      pts_ns,
      duration_ns,
      data: data.into(),
      is_keyframe: indice.sync,
    }))
  }
}

#[derive(Debug)]
struct Cursor<'a> {
  data: &'a [u8],
  pos: usize,
}

impl<'a> Cursor<'a> {
  fn new(data: &'a [u8], pos: usize) -> Self {
    Self { data, pos }
  }

  fn read_u8(&mut self, end: usize) -> Result<u8> {
    let end_pos = self.pos.checked_add(1).ok_or(Mp4Error::UnexpectedEof)?;
    if end_pos > end || end_pos > self.data.len() {
      return Err(Mp4Error::UnexpectedEof);
    }
    let v = *self.data.get(self.pos).ok_or(Mp4Error::UnexpectedEof)?;
    self.pos = end_pos;
    Ok(v)
  }

  fn read_u16(&mut self, end: usize) -> Result<u16> {
    let end_pos = self.pos.checked_add(2).ok_or(Mp4Error::UnexpectedEof)?;
    if end_pos > end || end_pos > self.data.len() {
      return Err(Mp4Error::UnexpectedEof);
    }
    let bytes = self
      .data
      .get(self.pos..end_pos)
      .ok_or(Mp4Error::UnexpectedEof)?;
    let arr: [u8; 2] = bytes.try_into().map_err(|_| Mp4Error::UnexpectedEof)?;
    self.pos = end_pos;
    Ok(u16::from_be_bytes(arr))
  }

  fn read_u32(&mut self, end: usize) -> Result<u32> {
    let end_pos = self.pos.checked_add(4).ok_or(Mp4Error::UnexpectedEof)?;
    if end_pos > end || end_pos > self.data.len() {
      return Err(Mp4Error::UnexpectedEof);
    }
    let bytes = self
      .data
      .get(self.pos..end_pos)
      .ok_or(Mp4Error::UnexpectedEof)?;
    let arr: [u8; 4] = bytes.try_into().map_err(|_| Mp4Error::UnexpectedEof)?;
    self.pos = end_pos;
    Ok(u32::from_be_bytes(arr))
  }

  fn read_u64(&mut self, end: usize) -> Result<u64> {
    let end_pos = self.pos.checked_add(8).ok_or(Mp4Error::UnexpectedEof)?;
    if end_pos > end || end_pos > self.data.len() {
      return Err(Mp4Error::UnexpectedEof);
    }
    let bytes = self
      .data
      .get(self.pos..end_pos)
      .ok_or(Mp4Error::UnexpectedEof)?;
    let arr: [u8; 8] = bytes.try_into().map_err(|_| Mp4Error::UnexpectedEof)?;
    self.pos = end_pos;
    Ok(u64::from_be_bytes(arr))
  }

  fn read_i32(&mut self, end: usize) -> Result<i32> {
    let v = self.read_u32(end)?;
    Ok(i32::from_be_bytes(v.to_be_bytes()))
  }

  fn skip(&mut self, end: usize, len: usize) -> Result<()> {
    let end_pos = self.pos.checked_add(len).ok_or(Mp4Error::UnexpectedEof)?;
    if end_pos > end || end_pos > self.data.len() {
      return Err(Mp4Error::UnexpectedEof);
    }
    self.pos = end_pos;
    Ok(())
  }
}

#[derive(Debug, Clone)]
struct BoxRef {
  typ: u32,
  content: Range<usize>,
  end: usize,
}

fn fourcc(tag: &[u8; 4]) -> u32 {
  u32::from_be_bytes(*tag)
}

fn next_box(cur: &mut Cursor<'_>, end: usize) -> Result<Option<BoxRef>> {
  if cur.pos >= end {
    return Ok(None);
  }
  if end - cur.pos < 8 {
    return Err(Mp4Error::UnexpectedEof);
  }

  let start = cur.pos;
  let size32 = cur.read_u32(end)?;
  let typ = cur.read_u32(end)?;

  let (size, header_len) = match size32 {
    0 => {
      let size = (end - start) as u64;
      (size, 8usize)
    }
    1 => {
      let size64 = cur.read_u64(end)?;
      (size64, 16usize)
    }
    n => (u64::from(n), 8usize),
  };

  if size < header_len as u64 {
    return Err(Mp4Error::InvalidBoxSize);
  }

  let size_usize = usize::try_from(size).map_err(|_| Mp4Error::InvalidBoxSize)?;
  let box_end = start
    .checked_add(size_usize)
    .ok_or(Mp4Error::InvalidBoxSize)?;
  if box_end > end {
    return Err(Mp4Error::InvalidBoxSize);
  }

  let content_start = start
    .checked_add(header_len)
    .ok_or(Mp4Error::InvalidBoxSize)?;
  let content_end = box_end;

  Ok(Some(BoxRef {
    typ,
    content: content_start..content_end,
    end: box_end,
  }))
}

fn find_top_level_box(bytes: &[u8], typ: u32) -> Result<Option<Range<usize>>> {
  let mut cur = Cursor::new(bytes, 0);
  let end = bytes.len();
  let mut deadline_counter = 0usize;
  while let Some(b) = next_box(&mut cur, end)? {
    check_root_periodic(
      &mut deadline_counter,
      MP4_PARSE_DEADLINE_STRIDE,
      RenderStage::Paint,
    )?;
    if b.typ == typ {
      return Ok(Some(b.content));
    }
    cur.pos = b.end;
  }
  Ok(None)
}

fn parse_moov(bytes: &[u8], moov: Range<usize>, limits: &MediaLimits) -> Result<Vec<TrackBoxes>> {
  let mut cur = Cursor::new(bytes, moov.start);
  let mut tracks = Vec::new();
  let mut deadline_counter = 0usize;

  while cur.pos < moov.end {
    check_root_periodic(
      &mut deadline_counter,
      MP4_PARSE_DEADLINE_STRIDE,
      RenderStage::Paint,
    )?;
    let Some(b) = next_box(&mut cur, moov.end)? else {
      break;
    };
    if b.typ == fourcc(b"trak") {
      if tracks.len() >= limits.max_track_count {
        return Err(Mp4Error::TooManyTracks {
          track_count: tracks.len() + 1,
          max: limits.max_track_count,
        });
      }
      tracks.push(parse_trak(bytes, b.content, limits)?);
    }
    cur.pos = b.end;
  }

  Ok(tracks)
}

fn parse_moov_seek(
  bytes: &[u8],
  moov: Range<usize>,
  limits: &MediaLimits,
) -> Result<Vec<SeekTrackBoxes>> {
  let mut cur = Cursor::new(bytes, moov.start);
  let mut tracks = Vec::new();
  let mut deadline_counter = 0usize;

  while cur.pos < moov.end {
    check_root_periodic(
      &mut deadline_counter,
      MP4_PARSE_DEADLINE_STRIDE,
      RenderStage::Paint,
    )?;
    let Some(b) = next_box(&mut cur, moov.end)? else {
      break;
    };
    if b.typ == fourcc(b"trak") {
      if tracks.len() >= limits.max_track_count {
        return Err(Mp4Error::TooManyTracks {
          track_count: tracks.len() + 1,
          max: limits.max_track_count,
        });
      }
      tracks.push(parse_trak_seek(bytes, b.content, limits)?);
    }
    cur.pos = b.end;
  }

  Ok(tracks)
}

fn parse_trak(bytes: &[u8], trak: Range<usize>, limits: &MediaLimits) -> Result<TrackBoxes> {
  let mut cur = Cursor::new(bytes, trak.start);
  let mut t = TrackBoxes::default();
  let mut deadline_counter = 0usize;

  while cur.pos < trak.end {
    check_root_periodic(
      &mut deadline_counter,
      MP4_PARSE_DEADLINE_STRIDE,
      RenderStage::Paint,
    )?;
    let Some(b) = next_box(&mut cur, trak.end)? else {
      break;
    };
    match b.typ {
      typ if typ == fourcc(b"tkhd") => {
        t.id = Some(parse_tkhd(bytes, b.content)?);
      }
      typ if typ == fourcc(b"mdia") => {
        parse_mdia(bytes, b.content, &mut t, limits)?;
      }
      _ => {}
    }
    cur.pos = b.end;
  }

  Ok(t)
}

fn parse_trak_seek(
  bytes: &[u8],
  trak: Range<usize>,
  limits: &MediaLimits,
) -> Result<SeekTrackBoxes> {
  let mut cur = Cursor::new(bytes, trak.start);
  let mut t = SeekTrackBoxes::default();
  let mut deadline_counter = 0usize;

  while cur.pos < trak.end {
    check_root_periodic(
      &mut deadline_counter,
      MP4_PARSE_DEADLINE_STRIDE,
      RenderStage::Paint,
    )?;
    let Some(b) = next_box(&mut cur, trak.end)? else {
      break;
    };
    match b.typ {
      typ if typ == fourcc(b"tkhd") => {
        t.id = Some(parse_tkhd(bytes, b.content)?);
      }
      typ if typ == fourcc(b"mdia") => {
        parse_mdia_seek(bytes, b.content, &mut t, limits)?;
      }
      _ => {}
    }
    cur.pos = b.end;
  }

  Ok(t)
}

fn parse_mdia(bytes: &[u8], mdia: Range<usize>, t: &mut TrackBoxes, limits: &MediaLimits) -> Result<()> {
  let mut cur = Cursor::new(bytes, mdia.start);
  let mut deadline_counter = 0usize;
  while cur.pos < mdia.end {
    check_root_periodic(
      &mut deadline_counter,
      MP4_PARSE_DEADLINE_STRIDE,
      RenderStage::Paint,
    )?;
    let Some(b) = next_box(&mut cur, mdia.end)? else {
      break;
    };
    match b.typ {
      typ if typ == fourcc(b"mdhd") => {
        t.timescale = Some(parse_mdhd(bytes, b.content)?);
      }
      typ if typ == fourcc(b"minf") => {
        parse_minf(bytes, b.content, t, limits)?;
      }
      _ => {}
    }
    cur.pos = b.end;
  }
  Ok(())
}

fn parse_mdia_seek(
  bytes: &[u8],
  mdia: Range<usize>,
  t: &mut SeekTrackBoxes,
  limits: &MediaLimits,
) -> Result<()> {
  let mut cur = Cursor::new(bytes, mdia.start);
  let mut deadline_counter = 0usize;
  while cur.pos < mdia.end {
    check_root_periodic(
      &mut deadline_counter,
      MP4_PARSE_DEADLINE_STRIDE,
      RenderStage::Paint,
    )?;
    let Some(b) = next_box(&mut cur, mdia.end)? else {
      break;
    };
    match b.typ {
      typ if typ == fourcc(b"mdhd") => {
        t.timescale = Some(parse_mdhd(bytes, b.content)?);
      }
      typ if typ == fourcc(b"minf") => {
        parse_minf_seek(bytes, b.content, t, limits)?;
      }
      _ => {}
    }
    cur.pos = b.end;
  }
  Ok(())
}

fn parse_minf(bytes: &[u8], minf: Range<usize>, t: &mut TrackBoxes, limits: &MediaLimits) -> Result<()> {
  let mut cur = Cursor::new(bytes, minf.start);
  let mut deadline_counter = 0usize;
  while cur.pos < minf.end {
    check_root_periodic(
      &mut deadline_counter,
      MP4_PARSE_DEADLINE_STRIDE,
      RenderStage::Paint,
    )?;
    let Some(b) = next_box(&mut cur, minf.end)? else {
      break;
    };
    if b.typ == fourcc(b"stbl") {
      parse_stbl(bytes, b.content, t, limits)?;
    }
    cur.pos = b.end;
  }
  Ok(())
}

fn parse_minf_seek(
  bytes: &[u8],
  minf: Range<usize>,
  t: &mut SeekTrackBoxes,
  limits: &MediaLimits,
) -> Result<()> {
  let mut cur = Cursor::new(bytes, minf.start);
  let mut deadline_counter = 0usize;
  while cur.pos < minf.end {
    check_root_periodic(
      &mut deadline_counter,
      MP4_PARSE_DEADLINE_STRIDE,
      RenderStage::Paint,
    )?;
    let Some(b) = next_box(&mut cur, minf.end)? else {
      break;
    };
    if b.typ == fourcc(b"stbl") {
      parse_stbl_seek(bytes, b.content, t, limits)?;
    }
    cur.pos = b.end;
  }
  Ok(())
}

fn parse_stbl(
  bytes: &[u8],
  stbl: Range<usize>,
  t: &mut TrackBoxes,
  limits: &MediaLimits,
) -> Result<()> {
  let mut cur = Cursor::new(bytes, stbl.start);
  let mut deadline_counter = 0usize;
  while cur.pos < stbl.end {
    check_root_periodic(
      &mut deadline_counter,
      MP4_PARSE_DEADLINE_STRIDE,
      RenderStage::Paint,
    )?;
    let Some(b) = next_box(&mut cur, stbl.end)? else {
      break;
    };
    match b.typ {
      typ if typ == fourcc(b"stts") => {
        t.stts = Some(parse_stts(bytes, b.content, limits)?);
      }
      typ if typ == fourcc(b"ctts") => {
        t.ctts = Some(parse_ctts(bytes, b.content, limits)?);
      }
      typ if typ == fourcc(b"stsc") => {
        t.stsc = Some(parse_stsc(bytes, b.content, limits)?);
      }
      typ if typ == fourcc(b"stsz") => {
        t.stsz = Some(parse_stsz(bytes, b.content, limits)?);
      }
      typ if typ == fourcc(b"stco") => {
        // Prefer co64 if present; otherwise store stco.
        if t.chunk_offsets.is_none() {
          t.chunk_offsets = Some(parse_stco(bytes, b.content, limits)?);
        }
      }
      typ if typ == fourcc(b"co64") => {
        t.chunk_offsets = Some(parse_co64(bytes, b.content, limits)?);
      }
      typ if typ == fourcc(b"stss") => {
        t.stss = Some(parse_stss(bytes, b.content, limits)?);
      }
      _ => {}
    }
    cur.pos = b.end;
  }
  Ok(())
}

fn parse_stbl_seek(
  bytes: &[u8],
  stbl: Range<usize>,
  t: &mut SeekTrackBoxes,
  limits: &MediaLimits,
) -> Result<()> {
  let mut cur = Cursor::new(bytes, stbl.start);
  let mut deadline_counter = 0usize;
  while cur.pos < stbl.end {
    check_root_periodic(
      &mut deadline_counter,
      MP4_PARSE_DEADLINE_STRIDE,
      RenderStage::Paint,
    )?;
    let Some(b) = next_box(&mut cur, stbl.end)? else {
      break;
    };
    match b.typ {
      typ if typ == fourcc(b"stts") => {
        t.stts = Some(parse_stts(bytes, b.content, limits)?);
      }
      typ if typ == fourcc(b"ctts") => {
        t.ctts = Some(parse_ctts(bytes, b.content, limits)?);
      }
      _ => {}
    }
    cur.pos = b.end;
  }
  Ok(())
}

fn read_fullbox_version(cur: &mut Cursor<'_>, end: usize) -> Result<u8> {
  let version = cur.read_u8(end)?;
  cur.skip(end, 3)?; // flags
  Ok(version)
}

fn parse_mdhd(bytes: &[u8], mdhd: Range<usize>) -> Result<u32> {
  let mut cur = Cursor::new(bytes, mdhd.start);
  let version = read_fullbox_version(&mut cur, mdhd.end)?;

  match version {
    0 => {
      cur.skip(mdhd.end, 8)?; // creation + modification
      let timescale = cur.read_u32(mdhd.end)?;
      Ok(timescale)
    }
    1 => {
      cur.skip(mdhd.end, 16)?; // creation + modification
      let timescale = cur.read_u32(mdhd.end)?;
      Ok(timescale)
    }
    v => Err(Mp4Error::UnsupportedBoxVersion {
      box_name: "mdhd",
      version: v,
    }),
  }
}

fn parse_tkhd(bytes: &[u8], tkhd: Range<usize>) -> Result<u32> {
  let mut cur = Cursor::new(bytes, tkhd.start);
  let version = read_fullbox_version(&mut cur, tkhd.end)?;

  let track_id = match version {
    0 => {
      cur.skip(tkhd.end, 8)?; // creation + modification
      cur.read_u32(tkhd.end)?
    }
    1 => {
      cur.skip(tkhd.end, 16)?; // creation + modification
      cur.read_u32(tkhd.end)?
    }
    v => {
      return Err(Mp4Error::UnsupportedBoxVersion {
        box_name: "tkhd",
        version: v,
      })
    }
  };

  if track_id == 0 {
    return Err(Mp4Error::Invalid("tkhd track_id must be > 0"));
  }

  Ok(track_id)
}

fn checked_entry_count(box_name: &'static str, entry_count: u32, limits: &MediaLimits) -> Result<usize> {
  let max_by_limits =
    u32::try_from(limits.max_samples_per_track).unwrap_or(MAX_TABLE_ENTRIES);
  let max = MAX_TABLE_ENTRIES.min(max_by_limits);
  if entry_count > max {
    return Err(Mp4Error::TooManyTableEntries {
      box_name,
      entry_count: u64::from(entry_count),
      max: u64::from(max),
    });
  }
  Ok(entry_count as usize)
}

fn parse_stts(bytes: &[u8], stts: Range<usize>, limits: &MediaLimits) -> Result<Vec<SttsEntry>> {
  let mut cur = Cursor::new(bytes, stts.start);
  let version = read_fullbox_version(&mut cur, stts.end)?;
  if version != 0 {
    return Err(Mp4Error::UnsupportedBoxVersion {
      box_name: "stts",
      version,
    });
  }

  let entry_count = cur.read_u32(stts.end)?;
  let entry_count = checked_entry_count("stts", entry_count, limits)?;
  let remaining_bytes = stts.end.saturating_sub(cur.pos);
  let max_entries_by_bytes = remaining_bytes / 8;
  if entry_count > max_entries_by_bytes {
    return Err(Mp4Error::UnexpectedEof);
  }
  let mut out = Vec::with_capacity(entry_count);
  let mut deadline_counter = 0usize;
  for _ in 0..entry_count {
    check_root_periodic(
      &mut deadline_counter,
      MP4_PARSE_DEADLINE_STRIDE,
      RenderStage::Paint,
    )?;
    let sample_count = cur.read_u32(stts.end)?;
    let sample_delta = cur.read_u32(stts.end)?;
    out.push(SttsEntry {
      sample_count,
      sample_delta,
    });
  }
  Ok(out)
}

fn parse_ctts(bytes: &[u8], ctts: Range<usize>, limits: &MediaLimits) -> Result<Vec<CttsEntry>> {
  let mut cur = Cursor::new(bytes, ctts.start);
  let version = read_fullbox_version(&mut cur, ctts.end)?;
  if version != 0 && version != 1 {
    return Err(Mp4Error::UnsupportedBoxVersion {
      box_name: "ctts",
      version,
    });
  }

  let entry_count = cur.read_u32(ctts.end)?;
  let entry_count = checked_entry_count("ctts", entry_count, limits)?;
  let remaining_bytes = ctts.end.saturating_sub(cur.pos);
  let max_entries_by_bytes = remaining_bytes / 8;
  if entry_count > max_entries_by_bytes {
    return Err(Mp4Error::UnexpectedEof);
  }
  let mut out = Vec::with_capacity(entry_count);
  let mut deadline_counter = 0usize;
  for _ in 0..entry_count {
    check_root_periodic(
      &mut deadline_counter,
      MP4_PARSE_DEADLINE_STRIDE,
      RenderStage::Paint,
    )?;
    let sample_count = cur.read_u32(ctts.end)?;
    let sample_offset = if version == 0 {
      i64::from(cur.read_u32(ctts.end)?)
    } else {
      i64::from(cur.read_i32(ctts.end)?)
    };
    out.push(CttsEntry {
      sample_count,
      sample_offset,
    });
  }
  Ok(out)
}

fn parse_stsc(bytes: &[u8], stsc: Range<usize>, limits: &MediaLimits) -> Result<Vec<StscEntry>> {
  let mut cur = Cursor::new(bytes, stsc.start);
  let version = read_fullbox_version(&mut cur, stsc.end)?;
  if version != 0 {
    return Err(Mp4Error::UnsupportedBoxVersion {
      box_name: "stsc",
      version,
    });
  }

  let entry_count = cur.read_u32(stsc.end)?;
  let entry_count = checked_entry_count("stsc", entry_count, limits)?;
  let remaining_bytes = stsc.end.saturating_sub(cur.pos);
  let max_entries_by_bytes = remaining_bytes / 12;
  if entry_count > max_entries_by_bytes {
    return Err(Mp4Error::UnexpectedEof);
  }
  let mut out = Vec::with_capacity(entry_count);
  let mut deadline_counter = 0usize;
  for _ in 0..entry_count {
    check_root_periodic(
      &mut deadline_counter,
      MP4_PARSE_DEADLINE_STRIDE,
      RenderStage::Paint,
    )?;
    let first_chunk = cur.read_u32(stsc.end)?;
    let samples_per_chunk = cur.read_u32(stsc.end)?;
    let _sample_desc = cur.read_u32(stsc.end)?;
    out.push(StscEntry {
      first_chunk,
      samples_per_chunk,
    });
  }
  if out.is_empty() {
    return Err(Mp4Error::Invalid("stsc must have at least one entry"));
  }
  Ok(out)
}

fn parse_stsz(bytes: &[u8], stsz: Range<usize>, limits: &MediaLimits) -> Result<StszBox> {
  let mut cur = Cursor::new(bytes, stsz.start);
  let version = read_fullbox_version(&mut cur, stsz.end)?;
  if version != 0 {
    return Err(Mp4Error::UnsupportedBoxVersion {
      box_name: "stsz",
      version,
    });
  }

  let sample_size = cur.read_u32(stsz.end)?;
  let sample_count = cur.read_u32(stsz.end)?;

  let mut sample_sizes = Vec::new();
  if u64::from(sample_count) > limits.max_samples_per_track as u64 {
    return Err(Mp4Error::TooManySamples {
      sample_count: u64::from(sample_count),
      max: limits.max_samples_per_track as u64,
    });
  }
  if sample_size != 0 && sample_size as usize > limits.max_packet_bytes {
    return Err(Mp4Error::SampleTooLarge {
      size: sample_size,
      max: limits.max_packet_bytes,
    });
  }
  if sample_size == 0 {
    let sample_count_usize = sample_count as usize;
    let remaining_bytes = stsz.end.saturating_sub(cur.pos);
    let max_entries_by_bytes = remaining_bytes / 4;
    if sample_count_usize > max_entries_by_bytes {
      return Err(Mp4Error::UnexpectedEof);
    }

    sample_sizes = Vec::with_capacity(sample_count_usize);
    let mut deadline_counter = 0usize;
    for _ in 0..sample_count {
      check_root_periodic(
        &mut deadline_counter,
        MP4_PARSE_DEADLINE_STRIDE,
        RenderStage::Paint,
      )?;
      let size = cur.read_u32(stsz.end)?;
      if size as usize > limits.max_packet_bytes {
        return Err(Mp4Error::SampleTooLarge {
          size,
          max: limits.max_packet_bytes,
        });
      }
      sample_sizes.push(size);
    }
  }

  Ok(StszBox {
    sample_size,
    sample_sizes,
    sample_count,
  })
}

fn parse_stco(bytes: &[u8], stco: Range<usize>, limits: &MediaLimits) -> Result<Vec<u64>> {
  let mut cur = Cursor::new(bytes, stco.start);
  let version = read_fullbox_version(&mut cur, stco.end)?;
  if version != 0 {
    return Err(Mp4Error::UnsupportedBoxVersion {
      box_name: "stco",
      version,
    });
  }

  let entry_count = cur.read_u32(stco.end)?;
  let entry_count = checked_entry_count("stco", entry_count, limits)?;
  let remaining_bytes = stco.end.saturating_sub(cur.pos);
  let max_entries_by_bytes = remaining_bytes / 4;
  if entry_count > max_entries_by_bytes {
    return Err(Mp4Error::UnexpectedEof);
  }
  let mut out = Vec::with_capacity(entry_count);
  let mut deadline_counter = 0usize;
  for _ in 0..entry_count {
    check_root_periodic(
      &mut deadline_counter,
      MP4_PARSE_DEADLINE_STRIDE,
      RenderStage::Paint,
    )?;
    out.push(u64::from(cur.read_u32(stco.end)?));
  }
  Ok(out)
}

fn parse_co64(bytes: &[u8], co64: Range<usize>, limits: &MediaLimits) -> Result<Vec<u64>> {
  let mut cur = Cursor::new(bytes, co64.start);
  let version = read_fullbox_version(&mut cur, co64.end)?;
  if version != 0 {
    return Err(Mp4Error::UnsupportedBoxVersion {
      box_name: "co64",
      version,
    });
  }

  let entry_count = cur.read_u32(co64.end)?;
  let entry_count = checked_entry_count("co64", entry_count, limits)?;
  let remaining_bytes = co64.end.saturating_sub(cur.pos);
  let max_entries_by_bytes = remaining_bytes / 8;
  if entry_count > max_entries_by_bytes {
    return Err(Mp4Error::UnexpectedEof);
  }
  let mut out = Vec::with_capacity(entry_count);
  let mut deadline_counter = 0usize;
  for _ in 0..entry_count {
    check_root_periodic(
      &mut deadline_counter,
      MP4_PARSE_DEADLINE_STRIDE,
      RenderStage::Paint,
    )?;
    out.push(cur.read_u64(co64.end)?);
  }
  Ok(out)
}

fn parse_stss(bytes: &[u8], stss: Range<usize>, limits: &MediaLimits) -> Result<Vec<u32>> {
  let mut cur = Cursor::new(bytes, stss.start);
  let version = read_fullbox_version(&mut cur, stss.end)?;
  if version != 0 {
    return Err(Mp4Error::UnsupportedBoxVersion {
      box_name: "stss",
      version,
    });
  }

  let entry_count = cur.read_u32(stss.end)?;
  let entry_count = checked_entry_count("stss", entry_count, limits)?;
  let remaining_bytes = stss.end.saturating_sub(cur.pos);
  let max_entries_by_bytes = remaining_bytes / 4;
  if entry_count > max_entries_by_bytes {
    return Err(Mp4Error::UnexpectedEof);
  }
  let mut out = Vec::with_capacity(entry_count);
  let mut deadline_counter = 0usize;
  for _ in 0..entry_count {
    check_root_periodic(
      &mut deadline_counter,
      MP4_PARSE_DEADLINE_STRIDE,
      RenderStage::Paint,
    )?;
    out.push(cur.read_u32(stss.end)?);
  }
  Ok(out)
}

#[derive(Debug)]
struct TableRunIter<'a> {
  // Common representation for both stts and ctts (u32 deltas / i64 offsets).
  stts: Option<&'a [SttsEntry]>,
  ctts: Option<&'a [CttsEntry]>,
  idx: usize,
  remaining: u32,
  cur_u32: u32,
  cur_i64: i64,
}

impl<'a> TableRunIter<'a> {
  fn new_stts(entries: &'a [SttsEntry]) -> Self {
    Self {
      stts: Some(entries),
      ctts: None,
      idx: 0,
      remaining: 0,
      cur_u32: 0,
      cur_i64: 0,
    }
  }

  fn new_ctts(entries: &'a [CttsEntry]) -> Self {
    Self {
      stts: None,
      ctts: Some(entries),
      idx: 0,
      remaining: 0,
      cur_u32: 0,
      cur_i64: 0,
    }
  }

  fn next_u32(&mut self) -> Option<u32> {
    let entries = self.stts?;
    if self.remaining == 0 {
      let e = entries.get(self.idx)?;
      self.idx += 1;
      self.remaining = e.sample_count;
      self.cur_u32 = e.sample_delta;
    }
    self.remaining -= 1;
    Some(self.cur_u32)
  }

  fn next_i64(&mut self) -> Option<i64> {
    let entries = self.ctts?;
    if entries.is_empty() {
      return Some(0);
    }
    if self.remaining == 0 {
      let e = entries.get(self.idx)?;
      self.idx += 1;
      self.remaining = e.sample_count;
      self.cur_i64 = e.sample_offset;
    }
    self.remaining -= 1;
    Some(self.cur_i64)
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use std::sync::Arc;

  #[test]
  fn seek_uses_binary_search_when_pts_monotonic() {
    let bytes = include_bytes!("../../tests/fixtures/media/test_h264_aac.mp4");
    let mut demuxer = Mp4Demuxer::new(bytes).expect("parse mp4");

    assert!(
      demuxer.tracks().len() >= 2,
      "fixture should contain at least audio+video tracks"
    );

    // The fixture is constrained-baseline H.264 + AAC and is generated without B-frames, so PTS is
    // monotonic in decode order for both tracks.
    for (i, track) in demuxer.tracks.iter().enumerate() {
      assert!(
        matches!(track.pts_index, PtsIndex::Monotonic),
        "track {i} should have monotonic pts in fixture"
      );
    }

    demuxer.seek(1_000_000_000); // 1s

    for (i, track) in demuxer.tracks.iter().enumerate() {
      assert_eq!(
        track.last_seek_method,
        Some(SeekMethod::MonotonicBinarySearch),
        "track {i} seek should use monotonic binary search"
      );
      assert!(
        track.next_sample <= track.samples.len(),
        "track {i} next_sample must be in-bounds"
      );
    }
  }

  #[test]
  fn seek_uses_sorted_index_when_pts_non_monotonic() {
    // Synthetic "B-frame" style non-monotonic PTS in decode order.
    //
    // Decode order sample indices: 0, 1, 2
    // PTS order: 0ns (0), 1000ns (2), 2000ns (1)
    let pts_ns_by_sample = vec![0_u64, 2_000, 1_000];
    let pts_index = build_pts_index(&pts_ns_by_sample).unwrap();
    assert!(
      matches!(pts_index, PtsIndex::Sorted { .. }),
      "non-monotonic PTS must build a sorted seek index"
    );

    let samples = vec![
      Mp4Sample {
        offset: 0,
        size: 0,
        dts_ticks: 0,
        duration_ticks: 0,
        is_sync: true,
      },
      Mp4Sample {
        offset: 0,
        size: 0,
        dts_ticks: 0,
        duration_ticks: 0,
        is_sync: true,
      },
      Mp4Sample {
        offset: 0,
        size: 0,
        dts_ticks: 0,
        duration_ticks: 0,
        is_sync: true,
      },
    ];

    let mut track = Mp4Track {
      id: 1,
      timescale: 1,
      samples,
      pts_ns_by_sample,
      pts_index,
      next_sample: 0,
      last_seek_method: None,
    };

    // Even though sample index 2 has the smallest PTS >= 500ns, seeking should return the *first*
    // decode-order sample whose PTS is >= target (index 1). This avoids jumping directly to a
    // B-frame that depends on earlier reference frames.
    track.seek(500);
    assert_eq!(
      track.last_seek_method(),
      Some(SeekMethod::SortedBinarySearch)
    );
    assert_eq!(
      track.next_sample(),
      1,
      "seek should choose the first decode-order sample with PTS >= target"
    );
  }

  #[test]
  fn seek_track_normalizes_negative_pts_ticks() {
    // PTS ticks derived from `dts + ctts`:
    // dts: 0, 1, 2, 3
    // ctts: -3, -1, 0, 1
    // pts: -3, 0, 2, 4  => normalize by +3 => 0, 3, 5, 7
    let limits = MediaLimits::default();
    let track = build_seek_track(
      SeekTrackBoxes {
        id: Some(1),
        timescale: Some(1),
        stts: Some(vec![SttsEntry {
          sample_count: 4,
        sample_delta: 1,
      }]),
      ctts: Some(vec![
        CttsEntry {
          sample_count: 1,
          sample_offset: -3,
        },
        CttsEntry {
          sample_count: 1,
          sample_offset: -1,
        },
        CttsEntry {
          sample_count: 1,
          sample_offset: 0,
        },
        CttsEntry {
          sample_count: 1,
          sample_offset: 1,
        },
      ]),
      },
      &limits,
    )
    .expect("build_seek_track");

    assert_eq!(
      track.pts_ns_by_sample,
      vec![0, 3_000_000_000, 5_000_000_000, 7_000_000_000]
    );
    assert!(matches!(track.pts_index, PtsIndex::Monotonic));
    assert_eq!(track.sample_index_at_or_after(0), 0);
    assert_eq!(track.sample_index_at_or_after(1), 1);
    assert_eq!(track.sample_index_at_or_after(3_000_000_000), 1);
  }

  #[test]
  fn mp4_video_packets_are_emitted_in_decode_order_even_when_pts_goes_backwards() {
    use mp4parse::unstable::CheckedInteger;

    // Sample index order (decode order): A, B, C.
    //
    // Presentation order: A, C, B (B-frame causes PTS to go backwards).
    let sample_table = vec![
      Indice {
        start_offset: CheckedInteger(0u64),
        end_offset: CheckedInteger(1u64),
        start_decode: CheckedInteger(0i64),
        start_composition: CheckedInteger(0i64),
        end_composition: CheckedInteger(1i64),
        sync: true,
        ..Default::default()
      },
      Indice {
        start_offset: CheckedInteger(1u64),
        end_offset: CheckedInteger(2u64),
        start_decode: CheckedInteger(1i64),
        start_composition: CheckedInteger(2i64),
        end_composition: CheckedInteger(3i64),
        sync: false,
        ..Default::default()
      },
      Indice {
        start_offset: CheckedInteger(2u64),
        end_offset: CheckedInteger(3u64),
        start_decode: CheckedInteger(2i64),
        start_composition: CheckedInteger(1i64),
        end_composition: CheckedInteger(2i64),
        sync: false,
        ..Default::default()
      },
    ];

    let reader = std::io::Cursor::new(vec![b'A', b'B', b'C']);
    let mut demuxer = Mp4TrackDemuxer::new(reader, sample_table, 1, 1, MediaTrackType::Video);

    let mut packets = Vec::new();
    while let Some(packet) = demuxer.next_packet().unwrap() {
      packets.push(packet);
    }

    assert_eq!(packets.len(), 3);

    // Emitted in decode/sample index order (not PTS order).
    assert_eq!(
      packets.iter().map(|p| p.as_slice()).collect::<Vec<_>>(),
      vec![b"A".as_slice(), b"B".as_slice(), b"C".as_slice()]
    );

    // DTS is derived from `Indice::start_decode`.
    assert_eq!(
      packets.iter().map(|p| p.dts_ns).collect::<Vec<_>>(),
      vec![0, 1_000_000_000, 2_000_000_000]
    );

    // PTS is derived from `Indice::start_composition` and can be non-monotonic for video.
    assert_eq!(
      packets.iter().map(|p| p.pts_ns).collect::<Vec<_>>(),
      vec![0, 2_000_000_000, 1_000_000_000]
    );
    assert!(packets[2].pts_ns < packets[1].pts_ns);
  }

  #[test]
  fn mp4_track_demuxer_normalizes_negative_composition_time() {
    use mp4parse::unstable::CheckedInteger;

    let sample_table = vec![
      Indice {
        start_offset: CheckedInteger(0u64),
        end_offset: CheckedInteger(1u64),
        start_decode: CheckedInteger(0i64),
        start_composition: CheckedInteger(-1i64),
        end_composition: CheckedInteger(0i64),
        sync: true,
        ..Default::default()
      },
      Indice {
        start_offset: CheckedInteger(1u64),
        end_offset: CheckedInteger(2u64),
        start_decode: CheckedInteger(1i64),
        start_composition: CheckedInteger(0i64),
        end_composition: CheckedInteger(1i64),
        sync: true,
        ..Default::default()
      },
    ];

    let reader = std::io::Cursor::new(vec![b'A', b'B']);
    let mut demuxer = Mp4TrackDemuxer::new(reader, sample_table, 1, 1, MediaTrackType::Video);

    let p0 = demuxer.next_packet().unwrap().unwrap();
    let p1 = demuxer.next_packet().unwrap().unwrap();
    assert_eq!(p0.pts_ns, 0);
    assert_eq!(p1.pts_ns, 1_000_000_000);
  }

  #[test]
  fn mp4_packets_use_shared_data_and_ranges_match_sample_table() {
    let bytes: &[u8] = include_bytes!("../../tests/fixtures/media/test_h264_aac.mp4");
    let arc: Arc<[u8]> = Arc::from(bytes);
    let demuxer = Mp4Demuxer::from_arc(Arc::clone(&arc)).expect("parse mp4");

    for (track_index, track) in demuxer.tracks().iter().enumerate() {
      let packets = demuxer
        .packets_for_track(track_index)
        .expect("packets for track");
      assert_eq!(
        packets.len(),
        track.samples.len(),
        "track {track_index} packet count should match sample table"
      );

      for (sample_index, (packet, sample)) in packets.iter().zip(track.samples.iter()).enumerate() {
        let start = usize::try_from(sample.offset).unwrap();
        let end = start + sample.size as usize;
        let expected = start..end;

        assert_eq!(packet.track_id, u64::from(track.id()));
        assert_eq!(
          packet.dts_ns,
          ticks_to_ns(
            i64::try_from(sample.dts_ticks).unwrap_or(i64::MAX),
            track.timescale()
          )
        );
        assert_eq!(packet.is_keyframe, sample.is_sync);

        match &packet.data {
          MediaData::Shared { bytes, range } => {
            assert!(Arc::ptr_eq(bytes, &arc));
            assert_eq!(
              range, &expected,
              "track {track_index} sample {sample_index} range mismatch"
            );
            assert_eq!(packet.as_slice(), &arc[expected]);
          }
          other => panic!("expected Shared packet data, got {other:?}"),
        }
      }
    }
  }

  #[test]
  fn mp4_parser_does_not_panic_on_truncated_buffers() {
    // These intentionally truncated buffers historically exercised unchecked slice reads in the
    // MP4 cursor helpers.
    for bytes in [
      &b""[..],
      &b"\0"[..],
      &b"\0\0\0\0"[..],
      // Incomplete box header (< 8 bytes).
      &b"\0\0\0\0\0\0\0"[..],
      // A header that claims a larger size than the available bytes.
      &b"\0\0\0\x10moov\0\0\0\0"[..],
      // Extended size marker (size32 == 1) without enough bytes for the 64-bit size.
      &b"\0\0\0\x01moov\0\0\0\0"[..],
    ] {
      let demux = std::panic::catch_unwind(|| Mp4Demuxer::new(bytes));
      assert!(
        demux.is_ok(),
        "Mp4Demuxer::new panicked for len={}",
        bytes.len()
      );
      assert!(demux.unwrap().is_err());

      let index = std::panic::catch_unwind(|| Mp4SeekIndex::from_bytes(bytes));
      assert!(
        index.is_ok(),
        "Mp4SeekIndex::from_bytes panicked for len={}",
        bytes.len()
      );
      assert!(index.unwrap().is_err());
    }
  }

  #[test]
  fn seek_track_rejects_excessive_sample_counts() {
    let mut limits = MediaLimits::default();
    limits.max_samples_per_track = 10;
    let stts = vec![SttsEntry {
      sample_count: u32::try_from(limits.max_samples_per_track)
        .unwrap_or(u32::MAX)
        .saturating_add(1),
      sample_delta: 1,
    }];
    let t = SeekTrackBoxes {
      id: Some(1),
      timescale: Some(1),
      stts: Some(stts),
      ctts: None,
    };

    match build_seek_track(t, &limits) {
      Err(Mp4Error::TooManySamples { sample_count, max }) => {
        assert!(sample_count > max);
      }
      other => panic!("expected TooManySamples error, got {other:?}"),
    }
  }

  #[test]
  fn mp4_table_parsers_reject_excessive_counts_without_panicking_or_allocating() {
    let limits = MediaLimits::default();

    // A tiny `stts` box that claims an absurd entry_count. Historically this would attempt to
    // allocate an enormous vector (or panic/abort) before failing with EOF.
    let stts = [0_u8, 0, 0, 0, 0xff, 0xff, 0xff, 0xff];
    let r = std::panic::catch_unwind(|| parse_stts(&stts, 0..stts.len(), &limits));
    assert!(matches!(
      r,
      Ok(Err(Mp4Error::TooManyTableEntries { box_name: "stts", .. }))
    ));

    // A `stsz` box with `sample_size == 0` (meaning per-sample sizes are present) but a gigantic
    // sample_count. The parser should reject this up front to avoid OOM.
    let mut stsz = Vec::new();
    stsz.extend_from_slice(&[0_u8, 0, 0, 0]); // version + flags
    stsz.extend_from_slice(&0_u32.to_be_bytes()); // sample_size
    stsz.extend_from_slice(&u32::MAX.to_be_bytes()); // sample_count
    let r = std::panic::catch_unwind(|| parse_stsz(&stsz, 0..stsz.len(), &limits));
    assert!(matches!(r, Ok(Err(Mp4Error::TooManySamples { .. }))));
  }

  #[test]
  fn cursor_reads_are_bounds_checked_against_buffer_length() {
    // Simulate a truncated buffer where the caller's `end` exceeds the available bytes.
    let data = [0_u8];

    let r = std::panic::catch_unwind(|| {
      let mut cur = Cursor::new(&data, 0);
      cur.read_u16(2)
    });
    assert!(matches!(r, Ok(Err(Mp4Error::UnexpectedEof))));

    let r = std::panic::catch_unwind(|| {
      let mut cur = Cursor::new(&data, 0);
      cur.read_u32(4)
    });
    assert!(matches!(r, Ok(Err(Mp4Error::UnexpectedEof))));

    let r = std::panic::catch_unwind(|| {
      let mut cur = Cursor::new(&data, 0);
      cur.read_u64(8)
    });
    assert!(matches!(r, Ok(Err(Mp4Error::UnexpectedEof))));
  }

  #[test]
  fn parse_stsz_rejects_excessive_sample_count_without_reading_entries() {
    let mut limits = MediaLimits::default();
    limits.max_samples_per_track = 10;

    // stsz (fullbox) content:
    // version+flags (4 bytes), sample_size (4 bytes), sample_count (4 bytes)
    //
    // We intentionally do not include per-sample sizes in the payload; the parser must reject the
    // sample_count before trying to allocate/read the table.
    let sample_count = u32::try_from(limits.max_samples_per_track)
      .unwrap_or(u32::MAX)
      .saturating_add(1);
    let mut bytes = Vec::new();
    bytes.extend_from_slice(&[0, 0, 0, 0]); // version 0 + flags
    bytes.extend_from_slice(&0u32.to_be_bytes()); // sample_size = 0 => would normally read table
    bytes.extend_from_slice(&sample_count.to_be_bytes());

    match parse_stsz(&bytes, 0..bytes.len(), &limits) {
      Err(Mp4Error::TooManySamples {
        sample_count: got,
        max,
      }) => {
        assert_eq!(got, u64::from(sample_count));
        assert_eq!(max, limits.max_samples_per_track as u64);
      }
      other => panic!("expected TooManySamples error, got {other:?}"),
    }
  }

  #[test]
  fn mp4_track_demuxer_rejects_excessive_packet_size_before_io() {
    use mp4parse::unstable::CheckedInteger;

    #[derive(Debug)]
    struct PanicReader;

    impl Read for PanicReader {
      fn read(&mut self, _buf: &mut [u8]) -> std::io::Result<usize> {
        panic!("read should not be called when sample size exceeds MAX_MP4_PACKET_BYTES");
      }
    }

    impl Seek for PanicReader {
      fn seek(&mut self, _pos: SeekFrom) -> std::io::Result<u64> {
        panic!("seek should not be called when sample size exceeds MAX_MP4_PACKET_BYTES");
      }
    }

    let sample_len = MAX_MP4_PACKET_BYTES + 1;
    let sample_table = vec![Indice {
      start_offset: CheckedInteger(0u64),
      end_offset: CheckedInteger(sample_len),
      start_decode: CheckedInteger(0i64),
      start_composition: CheckedInteger(0i64),
      end_composition: CheckedInteger(1i64),
      sync: true,
      ..Default::default()
    }];

    let mut demuxer =
      Mp4TrackDemuxer::new(PanicReader, sample_table, 1, 1, MediaTrackType::Video);
    match demuxer.next_packet() {
      Err(MediaError::Demux(msg)) => {
        assert!(
          msg.contains(&sample_len.to_string()),
          "error message should include sample length, got: {msg}"
        );
        assert!(
          msg.contains(&MAX_MP4_PACKET_BYTES.to_string()),
          "error message should include cap, got: {msg}"
        );
      }
      other => panic!("expected MediaError::Demux for oversized sample, got {other:?}"),
    }
  }
}
