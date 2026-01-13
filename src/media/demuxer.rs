use super::{
  MediaAudioInfo, MediaCodec, MediaError, MediaPacket, MediaResult, MediaTrackInfo, MediaTrackType,
  MediaVideoInfo,
};
use std::fs::File;
use std::io::{BufReader, Read, Seek};
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

struct Mp4TrackCursor {
  id: u32,
  timescale: u32,
  sample_count: u32,
  next_sample: u32,
  peeked: Option<MediaPacket>,
}

/// Simple MP4 packet demuxer that yields H.264 + AAC packets in demux order.
///
/// Note: the existing `crate::media::mp4` module focuses on sample tables and efficient seeking,
/// whereas this type focuses on producing compressed packets with codec metadata for decoding.
pub struct Mp4PacketDemuxer<R: Read + Seek + Send> {
  mp4: mp4::Mp4Reader<R>,
  tracks: Vec<MediaTrackInfo>,
  cursors: Vec<Mp4TrackCursor>,
}

impl Mp4PacketDemuxer<BufReader<File>> {
  pub fn open(path: impl AsRef<Path>) -> MediaResult<Self> {
    let file = File::open(path.as_ref())?;
    let len = file.metadata()?.len();
    let reader = BufReader::new(file);
    let mp4 = mp4::Mp4Reader::read_header(reader, len)
      .map_err(|e| MediaError::Demux(format!("mp4: failed to read header: {e}")))?;
    Self::from_reader(mp4)
  }
}

impl<R: Read + Seek + Send> Mp4PacketDemuxer<R> {
  pub fn from_reader(mp4: mp4::Mp4Reader<R>) -> MediaResult<Self> {
    let mut tracks = Vec::new();
    let mut cursors = Vec::new();

    for (track_id, track) in mp4.tracks().iter() {
      let media_type = track
        .media_type()
        .map_err(|e| MediaError::Demux(format!("mp4: failed to get media type: {e}")))?;

      let timescale = track.trak.mdia.mdhd.timescale;
      let sample_count = track.sample_count();

      match media_type {
        mp4::MediaType::H264 => {
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
        mp4::MediaType::AAC => {
          let (sample_rate, channels) = mp4_track_audio_params(track)
            .map_err(|e| MediaError::Demux(format!("mp4: failed to read audio params: {e}")))?;
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
        _ => {}
      }

      if matches!(media_type, mp4::MediaType::H264 | mp4::MediaType::AAC) {
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

    Ok(Self { mp4, tracks, cursors })
  }
}

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
      pts_ns,
      duration_ns: 0,
      data: sample.bytes.to_vec(),
      is_keyframe: sample.is_sync,
    });
    return Ok(());
  }

  Ok(())
}

fn mp4_track_audio_params(track: &mp4::Mp4Track) -> std::result::Result<(u32, u16), String> {
  let mp4a = track
    .trak
    .mdia
    .minf
    .stbl
    .stsd
    .mp4a
    .as_ref()
    .ok_or_else(|| "mp4: AAC track missing mp4a sample entry".to_string())?;

  // For MP4 audio tracks, `mdhd.timescale` is typically the sample rate.
  let sample_rate = track.trak.mdia.mdhd.timescale;
  let channels = mp4a.channelcount;

  Ok((sample_rate, channels))
}

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
