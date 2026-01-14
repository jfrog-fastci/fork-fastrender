use crate::error::RenderStage;
use crate::media::{
  MediaAudioInfo, MediaCodec, MediaData, MediaError, MediaPacket, MediaResult, MediaTrackInfo,
  MediaTrackType, MediaVideoInfo,
};
use crate::render_control::{check_root, check_root_periodic};
use std::io::{Read, Seek};
use std::ops::Range;
use std::sync::Arc;
use thiserror::Error;

const MP4_DEMUX_DEADLINE_STRIDE: usize = 1024;

#[derive(Debug, Error)]
enum Mp4ParseError {
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

type ParseResult<T> = std::result::Result<T, Mp4ParseError>;

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
  _sample_desc_index: u32,
}

#[derive(Debug, Clone)]
struct StszBox {
  sample_size: u32,
  sample_sizes: Vec<u32>,
  sample_count: u32,
}

#[derive(Debug, Default)]
struct TrackBoxes {
  track_id: Option<u32>,
  handler_type: Option<u32>,
  timescale: Option<u32>,
  stts: Option<Vec<SttsEntry>>,
  ctts: Option<Vec<CttsEntry>>,
  stsc: Option<Vec<StscEntry>>,
  stsz: Option<StszBox>,
  chunk_offsets: Option<Vec<u64>>,
  stss: Option<Vec<u32>>,
  stsd: Option<SampleDescription>,
}

#[derive(Debug, Clone)]
struct SampleDescription {
  codec_fourcc: u32,
  codec_private: Vec<u8>,
  video: Option<MediaVideoInfo>,
  audio: Option<MediaAudioInfo>,
}

#[derive(Debug, Clone)]
struct Sample {
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

#[derive(Debug, Clone)]
struct TrackState {
  id: u64,
  samples: Vec<Sample>,
  pts_index: PtsIndex,
  next_sample: usize,
}

impl TrackState {
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

pub struct Mp4Demuxer {
  bytes: Arc<[u8]>,
  tracks: Vec<MediaTrackInfo>,
  track_states: Vec<TrackState>,
  active_track_indices: Vec<usize>,
  min_pts_ns: u64,
}

impl Mp4Demuxer {
  pub fn open(mut reader: impl Read + Seek) -> MediaResult<Self> {
    check_root(RenderStage::Paint).map_err(MediaError::from)?;
    let mut bytes = Vec::new();
    reader.read_to_end(&mut bytes)?;
    Self::from_bytes(bytes)
  }

  pub fn from_bytes(bytes: Vec<u8>) -> MediaResult<Self> {
    check_root(RenderStage::Paint).map_err(MediaError::from)?;
    let bytes: Arc<[u8]> = bytes.into();

    let moov = find_top_level_box(bytes.as_ref(), fourcc(b"moov"))
      .map_err(map_parse_error)?
      .ok_or_else(|| MediaError::Demux(Mp4ParseError::MissingBox("moov").to_string()))?;

    let tracks_boxes = parse_moov(bytes.as_ref(), moov).map_err(map_parse_error)?;
    if tracks_boxes.is_empty() {
      return Err(MediaError::Demux(
        Mp4ParseError::MissingBox("trak").to_string(),
      ));
    }

    let mut tracks = Vec::new();
    let mut track_states = Vec::new();
    let mut active_track_indices = Vec::new();
    let mut deadline_counter = 0usize;

    for t in tracks_boxes.into_iter() {
      check_root_periodic(
        &mut deadline_counter,
        MP4_DEMUX_DEADLINE_STRIDE,
        RenderStage::Paint,
      )
      .map_err(MediaError::from)?;

      let Some(handler) = t.handler_type else {
        continue;
      };
      let track_type = match handler {
        h if h == fourcc(b"vide") => MediaTrackType::Video,
        h if h == fourcc(b"soun") => MediaTrackType::Audio,
        _ => continue,
      };

      let id = t
        .track_id
        .map(u64::from)
        .unwrap_or_else(|| (track_states.len() as u64) + 1);

      let stsd = t
        .stsd
        .clone()
        .ok_or_else(|| MediaError::Demux(Mp4ParseError::MissingBox("stsd").to_string()))?;

      let codec = match (track_type, stsd.codec_fourcc) {
        (MediaTrackType::Video, c) if c == fourcc(b"avc1") || c == fourcc(b"avc3") => {
          MediaCodec::H264
        }
        (MediaTrackType::Audio, c) if c == fourcc(b"mp4a") => MediaCodec::Aac,
        (_, other) => MediaCodec::Unknown(fourcc_to_string(other)),
      };

      let codec_private = stsd.codec_private.clone();
      let video = stsd.video;
      let audio = stsd.audio;

      let state = build_track_state(t, id).map_err(map_parse_error)?;
      let info = MediaTrackInfo {
        id,
        track_type,
        codec: codec.clone(),
        codec_private,
        codec_delay_ns: 0,
        video,
        audio,
      };

      let this_index = track_states.len();
      tracks.push(info);
      track_states.push(state);

      if matches!(codec, MediaCodec::H264 | MediaCodec::Aac) {
        active_track_indices.push(this_index);
      }
    }

    active_track_indices.sort_by_key(|&idx| track_states[idx].id);

    Ok(Self {
      bytes,
      tracks,
      track_states,
      active_track_indices,
      min_pts_ns: 0,
    })
  }

