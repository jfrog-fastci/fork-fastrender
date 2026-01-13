//! Minimal MP4 (ISO BMFF) demuxer utilities.
//!
//! The initial focus is *time-based seeking* by presentation timestamp (PTS).
//! MP4 sample tables can be large (millions of samples), so `seek(time_ns)` must avoid linear
//! scans for common files.
//!
//! This module intentionally implements only the subset of ISO BMFF needed for unit tests and
//! basic MP4 playback plumbing (non-fragmented `moov`/`mdat` files).

use std::ops::Range;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum Mp4Error {
  #[error("unexpected end of file")]
  UnexpectedEof,
  #[error("invalid mp4 box size")]
  InvalidBoxSize,
  #[error("invalid mp4: {0}")]
  Invalid(&'static str),
  #[error("unsupported mp4 box version {version} for {box_name}")]
  UnsupportedBoxVersion { box_name: &'static str, version: u8 },
  #[error("missing required mp4 box: {0}")]
  MissingBox(&'static str),
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
        let pos = sample_indices_by_pts
          .partition_point(|&i| self.pts_ns_by_sample[i as usize] < time_ns);
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
  tracks: Vec<Mp4Track>,
}

impl Mp4Demuxer {
  pub fn new(bytes: &[u8]) -> Result<Self> {
    Self::from_bytes(bytes)
  }

  pub fn from_bytes(bytes: &[u8]) -> Result<Self> {
    let moov = find_top_level_box(bytes, fourcc(b"moov"))?.ok_or(Mp4Error::MissingBox("moov"))?;

    let tracks = parse_moov(bytes, moov)?
      .into_iter()
      .map(build_track)
      .collect::<Result<Vec<_>>>()?;

    if tracks.is_empty() {
      return Err(Mp4Error::MissingBox("trak"));
    }

    Ok(Self { tracks })
  }

  #[must_use]
  pub fn tracks(&self) -> &[Mp4Track] {
    &self.tracks
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
    let moov = find_top_level_box(bytes, fourcc(b"moov"))?.ok_or(Mp4Error::MissingBox("moov"))?;

    let tracks = parse_moov_seek(bytes, moov)?
      .into_iter()
      .map(build_seek_track)
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
        let pos = sample_indices_by_pts
          .partition_point(|&i| self.pts_ns_by_sample[i as usize] < time_ns);
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

fn build_track(t: TrackBoxes) -> Result<Mp4Track> {
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

  for (chunk_idx0, &chunk_base) in chunk_offsets.iter().enumerate() {
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
      offset = offset
        .checked_add(u64::from(size))
        .ok_or(Mp4Error::Invalid("sample offset overflow"))?;

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

  // Fill timing and build PTS index vectors.
  let mut stts_iter = TableRunIter::new_stts(&stts);
  let mut ctts_iter = TableRunIter::new_ctts(&ctts);
  let has_ctts = !ctts.is_empty();

  let mut dts_ticks: i64 = 0;
  let mut pts_ns_by_sample = Vec::with_capacity(sample_count);
  let mut pts_is_monotonic = true;
  let mut prev_pts_ns = 0_u64;
  let mut saw_prev_pts = false;

  for sample in &mut samples {
    let dur = stts_iter
      .next_u32()
      .ok_or(Mp4Error::Invalid("stts shorter than sample_count"))?;

    let ctts_off = if has_ctts {
      ctts_iter
        .next_i64()
        .ok_or(Mp4Error::Invalid("ctts shorter than sample_count"))?
    } else {
      0
    };

    sample.dts_ticks = dts_ticks.max(0) as u64;
    sample.duration_ticks = dur;

    let pts_ticks = dts_ticks.saturating_add(ctts_off);
    let pts_ns = ticks_to_ns(pts_ticks, timescale);
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
    build_sorted_pts_index(&pts_ns_by_sample)
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

fn build_seek_track(t: SeekTrackBoxes) -> Result<Mp4SeekTrack> {
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

  if has_ctts {
    let ctts_total: u64 = ctts.iter().map(|e| u64::from(e.sample_count)).sum();
    if ctts_total != stts_total {
      return Err(Mp4Error::Invalid("ctts sample_count sum mismatch"));
    }
  }

  let sample_count = usize::try_from(stts_total).map_err(|_| Mp4Error::Invalid("sample_count overflow"))?;

  let mut stts_iter = TableRunIter::new_stts(&stts);
  let mut ctts_iter = TableRunIter::new_ctts(&ctts);

  let mut dts_ticks: i64 = 0;
  let mut pts_ns_by_sample = Vec::with_capacity(sample_count);
  let mut pts_is_monotonic = true;
  let mut prev_pts_ns = 0_u64;
  let mut saw_prev_pts = false;

  for _ in 0..sample_count {
    let dur = stts_iter
      .next_u32()
      .ok_or(Mp4Error::Invalid("stts shorter than sample_count"))?;

    let ctts_off = if has_ctts {
      ctts_iter
        .next_i64()
        .ok_or(Mp4Error::Invalid("ctts shorter than sample_count"))?
    } else {
      0
    };

    let pts_ticks = dts_ticks.saturating_add(ctts_off);
    let pts_ns = ticks_to_ns(pts_ticks, timescale);
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
    build_sorted_pts_index(&pts_ns_by_sample)
  };

  Ok(Mp4SeekTrack {
    id,
    timescale,
    pts_ns_by_sample,
    pts_index,
  })
}

fn build_pts_index(pts_ns_by_sample: &[u64]) -> PtsIndex {
  // Helper used in tests; production code does the monotonic check while building the table.
  if pts_ns_by_sample
    .windows(2)
    .all(|pair| pair[0] <= pair[1])
  {
    return PtsIndex::Monotonic;
  }
  build_sorted_pts_index(pts_ns_by_sample)
}

fn build_sorted_pts_index(pts_ns_by_sample: &[u64]) -> PtsIndex {
  // Non-monotonic (e.g. B-frames / CTTS reordering). Build a sorted index.
  let mut sample_indices_by_pts = Vec::with_capacity(pts_ns_by_sample.len());
  for i in 0..pts_ns_by_sample.len() {
    // `stsz.sample_count` is a u32, so sample tables should always fit.
    sample_indices_by_pts.push(i as u32);
  }
  // We don't require stable ordering because we include the sample index as a tiebreaker (unique
  // key), so `sort_unstable_by_key` avoids the extra scratch allocations of the stable sort.
  sample_indices_by_pts.sort_unstable_by_key(|&i| (pts_ns_by_sample[i as usize], i));

  // Precompute suffix minima so seeking can return the first decode-order sample index with PTS >=
  // target without scanning the remainder of the list.
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
    if self.pos + 1 > end {
      return Err(Mp4Error::UnexpectedEof);
    }
    let v = self.data[self.pos];
    self.pos += 1;
    Ok(v)
  }

  fn read_u16(&mut self, end: usize) -> Result<u16> {
    if self.pos + 2 > end {
      return Err(Mp4Error::UnexpectedEof);
    }
    let v = u16::from_be_bytes(self.data[self.pos..self.pos + 2].try_into().unwrap());
    self.pos += 2;
    Ok(v)
  }

  fn read_u32(&mut self, end: usize) -> Result<u32> {
    if self.pos + 4 > end {
      return Err(Mp4Error::UnexpectedEof);
    }
    let v = u32::from_be_bytes(self.data[self.pos..self.pos + 4].try_into().unwrap());
    self.pos += 4;
    Ok(v)
  }

  fn read_u64(&mut self, end: usize) -> Result<u64> {
    if self.pos + 8 > end {
      return Err(Mp4Error::UnexpectedEof);
    }
    let v = u64::from_be_bytes(self.data[self.pos..self.pos + 8].try_into().unwrap());
    self.pos += 8;
    Ok(v)
  }

  fn read_i32(&mut self, end: usize) -> Result<i32> {
    let v = self.read_u32(end)?;
    Ok(i32::from_be_bytes(v.to_be_bytes()))
  }

  fn skip(&mut self, end: usize, len: usize) -> Result<()> {
    if self.pos + len > end {
      return Err(Mp4Error::UnexpectedEof);
    }
    self.pos += len;
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

  let box_end = start
    .checked_add(size as usize)
    .ok_or(Mp4Error::InvalidBoxSize)?;
  if box_end > end {
    return Err(Mp4Error::InvalidBoxSize);
  }

  let content_start = start + header_len;
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
  while let Some(b) = next_box(&mut cur, end)? {
    if b.typ == typ {
      return Ok(Some(b.content));
    }
    cur.pos = b.end;
  }
  Ok(None)
}

fn parse_moov(bytes: &[u8], moov: Range<usize>) -> Result<Vec<TrackBoxes>> {
  let mut cur = Cursor::new(bytes, moov.start);
  let mut tracks = Vec::new();

  while cur.pos < moov.end {
    let Some(b) = next_box(&mut cur, moov.end)? else {
      break;
    };
    if b.typ == fourcc(b"trak") {
      tracks.push(parse_trak(bytes, b.content)?);
    }
    cur.pos = b.end;
  }

  Ok(tracks)
}

fn parse_moov_seek(bytes: &[u8], moov: Range<usize>) -> Result<Vec<SeekTrackBoxes>> {
  let mut cur = Cursor::new(bytes, moov.start);
  let mut tracks = Vec::new();

  while cur.pos < moov.end {
    let Some(b) = next_box(&mut cur, moov.end)? else {
      break;
    };
    if b.typ == fourcc(b"trak") {
      tracks.push(parse_trak_seek(bytes, b.content)?);
    }
    cur.pos = b.end;
  }

  Ok(tracks)
}

fn parse_trak(bytes: &[u8], trak: Range<usize>) -> Result<TrackBoxes> {
  let mut cur = Cursor::new(bytes, trak.start);
  let mut t = TrackBoxes::default();

  while cur.pos < trak.end {
    let Some(b) = next_box(&mut cur, trak.end)? else {
      break;
    };
    match b.typ {
      typ if typ == fourcc(b"tkhd") => {
        t.id = Some(parse_tkhd(bytes, b.content)?);
      }
      typ if typ == fourcc(b"mdia") => {
        parse_mdia(bytes, b.content, &mut t)?;
      }
      _ => {}
    }
    cur.pos = b.end;
  }

  Ok(t)
}

fn parse_trak_seek(bytes: &[u8], trak: Range<usize>) -> Result<SeekTrackBoxes> {
  let mut cur = Cursor::new(bytes, trak.start);
  let mut t = SeekTrackBoxes::default();

  while cur.pos < trak.end {
    let Some(b) = next_box(&mut cur, trak.end)? else {
      break;
    };
    match b.typ {
      typ if typ == fourcc(b"tkhd") => {
        t.id = Some(parse_tkhd(bytes, b.content)?);
      }
      typ if typ == fourcc(b"mdia") => {
        parse_mdia_seek(bytes, b.content, &mut t)?;
      }
      _ => {}
    }
    cur.pos = b.end;
  }

  Ok(t)
}

fn parse_mdia(bytes: &[u8], mdia: Range<usize>, t: &mut TrackBoxes) -> Result<()> {
  let mut cur = Cursor::new(bytes, mdia.start);
  while cur.pos < mdia.end {
    let Some(b) = next_box(&mut cur, mdia.end)? else {
      break;
    };
    match b.typ {
      typ if typ == fourcc(b"mdhd") => {
        t.timescale = Some(parse_mdhd(bytes, b.content)?);
      }
      typ if typ == fourcc(b"minf") => {
        parse_minf(bytes, b.content, t)?;
      }
      _ => {}
    }
    cur.pos = b.end;
  }
  Ok(())
}

fn parse_mdia_seek(bytes: &[u8], mdia: Range<usize>, t: &mut SeekTrackBoxes) -> Result<()> {
  let mut cur = Cursor::new(bytes, mdia.start);
  while cur.pos < mdia.end {
    let Some(b) = next_box(&mut cur, mdia.end)? else {
      break;
    };
    match b.typ {
      typ if typ == fourcc(b"mdhd") => {
        t.timescale = Some(parse_mdhd(bytes, b.content)?);
      }
      typ if typ == fourcc(b"minf") => {
        parse_minf_seek(bytes, b.content, t)?;
      }
      _ => {}
    }
    cur.pos = b.end;
  }
  Ok(())
}

fn parse_minf(bytes: &[u8], minf: Range<usize>, t: &mut TrackBoxes) -> Result<()> {
  let mut cur = Cursor::new(bytes, minf.start);
  while cur.pos < minf.end {
    let Some(b) = next_box(&mut cur, minf.end)? else {
      break;
    };
    if b.typ == fourcc(b"stbl") {
      parse_stbl(bytes, b.content, t)?;
    }
    cur.pos = b.end;
  }
  Ok(())
}

fn parse_minf_seek(bytes: &[u8], minf: Range<usize>, t: &mut SeekTrackBoxes) -> Result<()> {
  let mut cur = Cursor::new(bytes, minf.start);
  while cur.pos < minf.end {
    let Some(b) = next_box(&mut cur, minf.end)? else {
      break;
    };
    if b.typ == fourcc(b"stbl") {
      parse_stbl_seek(bytes, b.content, t)?;
    }
    cur.pos = b.end;
  }
  Ok(())
}

fn parse_stbl(bytes: &[u8], stbl: Range<usize>, t: &mut TrackBoxes) -> Result<()> {
  let mut cur = Cursor::new(bytes, stbl.start);
  while cur.pos < stbl.end {
    let Some(b) = next_box(&mut cur, stbl.end)? else {
      break;
    };
    match b.typ {
      typ if typ == fourcc(b"stts") => {
        t.stts = Some(parse_stts(bytes, b.content)?);
      }
      typ if typ == fourcc(b"ctts") => {
        t.ctts = Some(parse_ctts(bytes, b.content)?);
      }
      typ if typ == fourcc(b"stsc") => {
        t.stsc = Some(parse_stsc(bytes, b.content)?);
      }
      typ if typ == fourcc(b"stsz") => {
        t.stsz = Some(parse_stsz(bytes, b.content)?);
      }
      typ if typ == fourcc(b"stco") => {
        // Prefer co64 if present; otherwise store stco.
        if t.chunk_offsets.is_none() {
          t.chunk_offsets = Some(parse_stco(bytes, b.content)?);
        }
      }
      typ if typ == fourcc(b"co64") => {
        t.chunk_offsets = Some(parse_co64(bytes, b.content)?);
      }
      typ if typ == fourcc(b"stss") => {
        t.stss = Some(parse_stss(bytes, b.content)?);
      }
      _ => {}
    }
    cur.pos = b.end;
  }
  Ok(())
}

fn parse_stbl_seek(bytes: &[u8], stbl: Range<usize>, t: &mut SeekTrackBoxes) -> Result<()> {
  let mut cur = Cursor::new(bytes, stbl.start);
  while cur.pos < stbl.end {
    let Some(b) = next_box(&mut cur, stbl.end)? else {
      break;
    };
    match b.typ {
      typ if typ == fourcc(b"stts") => {
        t.stts = Some(parse_stts(bytes, b.content)?);
      }
      typ if typ == fourcc(b"ctts") => {
        t.ctts = Some(parse_ctts(bytes, b.content)?);
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

fn parse_stts(bytes: &[u8], stts: Range<usize>) -> Result<Vec<SttsEntry>> {
  let mut cur = Cursor::new(bytes, stts.start);
  let version = read_fullbox_version(&mut cur, stts.end)?;
  if version != 0 {
    return Err(Mp4Error::UnsupportedBoxVersion {
      box_name: "stts",
      version,
    });
  }

  let entry_count = cur.read_u32(stts.end)? as usize;
  let mut out = Vec::with_capacity(entry_count);
  for _ in 0..entry_count {
    let sample_count = cur.read_u32(stts.end)?;
    let sample_delta = cur.read_u32(stts.end)?;
    out.push(SttsEntry {
      sample_count,
      sample_delta,
    });
  }
  Ok(out)
}

fn parse_ctts(bytes: &[u8], ctts: Range<usize>) -> Result<Vec<CttsEntry>> {
  let mut cur = Cursor::new(bytes, ctts.start);
  let version = read_fullbox_version(&mut cur, ctts.end)?;
  if version != 0 && version != 1 {
    return Err(Mp4Error::UnsupportedBoxVersion {
      box_name: "ctts",
      version,
    });
  }

  let entry_count = cur.read_u32(ctts.end)? as usize;
  let mut out = Vec::with_capacity(entry_count);
  for _ in 0..entry_count {
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

fn parse_stsc(bytes: &[u8], stsc: Range<usize>) -> Result<Vec<StscEntry>> {
  let mut cur = Cursor::new(bytes, stsc.start);
  let version = read_fullbox_version(&mut cur, stsc.end)?;
  if version != 0 {
    return Err(Mp4Error::UnsupportedBoxVersion {
      box_name: "stsc",
      version,
    });
  }

  let entry_count = cur.read_u32(stsc.end)? as usize;
  let mut out = Vec::with_capacity(entry_count);
  for _ in 0..entry_count {
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

fn parse_stsz(bytes: &[u8], stsz: Range<usize>) -> Result<StszBox> {
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
  if sample_size == 0 {
    sample_sizes = Vec::with_capacity(sample_count as usize);
    for _ in 0..sample_count {
      sample_sizes.push(cur.read_u32(stsz.end)?);
    }
  }

  Ok(StszBox {
    sample_size,
    sample_sizes,
    sample_count,
  })
}

fn parse_stco(bytes: &[u8], stco: Range<usize>) -> Result<Vec<u64>> {
  let mut cur = Cursor::new(bytes, stco.start);
  let version = read_fullbox_version(&mut cur, stco.end)?;
  if version != 0 {
    return Err(Mp4Error::UnsupportedBoxVersion {
      box_name: "stco",
      version,
    });
  }

  let entry_count = cur.read_u32(stco.end)? as usize;
  let mut out = Vec::with_capacity(entry_count);
  for _ in 0..entry_count {
    out.push(u64::from(cur.read_u32(stco.end)?));
  }
  Ok(out)
}

fn parse_co64(bytes: &[u8], co64: Range<usize>) -> Result<Vec<u64>> {
  let mut cur = Cursor::new(bytes, co64.start);
  let version = read_fullbox_version(&mut cur, co64.end)?;
  if version != 0 {
    return Err(Mp4Error::UnsupportedBoxVersion {
      box_name: "co64",
      version,
    });
  }

  let entry_count = cur.read_u32(co64.end)? as usize;
  let mut out = Vec::with_capacity(entry_count);
  for _ in 0..entry_count {
    out.push(cur.read_u64(co64.end)?);
  }
  Ok(out)
}

fn parse_stss(bytes: &[u8], stss: Range<usize>) -> Result<Vec<u32>> {
  let mut cur = Cursor::new(bytes, stss.start);
  let version = read_fullbox_version(&mut cur, stss.end)?;
  if version != 0 {
    return Err(Mp4Error::UnsupportedBoxVersion {
      box_name: "stss",
      version,
    });
  }

  let entry_count = cur.read_u32(stss.end)? as usize;
  let mut out = Vec::with_capacity(entry_count);
  for _ in 0..entry_count {
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
    let pts_index = build_pts_index(&pts_ns_by_sample);
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
    assert_eq!(track.last_seek_method(), Some(SeekMethod::SortedBinarySearch));
    assert_eq!(
      track.next_sample(),
      1,
      "seek should choose the first decode-order sample with PTS >= target"
    );
  }
} 
