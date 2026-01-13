use super::{
  MediaAudioInfo, MediaCodec, MediaError, MediaPacket, MediaResult, MediaTrackInfo, MediaTrackType,
  MediaVideoInfo,
};
#[cfg(feature = "media_mp4")]
use super::mp4 as mp4_index;
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
  seek_index: Option<mp4_index::Mp4SeekIndex>,
}

#[cfg(feature = "media_mp4")]
impl Mp4PacketDemuxer<BufReader<File>> {
  pub fn open(path: impl AsRef<Path>) -> MediaResult<Self> {
    let mut file = File::open(path.as_ref())?;
    let len = file.metadata()?.len();

    // Best-effort: read the `moov` box so we can build an efficient timestamp→sample index for
    // seeking without scanning packets.
    let seek_index = match read_top_level_box_bytes(&mut file, len, *b"moov") {
      Ok(Some(moov_bytes)) => mp4_index::Mp4SeekIndex::from_bytes(&moov_bytes).ok(),
      _ => None,
    };

    // Best-effort: parse MP4 sample descriptions via mp4parse so we can detect codecs that the `mp4`
    // crate does not expose via `Mp4Track::media_type()` yet (notably VP9).
    let vp9_tracks = {
      file.rewind()?;
      let map = mp4parse_vp9_tracks(&mut file).unwrap_or_default();
      file.rewind()?;
      map
    };

    file.rewind()?;
    let reader = BufReader::new(file);
    let mp4 = mp4::Mp4Reader::read_header(reader, len)
      .map_err(|e| MediaError::Demux(format!("mp4: failed to read header: {e}")))?;

    let mut demuxer = Self::from_reader_with_vp9_tracks(mp4, vp9_tracks)?;
    demuxer.seek_index = seek_index;
    Ok(demuxer)
  }
}

#[cfg(feature = "media_mp4")]
impl<R: Read + Seek + Send> Mp4PacketDemuxer<R> {
  pub fn from_reader(mp4: mp4::Mp4Reader<R>) -> MediaResult<Self> {
    Self::from_reader_with_vp9_tracks(mp4, HashMap::new())
  }