  pub fn tracks(&self) -> &[MediaTrackInfo] {
    &self.tracks
  }

  pub fn next_packet(&mut self) -> MediaResult<Option<MediaPacket>> {
    check_root(RenderStage::Paint).map_err(MediaError::from)?;

    loop {
      // Pick the track whose next sample has the smallest DTS (decode timestamp).
      let mut best: Option<(usize, u64, u64)> = None; // (track_idx, dts, track_id)

      for &track_idx in &self.active_track_indices {
        let track = &mut self.track_states[track_idx];

        // After seeking, we must not emit packets earlier than the seek target. Skip any samples
        // with PTS < `min_pts_ns`.
        while track.next_sample < track.samples.len()
          && track.samples[track.next_sample].pts_ns < self.min_pts_ns
        {
          track.next_sample += 1;
        }

        if track.next_sample >= track.samples.len() {
          continue;
        }

        let dts = track.samples[track.next_sample].dts_ns;
        let id = track.id;
        match best {
          None => best = Some((track_idx, dts, id)),
          Some((_, best_dts, best_id)) => {
            if dts < best_dts || (dts == best_dts && id < best_id) {
              best = Some((track_idx, dts, id));
            }
          }
        }
      }

      let Some((track_idx, _dts, _id)) = best else {
        return Ok(None);
      };

      let track = &mut self.track_states[track_idx];
      let sample = match track.samples.get(track.next_sample) {
        Some(s) => s.clone(),
        None => continue,
      };
      track.next_sample += 1;

      let start = usize::try_from(sample.offset)
        .map_err(|_| MediaError::Demux("MP4 sample offset out of range".to_string()))?;
      let end = start
        .checked_add(sample.size as usize)
        .ok_or_else(|| MediaError::Demux("MP4 sample size overflow".to_string()))?;
      if end > self.bytes.len() {
        return Err(MediaError::Demux(
          "MP4 sample data out of bounds".to_string(),
        ));
      }

      return Ok(Some(MediaPacket {
        track_id: track.id,
        dts_ns: sample.dts_ns,
        pts_ns: sample.pts_ns,
        duration_ns: sample.duration_ns,
        data: MediaData::Shared {
          bytes: self.bytes.clone(),
          range: start..end,
        },
        is_keyframe: sample.is_sync,
      }));
    }
  }