  fn from_reader_with_vp9_tracks(
    mp4: mp4::Mp4Reader<R>,
    vp9_tracks: HashMap<u32, Mp4Vp9TrackMeta>,
  ) -> MediaResult<Self> {
    let mut tracks = Vec::new();
    let mut cursors = Vec::new();

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
            return Err(MediaError::Demux(format!(
              "mp4: failed to get media type: {err}"
            )));
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
            codec_private: asc,
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
        cursors.push(Mp4TrackCursor {
          id: *track_id,
          timescale,
          sample_count,
          next_sample: 1,
          peeked: None,
        });
      }
    }

    tracks.sort_by_key(|t| t.id);
    cursors.sort_by_key(|c| c.id);

    Ok(Self {
      mp4,
      tracks,
      cursors,
      seek_index: None,
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
fn mp4parse_vp9_tracks<R: Read>(reader: &mut R) -> MediaResult<HashMap<u32, Mp4Vp9TrackMeta>> {
  let ctx =
    mp4parse::read_mp4(reader).map_err(|e| MediaError::Demux(format!("mp4parse: {e:?}")))?;

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

#[cfg(all(test, feature = "media_mp4", feature = "codec_vp9_libvpx"))]
mod tests {
  use super::Mp4PacketDemuxer;
  use crate::media::decoder::create_video_decoder;
  use crate::media::{MediaCodec, MediaTrackType};
  use std::path::PathBuf;

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

    let pkt = demuxer
      .next_packet()
      .expect("demux should succeed")
      .expect("expected at least one packet");
    assert_eq!(pkt.track_id, track.id);
    assert!(!pkt.as_slice().is_empty());

    let mut decoder = create_video_decoder(track).expect("vp9 decoder should be constructible");
    let frames = decoder.decode(&pkt).expect("vp9 decode should succeed");
    assert!(!frames.is_empty());
    assert_eq!((frames[0].width, frames[0].height), (16, 16));
  }
}

#[cfg(feature = "media_mp4")]
impl<R: Read + Seek + Send> MediaDemuxer for Mp4PacketDemuxer<R> {
  fn tracks(&self) -> &[MediaTrackInfo] {
    &self.tracks
  }

  fn next_packet(&mut self) -> MediaResult<Option<MediaPacket>> {
    for cursor in &mut self.cursors {
      mp4_fill_peeked(&mut self.mp4, cursor)?;
    }

    let Some((best_idx, _)) = self
      .cursors
      .iter()
      .enumerate()
      .filter_map(|(i, c)| c.peeked.as_ref().map(|p| (i, p.pts_ns)))
      .min_by_key(|(_, ts)| *ts)
    else {
      return Ok(None);
    };

    Ok(self.cursors[best_idx].peeked.take())
  }

  fn seek(&mut self, time_ns: u64) -> MediaResult<()> {
    if let Some(seek_index) = self.seek_index.as_ref() {
      for cursor in &mut self.cursors {
        cursor.peeked = None;

        if time_ns == 0 {
          cursor.next_sample = 1;
          continue;
        }

        let sample_idx0 = seek_index
          .track(cursor.id)
          .map(|t| t.sample_index_at_or_after(time_ns))
          .unwrap_or(0);

        if sample_idx0 >= cursor.sample_count as usize {
          cursor.next_sample = cursor.sample_count.saturating_add(1);
        } else {
          cursor.next_sample = (sample_idx0 as u32).saturating_add(1);
        }
      }
      return Ok(());
    }

    // Fallback for demuxers that weren't constructed from a file path (no prebuilt index).
    for cursor in &mut self.cursors {
      cursor.next_sample = 1;
      cursor.peeked = None;

      if time_ns == 0 {
        continue;
      }

      loop {
        mp4_fill_peeked(&mut self.mp4, cursor)?;
        match cursor.peeked.as_ref() {
          None => break,
          Some(pkt) if pkt.pts_ns < time_ns => {
            cursor.peeked = None;
            continue;
          }
          Some(_) => break,
        }
      }
    }
    Ok(())
  }
}

#[cfg(feature = "media_mp4")]
fn read_top_level_box_bytes(
  reader: &mut File,
  file_len: u64,
  typ: [u8; 4],
) -> std::io::Result<Option<Vec<u8>>> {
  use std::io::{Read, SeekFrom};

  // `Mp4PacketDemuxer::open` reads the `moov` box once to build a seek index, then rewinds and lets
  // the `mp4` crate parse the file normally. Cap how much we read for this *best-effort* index build
  // so we don't temporarily allocate/IO huge `moov` boxes (which can be attacker-controlled).
  //
  // If the box is larger, we simply fall back to the old linear-scan seek path.
  const MAX_BOX_BYTES_FOR_INDEX: u64 = 64 * 1024 * 1024;

  reader.seek(SeekFrom::Start(0))?;
  let mut pos = 0_u64;
  while pos + 8 <= file_len {
    reader.seek(SeekFrom::Start(pos))?;
    let mut header = [0_u8; 8];
    reader.read_exact(&mut header)?;
    let size32 = u32::from_be_bytes(header[0..4].try_into().unwrap()); // fastrender-allow-unwrap
    let name: [u8; 4] = header[4..8].try_into().unwrap(); // fastrender-allow-unwrap

    let (size, header_len) = match size32 {
      0 => (file_len.saturating_sub(pos), 8_u64),
      1 => {
        let mut ext = [0_u8; 8];
        reader.read_exact(&mut ext)?;
        (u64::from_be_bytes(ext), 16_u64)
      }
      n => (u64::from(n), 8_u64),
    };

    if size < header_len || pos.saturating_add(size) > file_len {
      return Ok(None);
    }

    if name == typ {
      if size > MAX_BOX_BYTES_FOR_INDEX {
        return Ok(None);
      }
      let Ok(size_usize) = usize::try_from(size) else {
        return Ok(None);
      };
      let mut buf = vec![0_u8; size_usize];
      reader.seek(SeekFrom::Start(pos))?;
      reader.read_exact(&mut buf)?;
      return Ok(Some(buf));
    }

    pos = pos.saturating_add(size);
  }

  Ok(None)
}

#[cfg(feature = "media_mp4")]
fn mp4_fill_peeked<R: Read + Seek>(
  mp4: &mut mp4::Mp4Reader<R>,
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
      .map_err(|e| MediaError::Demux(format!("mp4: failed to read sample: {e}")))? else {
      continue;
    };

    // The mp4 crate uses time units in the track's timescale.
    let start_time = sample.start_time;
    let pts_ns = if cursor.timescale == 0 {
      0
    } else {
      (start_time.saturating_mul(1_000_000_000)).saturating_div(u64::from(cursor.timescale))
    };

    cursor.peeked = Some(MediaPacket {
      track_id: u64::from(cursor.id),
      // The `mp4` crate doesn't currently expose a separate DTS/CTTS-derived composition timestamp
      // for samples. For now we treat the track time as both decode and presentation timestamp.
      dts_ns: pts_ns,
      pts_ns,
      duration_ns: 0,
      data: sample.bytes.to_vec().into(),
      is_keyframe: sample.is_sync,
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
      "`media_mp4` feature disabled (enable Cargo feature `media_mp4` or `media`)",
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
      "`media_mp4` feature disabled (enable Cargo feature `media_mp4` or `media`)",
    ))
  }

  fn seek(&mut self, _time_ns: u64) -> MediaResult<()> {
    Err(MediaError::Unsupported(
      "`media_mp4` feature disabled (enable Cargo feature `media_mp4` or `media`)",
    ))
  }
}