  pub fn seek(&mut self, time_ns: u64) -> MediaResult<()> {
    check_root(RenderStage::Paint).map_err(MediaError::from)?;

    self.min_pts_ns = time_ns;

    for &track_idx in &self.active_track_indices {
      self.track_states[track_idx].seek(time_ns);
    }

    Ok(())
  }
}

fn map_parse_error(err: Mp4ParseError) -> MediaError {
  MediaError::Demux(err.to_string())
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

  fn read_u8(&mut self, end: usize) -> ParseResult<u8> {
    if self.pos + 1 > end {
      return Err(Mp4ParseError::UnexpectedEof);
    }
    let v = self.data[self.pos];
    self.pos += 1;
    Ok(v)
  }

  fn read_u16(&mut self, end: usize) -> ParseResult<u16> {
    if self.pos + 2 > end {
      return Err(Mp4ParseError::UnexpectedEof);
    }
    let v = u16::from_be_bytes(self.data[self.pos..self.pos + 2].try_into().unwrap());
    self.pos += 2;
    Ok(v)
  }

  fn read_u32(&mut self, end: usize) -> ParseResult<u32> {
    if self.pos + 4 > end {
      return Err(Mp4ParseError::UnexpectedEof);
    }
    let v = u32::from_be_bytes(self.data[self.pos..self.pos + 4].try_into().unwrap());
    self.pos += 4;
    Ok(v)
  }

  fn read_u64(&mut self, end: usize) -> ParseResult<u64> {
    if self.pos + 8 > end {
      return Err(Mp4ParseError::UnexpectedEof);
    }
    let v = u64::from_be_bytes(self.data[self.pos..self.pos + 8].try_into().unwrap());
    self.pos += 8;
    Ok(v)
  }

  fn read_i32(&mut self, end: usize) -> ParseResult<i32> {
    let v = self.read_u32(end)?;
    Ok(i32::from_be_bytes(v.to_be_bytes()))
  }

  fn skip(&mut self, end: usize, len: usize) -> ParseResult<()> {
    if self.pos + len > end {
      return Err(Mp4ParseError::UnexpectedEof);
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

fn fourcc_to_string(tag: u32) -> String {
  let bytes = tag.to_be_bytes();
  if bytes.iter().all(|b| (0x20..=0x7e).contains(b)) {
    String::from_utf8_lossy(&bytes).into_owned()
  } else {
    format!("0x{tag:08x}")
  }
}

fn next_box(cur: &mut Cursor<'_>, end: usize) -> ParseResult<Option<BoxRef>> {
  if cur.pos >= end {
    return Ok(None);
  }
  if end - cur.pos < 8 {
    return Err(Mp4ParseError::UnexpectedEof);
  }

  let start = cur.pos;
  let size32 = cur.read_u32(end)?;
  let typ = cur.read_u32(end)?;

  let (size, header_len) = match size32 {
    0 => ((end - start) as u64, 8usize),
    1 => {
      let size64 = cur.read_u64(end)?;
      (size64, 16usize)
    }
    n => (u64::from(n), 8usize),
  };

  if size < header_len as u64 {
    return Err(Mp4ParseError::InvalidBoxSize);
  }

  let box_end = start
    .checked_add(size as usize)
    .ok_or(Mp4ParseError::InvalidBoxSize)?;
  if box_end > end {
    return Err(Mp4ParseError::InvalidBoxSize);
  }

  let content_start = start + header_len;
  let content_end = box_end;

  Ok(Some(BoxRef {
    typ,
    content: content_start..content_end,
    end: box_end,
  }))
}

fn find_top_level_box(bytes: &[u8], typ: u32) -> ParseResult<Option<Range<usize>>> {
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

fn parse_moov(bytes: &[u8], moov: Range<usize>) -> ParseResult<Vec<TrackBoxes>> {
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

fn parse_trak(bytes: &[u8], trak: Range<usize>) -> ParseResult<TrackBoxes> {
  let mut cur = Cursor::new(bytes, trak.start);
  let mut t = TrackBoxes::default();

  while cur.pos < trak.end {
    let Some(b) = next_box(&mut cur, trak.end)? else {
      break;
    };
    match b.typ {
      typ if typ == fourcc(b"tkhd") => {
        t.track_id = Some(parse_tkhd(bytes, b.content)?);
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

fn parse_mdia(bytes: &[u8], mdia: Range<usize>, t: &mut TrackBoxes) -> ParseResult<()> {
  let mut cur = Cursor::new(bytes, mdia.start);
  while cur.pos < mdia.end {
    let Some(b) = next_box(&mut cur, mdia.end)? else {
      break;
    };
    match b.typ {
      typ if typ == fourcc(b"mdhd") => {
        t.timescale = Some(parse_mdhd(bytes, b.content)?);
      }
      typ if typ == fourcc(b"hdlr") => {
        t.handler_type = Some(parse_hdlr(bytes, b.content)?);
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

fn parse_minf(bytes: &[u8], minf: Range<usize>, t: &mut TrackBoxes) -> ParseResult<()> {
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

fn parse_stbl(bytes: &[u8], stbl: Range<usize>, t: &mut TrackBoxes) -> ParseResult<()> {
  let mut cur = Cursor::new(bytes, stbl.start);
  while cur.pos < stbl.end {
    let Some(b) = next_box(&mut cur, stbl.end)? else {
      break;
    };
    match b.typ {
      typ if typ == fourcc(b"stsd") => {
        t.stsd = Some(parse_stsd(bytes, b.content)?);
      }
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

fn read_fullbox_version(cur: &mut Cursor<'_>, end: usize) -> ParseResult<u8> {
  let version = cur.read_u8(end)?;
  cur.skip(end, 3)?;
  Ok(version)
}

fn parse_tkhd(bytes: &[u8], tkhd: Range<usize>) -> ParseResult<u32> {
  let mut cur = Cursor::new(bytes, tkhd.start);
  let version = read_fullbox_version(&mut cur, tkhd.end)?;

  match version {
    0 => {
      cur.skip(tkhd.end, 8)?; // creation + modification
      let track_id = cur.read_u32(tkhd.end)?;
      Ok(track_id)
    }
    1 => {
      cur.skip(tkhd.end, 16)?; // creation + modification
      let track_id = cur.read_u32(tkhd.end)?;
      Ok(track_id)
    }
    v => Err(Mp4ParseError::UnsupportedBoxVersion {
      box_name: "tkhd",
      version: v,
    }),
  }
}

fn parse_hdlr(bytes: &[u8], hdlr: Range<usize>) -> ParseResult<u32> {
  let mut cur = Cursor::new(bytes, hdlr.start);
  let version = read_fullbox_version(&mut cur, hdlr.end)?;
  if version != 0 {
    return Err(Mp4ParseError::UnsupportedBoxVersion {
      box_name: "hdlr",
      version,
    });
  }

  cur.skip(hdlr.end, 4)?; // pre_defined
  cur.read_u32(hdlr.end)
}

fn parse_mdhd(bytes: &[u8], mdhd: Range<usize>) -> ParseResult<u32> {
  let mut cur = Cursor::new(bytes, mdhd.start);
  let version = read_fullbox_version(&mut cur, mdhd.end)?;

  match version {
    0 => {
      cur.skip(mdhd.end, 8)?; // creation + modification
      Ok(cur.read_u32(mdhd.end)?)
    }
    1 => {
      cur.skip(mdhd.end, 16)?; // creation + modification
      Ok(cur.read_u32(mdhd.end)?)
    }
    v => Err(Mp4ParseError::UnsupportedBoxVersion {
      box_name: "mdhd",
      version: v,
    }),
  }
}

fn parse_stsd(bytes: &[u8], stsd: Range<usize>) -> ParseResult<SampleDescription> {
  let mut cur = Cursor::new(bytes, stsd.start);
  let version = read_fullbox_version(&mut cur, stsd.end)?;
  if version != 0 {
    return Err(Mp4ParseError::UnsupportedBoxVersion {
      box_name: "stsd",
      version,
    });
  }

  let entry_count = cur.read_u32(stsd.end)? as usize;
  if entry_count == 0 {
    return Err(Mp4ParseError::Invalid("stsd must have at least one entry"));
  }

  let first = next_box(&mut cur, stsd.end)?.ok_or(Mp4ParseError::UnexpectedEof)?;
  let codec_fourcc = first.typ;

  let (codec_private, video, audio) = match codec_fourcc {
    typ if typ == fourcc(b"avc1") || typ == fourcc(b"avc3") => {
      let (video, avcc) = parse_avc_sample_entry(bytes, first.content)?;
      (avcc, Some(video), None)
    }
    typ if typ == fourcc(b"mp4a") => {
      let (audio, asc) = parse_aac_sample_entry(bytes, first.content)?;
      (asc, None, Some(audio))
    }
    _ => (Vec::new(), None, None),
  };

  Ok(SampleDescription {
    codec_fourcc,
    codec_private,
    video,
    audio,
  })
}

fn parse_avc_sample_entry(
  bytes: &[u8],
  entry: Range<usize>,
) -> ParseResult<(MediaVideoInfo, Vec<u8>)> {
  let mut cur = Cursor::new(bytes, entry.start);
  cur.skip(entry.end, 6)?; // reserved
  cur.skip(entry.end, 2)?; // data_reference_index
  cur.skip(entry.end, 16)?; // pre_defined + reserved + pre_defined[3]
  let width = cur.read_u16(entry.end)? as u32;
  let height = cur.read_u16(entry.end)? as u32;
  cur.skip(entry.end, 50)?; // remainder of VisualSampleEntry fields

  let mut avcc = Vec::new();
  while cur.pos < entry.end {
    let Some(b) = next_box(&mut cur, entry.end)? else {
      break;
    };
    if b.typ == fourcc(b"avcC") {
      avcc = bytes[b.content.clone()].to_vec();
    }
    cur.pos = b.end;
  }

  if avcc.is_empty() {
    return Err(Mp4ParseError::MissingBox("avcC"));
  }

  let codec_private = parse_avcc_for_h264_codec_private(&avcc)?;
  Ok((MediaVideoInfo { width, height }, codec_private))
}

fn parse_avcc_for_h264_codec_private(avcc: &[u8]) -> ParseResult<Vec<u8>> {
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
    return Err(Mp4ParseError::UnexpectedEof);
  }
  let mut i = 0usize;

  let configuration_version = avcc[i];
  i += 1;
  if configuration_version != 1 {
    return Err(Mp4ParseError::Invalid(
      "unsupported avcC configurationVersion",
    ));
  }

  // Skip profile/compat/level.
  i += 3;

  let length_size_minus_one = avcc[i] & 0b11;
  i += 1;
  let nal_length_size = length_size_minus_one + 1;
  if nal_length_size == 0 || nal_length_size > 4 {
    return Err(Mp4ParseError::Invalid("invalid avcC NAL length size"));
  }

  let num_sps = avcc[i] & 0b1_1111;
  i += 1;
  let sps_count = num_sps as usize;

  let mut out = Vec::new();
  out.push(nal_length_size);
  out.push(num_sps);

  for _ in 0..sps_count {
    if i + 2 > avcc.len() {
      return Err(Mp4ParseError::UnexpectedEof);
    }
    let len = u16::from_be_bytes([avcc[i], avcc[i + 1]]) as usize;
    i += 2;
    let end = i
      .checked_add(len)
      .ok_or(Mp4ParseError::Invalid("avcC length overflow"))?;
    if end > avcc.len() {
      return Err(Mp4ParseError::UnexpectedEof);
    }
    out.extend_from_slice(&(len as u16).to_be_bytes());
    out.extend_from_slice(&avcc[i..end]);
    i = end;
  }

  if i >= avcc.len() {
    return Err(Mp4ParseError::UnexpectedEof);
  }
  let num_pps = avcc[i];
  i += 1;
  let pps_count = num_pps as usize;

  out.push(num_pps);
  for _ in 0..pps_count {
    if i + 2 > avcc.len() {
      return Err(Mp4ParseError::UnexpectedEof);
    }
    let len = u16::from_be_bytes([avcc[i], avcc[i + 1]]) as usize;
    i += 2;
    let end = i
      .checked_add(len)
      .ok_or(Mp4ParseError::Invalid("avcC length overflow"))?;
    if end > avcc.len() {
      return Err(Mp4ParseError::UnexpectedEof);
    }
    out.extend_from_slice(&(len as u16).to_be_bytes());
    out.extend_from_slice(&avcc[i..end]);
    i = end;
  }

  Ok(out)
}

fn parse_aac_sample_entry(
  bytes: &[u8],
  entry: Range<usize>,
) -> ParseResult<(MediaAudioInfo, Vec<u8>)> {
  let mut cur = Cursor::new(bytes, entry.start);
  cur.skip(entry.end, 6)?; // reserved
  cur.skip(entry.end, 2)?; // data_reference_index
  let version = cur.read_u16(entry.end)?;
  cur.skip(entry.end, 2)?; // revision_level
  cur.skip(entry.end, 4)?; // vendor
  let channels = cur.read_u16(entry.end)?;
  cur.skip(entry.end, 2)?; // sample_size
  cur.skip(entry.end, 2)?; // compression_id
  cur.skip(entry.end, 2)?; // packet_size
  let sample_rate_fixed = cur.read_u32(entry.end)?;
  let sample_rate = sample_rate_fixed >> 16;

  // Skip version-specific extensions (QuickTime-style).
  match version {
    0 => {}
    1 => cur.skip(entry.end, 16)?,
    2 => cur.skip(entry.end, 36)?,
    _ => {
      return Err(Mp4ParseError::Invalid(
        "unsupported mp4a sample entry version",
      ))
    }
  }

  let mut asc = Vec::new();
  while cur.pos < entry.end {
    let Some(b) = next_box(&mut cur, entry.end)? else {
      break;
    };
    if b.typ == fourcc(b"esds") {
      asc = parse_esds_for_asc(bytes, b.content)?;
    }
    cur.pos = b.end;
  }

  if asc.is_empty() {
    return Err(Mp4ParseError::MissingBox("esds/asc"));
  }

  Ok((
    MediaAudioInfo {
      sample_rate,
      channels,
    },
    asc,
  ))
}

fn parse_esds_for_asc(bytes: &[u8], esds: Range<usize>) -> ParseResult<Vec<u8>> {
  let data = &bytes[esds.clone()];
  if data.len() < 4 {
    return Err(Mp4ParseError::UnexpectedEof);
  }

  // FullBox header.
  let version = data[0];
  if version != 0 {
    return Err(Mp4ParseError::UnsupportedBoxVersion {
      box_name: "esds",
      version,
    });
  }
  let mut pos = 4usize;

  // ES_Descriptor (tag 0x03).
  if pos >= data.len() {
    return Err(Mp4ParseError::UnexpectedEof);
  }
  let tag = data[pos];
  pos += 1;
  if tag != 0x03 {
    return Err(Mp4ParseError::Invalid("esds missing ES_Descriptor"));
  }
  let es_len = read_descriptor_len(data, &mut pos)? as usize;
  let es_end = pos
    .checked_add(es_len)
    .ok_or(Mp4ParseError::Invalid("esds length overflow"))?;
  if es_end > data.len() {
    return Err(Mp4ParseError::UnexpectedEof);
  }

  // ES_ID (u16) + flags (u8).
  if pos + 3 > es_end {
    return Err(Mp4ParseError::UnexpectedEof);
  }
  pos += 2;
  let flags = data[pos];
  pos += 1;
  if flags & 0x80 != 0 {
    pos += 2; // dependsOn_ES_ID
  }
  if flags & 0x40 != 0 {
    if pos >= es_end {
      return Err(Mp4ParseError::UnexpectedEof);
    }
    let url_len = data[pos] as usize;
    pos += 1 + url_len;
  }
  if flags & 0x20 != 0 {
    pos += 2; // OCR_ES_ID
  }
  if pos > es_end {
    return Err(Mp4ParseError::UnexpectedEof);
  }

  // DecoderConfigDescriptor (tag 0x04).
  if pos >= es_end {
    return Err(Mp4ParseError::UnexpectedEof);
  }
  let tag = data[pos];
  pos += 1;
  if tag != 0x04 {
    return Err(Mp4ParseError::Invalid(
      "esds missing DecoderConfigDescriptor (tag 0x04)",
    ));
  }
  let cfg_len = read_descriptor_len(data, &mut pos)? as usize;
  let cfg_end = pos
    .checked_add(cfg_len)
    .ok_or(Mp4ParseError::Invalid("esds length overflow"))?;
  if cfg_end > es_end {
    return Err(Mp4ParseError::UnexpectedEof);
  }

  // objectTypeIndication (1) + streamType (1) + bufferSizeDB (3) + maxBitrate (4) + avgBitrate (4)
  if pos + 13 > cfg_end {
    return Err(Mp4ParseError::UnexpectedEof);
  }
  pos += 13;

  // DecoderSpecificInfo (tag 0x05).
  if pos >= cfg_end {
    return Err(Mp4ParseError::UnexpectedEof);
  }
  let tag = data[pos];
  pos += 1;
  if tag != 0x05 {
    return Err(Mp4ParseError::Invalid(
      "esds missing DecoderSpecificInfo (tag 0x05)",
    ));
  }
  let asc_len = read_descriptor_len(data, &mut pos)? as usize;
  let asc_end = pos
    .checked_add(asc_len)
    .ok_or(Mp4ParseError::Invalid("esds length overflow"))?;
  if asc_end > cfg_end {
    return Err(Mp4ParseError::UnexpectedEof);
  }

  Ok(data[pos..asc_end].to_vec())
}

fn read_descriptor_len(data: &[u8], pos: &mut usize) -> ParseResult<u32> {
  let mut out: u32 = 0;
  for _ in 0..4 {
    let b = *data.get(*pos).ok_or(Mp4ParseError::UnexpectedEof)?;
    *pos += 1;
    out = (out << 7) | u32::from(b & 0x7f);
    if (b & 0x80) == 0 {
      return Ok(out);
    }
  }
  Ok(out)
}

fn parse_stts(bytes: &[u8], stts: Range<usize>) -> ParseResult<Vec<SttsEntry>> {
  let mut cur = Cursor::new(bytes, stts.start);
  let version = read_fullbox_version(&mut cur, stts.end)?;
  if version != 0 {
    return Err(Mp4ParseError::UnsupportedBoxVersion {
      box_name: "stts",
      version,
    });
  }

  let entry_count = cur.read_u32(stts.end)? as usize;
  let mut out = Vec::with_capacity(entry_count);
  for _ in 0..entry_count {
    out.push(SttsEntry {
      sample_count: cur.read_u32(stts.end)?,
      sample_delta: cur.read_u32(stts.end)?,
    });
  }
  Ok(out)
}

fn parse_ctts(bytes: &[u8], ctts: Range<usize>) -> ParseResult<Vec<CttsEntry>> {
  let mut cur = Cursor::new(bytes, ctts.start);
  let version = read_fullbox_version(&mut cur, ctts.end)?;
  if version != 0 && version != 1 {
    return Err(Mp4ParseError::UnsupportedBoxVersion {
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

fn parse_stsc(bytes: &[u8], stsc: Range<usize>) -> ParseResult<Vec<StscEntry>> {
  let mut cur = Cursor::new(bytes, stsc.start);
  let version = read_fullbox_version(&mut cur, stsc.end)?;
  if version != 0 {
    return Err(Mp4ParseError::UnsupportedBoxVersion {
      box_name: "stsc",
      version,
    });
  }

  let entry_count = cur.read_u32(stsc.end)? as usize;
  let mut out = Vec::with_capacity(entry_count);
  for _ in 0..entry_count {
    out.push(StscEntry {
      first_chunk: cur.read_u32(stsc.end)?,
      samples_per_chunk: cur.read_u32(stsc.end)?,
      _sample_desc_index: cur.read_u32(stsc.end)?,
    });
  }
  if out.is_empty() {
    return Err(Mp4ParseError::Invalid("stsc must have at least one entry"));
  }
  Ok(out)
}

fn parse_stsz(bytes: &[u8], stsz: Range<usize>) -> ParseResult<StszBox> {
  let mut cur = Cursor::new(bytes, stsz.start);
  let version = read_fullbox_version(&mut cur, stsz.end)?;
  if version != 0 {
    return Err(Mp4ParseError::UnsupportedBoxVersion {
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

fn parse_stco(bytes: &[u8], stco: Range<usize>) -> ParseResult<Vec<u64>> {
  let mut cur = Cursor::new(bytes, stco.start);
  let version = read_fullbox_version(&mut cur, stco.end)?;
  if version != 0 {
    return Err(Mp4ParseError::UnsupportedBoxVersion {
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

fn parse_co64(bytes: &[u8], co64: Range<usize>) -> ParseResult<Vec<u64>> {
  let mut cur = Cursor::new(bytes, co64.start);
  let version = read_fullbox_version(&mut cur, co64.end)?;
  if version != 0 {
    return Err(Mp4ParseError::UnsupportedBoxVersion {
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

fn parse_stss(bytes: &[u8], stss: Range<usize>) -> ParseResult<Vec<u32>> {
  let mut cur = Cursor::new(bytes, stss.start);
  let version = read_fullbox_version(&mut cur, stss.end)?;
  if version != 0 {
    return Err(Mp4ParseError::UnsupportedBoxVersion {
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

fn build_track_state(t: TrackBoxes, id: u64) -> ParseResult<TrackState> {
  let timescale = t.timescale.ok_or(Mp4ParseError::MissingBox("mdhd"))?;
  let stts = t.stts.ok_or(Mp4ParseError::MissingBox("stts"))?;
  let stsc = t.stsc.ok_or(Mp4ParseError::MissingBox("stsc"))?;
  let stsz = t.stsz.ok_or(Mp4ParseError::MissingBox("stsz"))?;
  let chunk_offsets = t
    .chunk_offsets
    .ok_or(Mp4ParseError::MissingBox("stco/co64"))?;
  let ctts = t.ctts.unwrap_or_default();

  let sample_count = stsz.sample_count as usize;
  if sample_count == 0 {
    return Ok(TrackState {
      id,
      samples: Vec::new(),
      pts_index: PtsIndex::Monotonic,
      next_sample: 0,
    });
  }

  // Build sync flags.
  let mut sync_flags = vec![false; sample_count];
  match t.stss {
    None => sync_flags.fill(true),
    Some(stss) => {
      for sample_num_1_based in stss {
        let idx = sample_num_1_based
          .checked_sub(1)
          .ok_or(Mp4ParseError::Invalid("stss sample number must be >= 1"))?
          as usize;
        if idx >= sample_count {
          return Err(Mp4ParseError::Invalid("stss sample number out of range"));
        }
        sync_flags[idx] = true;
      }
    }
  }

  // Build sample offsets/sizes in decode order.
  let mut samples = Vec::with_capacity(sample_count);
  let mut sample_idx = 0usize;
  let mut stsc_idx = 0usize;

  for (chunk_idx0, &chunk_base) in chunk_offsets.iter().enumerate() {
    if sample_idx >= sample_count {
      break;
    }

    let chunk_num = (chunk_idx0 + 1) as u32;
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
          .ok_or(Mp4ParseError::Invalid("stsz sample_sizes underrun"))?
      };

      let start = offset;
      offset = offset
        .checked_add(u64::from(size))
        .ok_or(Mp4ParseError::Invalid("sample offset overflow"))?;

      samples.push(Sample {
        offset: start,
        size,
        dts_ns: 0,
        pts_ns: 0,
        duration_ns: 0,
        is_sync: sync_flags[sample_idx],
      });

      sample_idx += 1;
    }
  }

  if sample_idx != sample_count {
    return Err(Mp4ParseError::Invalid(
      "sample table construction did not yield expected sample_count",
    ));
  }

  let mut stts_iter = TableRunIter::new_stts(&stts);
  let mut ctts_iter = TableRunIter::new_ctts(&ctts);

  // Compute the minimum PTS tick value so we can normalize away negative PTS ticks (CTTS v1).
  let mut dts_ticks: i64 = 0;
  let mut min_pts_ticks: i64 = i64::MAX;
  for sample in &mut samples {
    let dur_ticks = stts_iter
      .next_u32()
      .ok_or(Mp4ParseError::Invalid("stts shorter than sample_count"))?;
    let ctts_off = ctts_iter.next_i64().unwrap_or(0);

    sample.dts_ns = ticks_to_ns(dts_ticks, timescale);
    let pts_ticks = dts_ticks.saturating_add(ctts_off);
    min_pts_ticks = min_pts_ticks.min(pts_ticks);
    sample.duration_ns = ticks_to_ns(i64::from(dur_ticks), timescale);

    dts_ticks = dts_ticks.saturating_add(i64::from(dur_ticks));
  }

  let pts_offset_ticks: i128 = if min_pts_ticks < 0 {
    -(min_pts_ticks as i128)
  } else {
    0
  };

  // Second pass: fill PTS ns values, applying the normalization offset.
  let mut stts_iter = TableRunIter::new_stts(&stts);
  let mut ctts_iter = TableRunIter::new_ctts(&ctts);
  let mut dts_ticks: i64 = 0;
  for sample in &mut samples {
    let dur_ticks = stts_iter
      .next_u32()
      .ok_or(Mp4ParseError::Invalid("stts shorter than sample_count"))?;
    let ctts_off = ctts_iter.next_i64().unwrap_or(0);

    sample.dts_ns = ticks_to_ns(dts_ticks, timescale);
    let pts_ticks = dts_ticks.saturating_add(ctts_off);
    let shifted = (pts_ticks as i128).saturating_add(pts_offset_ticks);
    let shifted = if shifted > i64::MAX as i128 {
      i64::MAX
    } else if shifted <= 0 {
      0
    } else {
      shifted as i64
    };
    sample.pts_ns = ticks_to_ns(shifted, timescale);

    dts_ticks = dts_ticks.saturating_add(i64::from(dur_ticks));
  }

  let pts_index = build_pts_index(&samples);

  Ok(TrackState {
    id,
    samples,
    pts_index,
    next_sample: 0,
  })
}

fn build_pts_index(samples: &[Sample]) -> PtsIndex {
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

fn ticks_to_ns(ticks: i64, timescale: u32) -> u64 {
  if ticks <= 0 {
    return 0;
  }

  let den = u128::from(timescale);
  if den == 0 {
    return u64::MAX;
  }

  let ticks = ticks as u128;
  let ns = ticks
    .saturating_mul(1_000_000_000u128)
    .saturating_add(den / 2)
    / den;

  ns.min(u128::from(u64::MAX)) as u64
}

#[cfg(test)]
mod tests {
  use super::*;
  use std::io::Cursor;

  #[test]
  fn demuxes_h264_aac_and_seeks() {
    let fixture_path = crate::testing::fixture_path("fixtures/media/test_h264_aac.mp4");
    let bytes = std::fs::read(fixture_path).expect("read mp4 fixture");

    let mut demuxer = Mp4Demuxer::open(Cursor::new(bytes.as_slice())).expect("open mp4");

    assert!(
      demuxer.tracks().len() >= 2,
      "fixture should expose at least audio+video tracks"
    );

    let video_track = demuxer
      .tracks()
      .iter()
      .find(|t| t.codec == MediaCodec::H264)
      .map(|t| t.id)
      .expect("H264 track");
    let audio_track = demuxer
      .tracks()
      .iter()
      .find(|t| t.codec == MediaCodec::Aac)
      .map(|t| t.id)
      .expect("AAC track");

    let mut saw_video = false;
    let mut saw_audio = false;
    let mut last_video_dts = None::<u64>;
    let mut last_audio_dts = None::<u64>;
    for _ in 0..256 {
      let Some(pkt) = demuxer.next_packet().expect("read packet") else {
        break;
      };
      assert!(
        !pkt.as_slice().is_empty(),
        "demuxed packet should have non-empty data"
      );
      if pkt.track_id == video_track {
        if let Some(prev) = last_video_dts {
          assert!(
            pkt.dts_ns >= prev,
            "video DTS must be non-decreasing (prev {}ns, got {}ns)",
            prev,
            pkt.dts_ns
          );
        }
        last_video_dts = Some(pkt.dts_ns);
        saw_video = true;
      }
      if pkt.track_id == audio_track {
        if let Some(prev) = last_audio_dts {
          assert!(
            pkt.dts_ns >= prev,
            "audio DTS must be non-decreasing (prev {}ns, got {}ns)",
            prev,
            pkt.dts_ns
          );
        }
        last_audio_dts = Some(pkt.dts_ns);
        saw_audio = true;
      }
      if saw_video && saw_audio {
        break;
      }
    }
    assert!(saw_video, "expected at least one H264 packet");
    assert!(saw_audio, "expected at least one AAC packet");

    let seek_target_ns = 500_000_000_u64;
    demuxer.seek(seek_target_ns).expect("seek");

    let mut post_seek_video = false;
    let mut post_seek_audio = false;
    let mut last_video_dts = None::<u64>;
    let mut last_audio_dts = None::<u64>;
    for _ in 0..256 {
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
        if let Some(prev) = last_video_dts {
          assert!(
            pkt.dts_ns >= prev,
            "video DTS must be non-decreasing after seek (prev {}ns, got {}ns)",
            prev,
            pkt.dts_ns
          );
        }
        last_video_dts = Some(pkt.dts_ns);
        post_seek_video = true;
      }
      if pkt.track_id == audio_track {
        if let Some(prev) = last_audio_dts {
          assert!(
            pkt.dts_ns >= prev,
            "audio DTS must be non-decreasing after seek (prev {}ns, got {}ns)",
            prev,
            pkt.dts_ns
          );
        }
        last_audio_dts = Some(pkt.dts_ns);
        post_seek_audio = true;
      }
      if post_seek_video && post_seek_audio {
        break;
      }
    }
    assert!(post_seek_video, "expected H264 packet after seek");
    assert!(post_seek_audio, "expected AAC packet after seek");
  }

  #[test]
  fn track_state_normalizes_negative_pts_ticks() {
    // PTS ticks derived from `dts + ctts`:
    // dts: 0, 1, 2, 3
    // ctts: -3, -1, 0, 1
    // pts: -3, 0, 2, 4  => normalize by +3 => 0, 3, 5, 7
    let state = build_track_state(
      TrackBoxes {
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
        stsc: Some(vec![StscEntry {
          first_chunk: 1,
          samples_per_chunk: 4,
          _sample_desc_index: 1,
        }]),
        stsz: Some(StszBox {
          sample_size: 1,
          sample_sizes: Vec::new(),
          sample_count: 4,
        }),
        chunk_offsets: Some(vec![0]),
        ..Default::default()
      },
      1,
    )
    .expect("track state");

    assert_eq!(
      state.samples.iter().map(|s| s.pts_ns).collect::<Vec<_>>(),
      vec![0, 3_000_000_000, 5_000_000_000, 7_000_000_000]
    );
  }

  #[test]
  fn track_state_seek_sorted_pts_returns_decode_order_sample_index() {
    // Non-monotonic PTS in decode order (typical B-frame reordering):
    //
    // dts: 0, 1, 2, 3
    // ctts: 0, +2, -1, -1
    // pts: 0, 3, 1, 2
    //
    // When seeking to pts>=2, the first *decode-order* sample whose PTS is >=2 is sample 1 (pts=3),
    // not sample 3 (pts=2).
    let mut state = build_track_state(
      TrackBoxes {
        timescale: Some(1),
        stts: Some(vec![SttsEntry {
          sample_count: 4,
          sample_delta: 1,
        }]),
        ctts: Some(vec![
          CttsEntry {
            sample_count: 1,
            sample_offset: 0,
          },
          CttsEntry {
            sample_count: 1,
            sample_offset: 2,
          },
          CttsEntry {
            sample_count: 2,
            sample_offset: -1,
          },
        ]),
        stsc: Some(vec![StscEntry {
          first_chunk: 1,
          samples_per_chunk: 4,
          _sample_desc_index: 1,
        }]),
        stsz: Some(StszBox {
          sample_size: 1,
          sample_sizes: Vec::new(),
          sample_count: 4,
        }),
        chunk_offsets: Some(vec![0]),
        ..Default::default()
      },
      1,
    )
    .expect("track state");

    state.seek(2_000_000_000);
    assert_eq!(state.next_sample, 1);
  }

  #[test]
  fn demuxer_h264_codec_private_is_decoder_format() {
    let fixture_path = crate::testing::fixture_path("fixtures/media/test_h264_aac.mp4");
    let bytes = std::fs::read(fixture_path).expect("read mp4 fixture");

    let demuxer = Mp4Demuxer::open(Cursor::new(bytes.as_slice())).expect("open mp4");
    let h264 = demuxer
      .tracks()
      .iter()
      .find(|t| t.codec == MediaCodec::H264)
      .expect("H264 track");

    // Parse the minimal H264 extradata format expected by `decoder::H264Decoder`:
    // u8 nal_length_size
    // u8 sps_count
    // [sps_count] { u16be len, [len] bytes }
    // u8 pps_count
    // [pps_count] { u16be len, [len] bytes }
    let data = &h264.codec_private;
    assert!(!data.is_empty(), "expected non-empty H264 codec_private");

    let mut i = 0usize;
    let nal_length_size = *data.get(i).expect("nal length size");
    assert!(
      (1..=4).contains(&nal_length_size),
      "unexpected nal_length_size: {nal_length_size}"
    );
    i += 1;

    let sps_count = *data.get(i).expect("sps count") as usize;
    i += 1;
    assert!(sps_count > 0, "expected at least one SPS");

    for _ in 0..sps_count {
      let len = u16::from_be_bytes([data[i], data[i + 1]]) as usize;
      i += 2;
      let end = i + len;
      assert!(end <= data.len(), "SPS out of bounds");
      assert!(len > 0, "SPS must be non-empty");
      i = end;
    }

    let pps_count = *data.get(i).expect("pps count") as usize;
    i += 1;
    assert!(pps_count > 0, "expected at least one PPS");

    for _ in 0..pps_count {
      let len = u16::from_be_bytes([data[i], data[i + 1]]) as usize;
      i += 2;
      let end = i + len;
      assert!(end <= data.len(), "PPS out of bounds");
      assert!(len > 0, "PPS must be non-empty");
      i = end;
    }

    assert_eq!(i, data.len(), "unexpected trailing bytes in codec_private");
  }
}
