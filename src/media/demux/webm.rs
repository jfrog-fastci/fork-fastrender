use crate::error::{RenderError, RenderStage};
use crate::media::track_selection::{
  select_primary_audio_track_id, select_primary_video_track_id, TrackCandidate, TrackFilterMode,
  TrackSelectionPolicy,
};
use crate::media::{
  MediaAudioInfo, MediaCodec, MediaError, MediaPacket, MediaResult, MediaTrackInfo, MediaTrackType,
  MediaVideoInfo,
};
use crate::render_control::{check_root, check_root_periodic};
use matroska_demuxer::{ContentEncodingValue, DemuxError, Frame, MatroskaFile, TrackType};
use std::collections::{HashMap, VecDeque};
use std::io::{self, Read, Seek, SeekFrom};

const WEBM_DEMUX_DEADLINE_STRIDE: usize = 1024;
const WEBM_DEMUX_IO_DEADLINE_STRIDE: usize = 8192;
/// Hard cap on encoded packet size to avoid unbounded memory usage on corrupted/adversarial WebM
/// files.
const MAX_WEBM_PACKET_BYTES: usize = 64 * 1024 * 1024;

fn check_webm_packet_size(track_id: u64, len: usize) -> MediaResult<()> {
  if len > MAX_WEBM_PACKET_BYTES {
    return Err(MediaError::Demux(format!(
      "WebM packet too large (track {track_id}, size {len} bytes, cap {MAX_WEBM_PACKET_BYTES} bytes)"
    )));
  }
  Ok(())
}

struct DeadlineReader<R> {
  inner: R,
  deadline_counter: usize,
}

impl<R> DeadlineReader<R> {
  fn new(inner: R) -> Self {
    Self {
      inner,
      deadline_counter: 0,
    }
  }

  fn check_deadline(&mut self) -> io::Result<()> {
    check_root_periodic(
      &mut self.deadline_counter,
      WEBM_DEMUX_IO_DEADLINE_STRIDE,
      RenderStage::Paint,
    )
    .map_err(|err| io::Error::new(io::ErrorKind::Other, err))
  }
}

impl<R: Read> Read for DeadlineReader<R> {
  fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
    self.check_deadline()?;
    self.inner.read(buf)
  }
}

impl<R: Seek> Seek for DeadlineReader<R> {
  fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
    self.check_deadline()?;
    self.inner.seek(pos)
  }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WebmContentEncodingKind {
  Unknown,
  Compression,
  Encryption,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
struct WebmTrackEncodingMeta {
  content_encodings: Vec<WebmContentEncodingKind>,
}

impl WebmTrackEncodingMeta {
  fn from_track(track: &matroska_demuxer::TrackEntry) -> Self {
    let Some(encodings) = track.content_encodings() else {
      return Self::default();
    };
    let content_encodings = encodings
      .iter()
      .map(|encoding| match encoding.encoding() {
        ContentEncodingValue::Encryption(_) => WebmContentEncodingKind::Encryption,
        ContentEncodingValue::Compression(_) => WebmContentEncodingKind::Compression,
        ContentEncodingValue::Unknown => WebmContentEncodingKind::Unknown,
      })
      .collect();
    Self { content_encodings }
  }
}

fn reject_unsupported_track_encodings(meta: &WebmTrackEncodingMeta) -> MediaResult<()> {
  if meta.content_encodings.is_empty() {
    return Ok(());
  }

  // Matroska `ContentEncodings` can describe encryption and/or compression. We don't support either
  // at the moment (no DRM/EME; no Matroska content compression codecs).
  //
  // Fail fast with an explicit Unsupported error so callers don't see opaque decode errors later
  // in the pipeline.
  let mut kinds: Vec<&'static str> = meta
    .content_encodings
    .iter()
    .map(|kind| match kind {
      WebmContentEncodingKind::Encryption => "encryption",
      WebmContentEncodingKind::Compression => "compression",
      WebmContentEncodingKind::Unknown => "unknown",
    })
    .collect();
  kinds.sort_unstable();
  kinds.dedup();
  let detail = if kinds.is_empty() {
    String::new()
  } else {
    format!(": {}", kinds.join("+"))
  };

  Err(MediaError::Unsupported(
    format!("encrypted (Matroska ContentEncodings{detail})").into(),
  ))
}

#[derive(Debug, Clone, Copy)]
pub struct WebmDemuxerOptions {
  /// If enabled, `next_packet()` yields packets ordered by PTS across tracks (video + audio), using
  /// a small bounded read-ahead buffer per track.
  pub inter_track_reordering: bool,
  /// Maximum number of queued packets per track when inter-track reordering is enabled.
  pub per_track_queue_capacity: usize,
  /// Policy used to pick the primary audio/video tracks when multiple tracks are present.
  pub track_selection_policy: TrackSelectionPolicy,
  /// Which tracks to emit packets for.
  pub track_filter: TrackFilterMode,
}

impl Default for WebmDemuxerOptions {
  fn default() -> Self {
    Self {
      inter_track_reordering: true,
      per_track_queue_capacity: 8,
      track_selection_policy: TrackSelectionPolicy::default(),
      track_filter: TrackFilterMode::PrimaryOnly,
    }
  }
}

#[derive(Debug, Clone)]
struct WebmTrackSelectionInfo {
  id: u64,
  track_type: TrackType,
  codec: MediaCodec,
  enabled: bool,
  default: bool,
  commentary: bool,
  hearing_impaired: bool,
  pixel_count: u64,
}

fn select_primary_track_ids(
  tracks: &[WebmTrackSelectionInfo],
  policy: TrackSelectionPolicy,
) -> (Option<u64>, Option<u64>) {
  let mut video_candidates = Vec::new();
  let mut audio_candidates = Vec::new();

  for t in tracks {
    match t.track_type {
      TrackType::Video => {
        if t.codec != MediaCodec::Vp9 {
          continue;
        }
        video_candidates.push(TrackCandidate {
          id: t.id,
          enabled: t.enabled,
          default: t.default,
          commentary: t.commentary,
          hearing_impaired: t.hearing_impaired,
          pixel_count: t.pixel_count,
        });
      }
      TrackType::Audio => {
        if t.codec != MediaCodec::Opus {
          continue;
        }
        audio_candidates.push(TrackCandidate {
          id: t.id,
          enabled: t.enabled,
          default: t.default,
          commentary: t.commentary,
          hearing_impaired: t.hearing_impaired,
          pixel_count: 0,
        });
      }
      _ => {}
    }
  }

  (
    select_primary_video_track_id(&video_candidates, policy),
    select_primary_audio_track_id(&audio_candidates, policy),
  )
}

pub struct WebmDemuxer<R: Read + Seek> {
  mkv: MatroskaFile<DeadlineReader<R>>,
  options: WebmDemuxerOptions,
  tracks: Vec<MediaTrackInfo>,
  primary_video_track_id: Option<u64>,
  primary_audio_track_id: Option<u64>,
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
    check_root(RenderStage::Paint).map_err(MediaError::from)?;

    if options.per_track_queue_capacity == 0 {
      return Err(MediaError::Unsupported(
        "invalid WebM demuxer queue capacity (must be >= 1)".into(),
      ));
    }

    let mkv = MatroskaFile::open(DeadlineReader::new(reader)).map_err(map_demux_error)?;
    let timestamp_scale_ns = mkv.info().timestamp_scale().get();

    let mut selection_infos = Vec::new();
    let mut supported_codec_delay_ns = HashMap::new();
    let mut tracks = Vec::new();

    for track in mkv.tracks() {
      let encoding_meta = WebmTrackEncodingMeta::from_track(track);
      reject_unsupported_track_encodings(&encoding_meta)?;

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

      if matches!(track.track_type(), TrackType::Video | TrackType::Audio) {
        let pixel_count = track
          .video()
          .map(|v| v.pixel_width().get().saturating_mul(v.pixel_height().get()))
          .unwrap_or(0);
        selection_infos.push(WebmTrackSelectionInfo {
          id,
          track_type: track.track_type(),
          codec: codec.clone(),
          enabled: track.flag_enabled(),
          default: track.flag_default(),
          commentary: track.flag_commentary(),
          hearing_impaired: track.flag_hearing_impaired(),
          pixel_count,
        });
      }

      // Store codec delay only for the codecs we currently emit packets for.
      if matches!(codec, MediaCodec::Vp9 | MediaCodec::Opus) {
        supported_codec_delay_ns.insert(id, codec_delay);
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

    let (primary_video_track_id, primary_audio_track_id) =
      select_primary_track_ids(&selection_infos, options.track_selection_policy);

    let mut active_track_ids = match options.track_filter {
      TrackFilterMode::AllTracks => supported_codec_delay_ns.keys().copied().collect(),
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

    let mut codec_delay_ns = HashMap::new();
    let mut max_codec_delay_ns = 0_u64;
    for id in &active_track_ids {
      if let Some(delay) = supported_codec_delay_ns.get(id) {
        codec_delay_ns.insert(*id, *delay);
        max_codec_delay_ns = max_codec_delay_ns.max(*delay);
      }
    }

    let mut packet_queues = HashMap::new();
    for &track_id in &active_track_ids {
      packet_queues.insert(
        track_id,
        VecDeque::with_capacity(options.per_track_queue_capacity),
      );
    }

    Ok(Self {
      mkv,
      options,
      tracks,
      primary_video_track_id,
      primary_audio_track_id,
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

  pub fn primary_video_track_id(&self) -> Option<u64> {
    self.primary_video_track_id
  }

  pub fn primary_audio_track_id(&self) -> Option<u64> {
    self.primary_audio_track_id
  }

  fn read_next_supported_packet(&mut self) -> MediaResult<Option<MediaPacket>> {
    check_root(RenderStage::Paint).map_err(MediaError::from)?;
    let mut deadline_counter = 0usize;

    loop {
      check_root_periodic(
        &mut deadline_counter,
        WEBM_DEMUX_DEADLINE_STRIDE,
        RenderStage::Paint,
      )
      .map_err(MediaError::from)?;

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
      check_webm_packet_size(self.frame.track, data.len())?;
      let is_keyframe = match self.frame.is_keyframe {
        Some(is_keyframe) => is_keyframe,
        None => {
          let is_vp9 = self
            .tracks
            .iter()
            .any(|track| track.id == self.frame.track && track.codec == MediaCodec::Vp9);
          if is_vp9 {
            vp9_is_keyframe(&data).unwrap_or(false)
          } else {
            false
          }
        }
      };

      return Ok(Some(MediaPacket {
        track_id: self.frame.track,
        dts_ns: pts_ns,
        pts_ns,
        duration_ns,
        data: data.into(),
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
        .expect("queue must exist for supported track"); // fastrender-allow-unwrap
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
    check_root(RenderStage::Paint).map_err(MediaError::from)?;

    if !self.options.inter_track_reordering || self.active_track_ids.len() <= 1 {
      return self.read_next_supported_packet();
    }

    self.fill_reorder_queues()?;
    Ok(self.pop_next_reordered_packet())
  }

  pub fn seek(&mut self, time_ns: u64) -> MediaResult<()> {
    check_root(RenderStage::Paint).map_err(MediaError::from)?;

    if self.timestamp_scale_ns == 0 {
      return Err(MediaError::Unsupported("invalid Matroska timestamp scale".into()));
    }

    // `codec_delay` must be subtracted from timestamps to get the actual PTS (see Matroska spec).
    // For a best-effort seek that guarantees `pts_ns >= time_ns` even after applying per-track
    // codec delay, we seek to `time_ns + max(codec_delay)` and then output PTS adjusted per track.
    let target_ns = time_ns.saturating_add(self.max_codec_delay_ns);

    // Convert nanoseconds to Matroska timecode units (inverse of timestamp_scale).
    // `MatroskaFile::seek()` places the cursor on the first frame with timestamp >= seek_timestamp.
    let seek_timestamp =
      target_ns.saturating_add(self.timestamp_scale_ns.saturating_sub(1)) / self.timestamp_scale_ns;

    // Seeking invalidates any queued packets from the old position.
    for q in self.packet_queues.values_mut() {
      q.clear();
    }
    self.reached_eof = false;

    self.mkv.seek(seek_timestamp).map_err(|err| match err {
      // When seeking in damaged/unindexed files, the demuxer may not be able to locate clusters.
      DemuxError::CantFindCluster => {
        MediaError::Unsupported("Matroska seek unsupported (no cluster index)".into())
      }
      other => map_demux_error(other),
    })
  }
}

fn map_demux_error(err: DemuxError) -> MediaError {
  match err {
    DemuxError::IoError(err) => {
      if let Some(render_err) = err
        .get_ref()
        .and_then(|source| source.downcast_ref::<RenderError>())
      {
        return MediaError::Render(render_err.clone());
      }
      MediaError::Io(err)
    }
    other => MediaError::Demux(other.to_string()),
  }
}

fn vp9_is_keyframe(frame: &[u8]) -> MediaResult<bool> {
  let byte0 = *frame
    .first()
    .ok_or_else(|| MediaError::Demux("VP9 header truncated".to_string()))?;

  // VP9 uncompressed header bit layout (byte 0):
  // - frame_marker: 2 bits (must be 0b10)
  // - profile_low_bit: 1 bit
  // - profile_high_bit: 1 bit
  // - if profile == 3: reserved_zero_bit: 1 bit
  // - show_existing_frame: 1 bit
  // - if show_existing_frame == 0: frame_type: 1 bit (0 keyframe, 1 inter)
  let frame_marker = byte0 >> 6;
  if frame_marker != 0b10 {
    return Err(MediaError::Demux(format!(
      "invalid VP9 frame marker: {frame_marker:#b}"
    )));
  }

  // Note: VP9 stores the profile bits in low/high order (low bit first).
  let profile_low = (byte0 >> 5) & 1;
  let profile_high = (byte0 >> 4) & 1;
  let profile = profile_low | (profile_high << 1);

  if profile == 3 {
    let reserved = (byte0 >> 3) & 1;
    if reserved != 0 {
      return Err(MediaError::Demux(
        "invalid VP9 reserved profile bit".to_string(),
      ));
    }

    let show_existing_frame = (byte0 >> 2) & 1;
    if show_existing_frame == 1 {
      return Ok(false);
    }
    let frame_type = (byte0 >> 1) & 1;
    return Ok(frame_type == 0);
  }

  let show_existing_frame = (byte0 >> 3) & 1;
  if show_existing_frame == 1 {
    return Ok(false);
  }
  let frame_type = (byte0 >> 2) & 1;
  Ok(frame_type == 0)
}

#[cfg(test)]
mod tests {
  use super::*;
  use std::io::Cursor;
  use std::path::PathBuf;
  use std::time::Duration;

  #[test]
  fn selects_highest_resolution_video_track_among_preferred_candidates() {
    let policy = TrackSelectionPolicy::default();
    let tracks = vec![
      WebmTrackSelectionInfo {
        id: 1,
        track_type: TrackType::Video,
        codec: MediaCodec::Vp9,
        enabled: true,
        default: true,
        commentary: false,
        hearing_impaired: false,
        pixel_count: 640 * 360,
      },
      WebmTrackSelectionInfo {
        id: 2,
        track_type: TrackType::Video,
        codec: MediaCodec::Vp9,
        enabled: true,
        default: true,
        commentary: false,
        hearing_impaired: false,
        pixel_count: 1920 * 1080,
      },
      WebmTrackSelectionInfo {
        id: 10,
        track_type: TrackType::Audio,
        codec: MediaCodec::Opus,
        enabled: true,
        default: true,
        commentary: false,
        hearing_impaired: false,
        pixel_count: 0,
      },
    ];

    let (video, audio) = select_primary_track_ids(&tracks, policy);
    assert_eq!(video, Some(2));
    assert_eq!(audio, Some(10));
  }

  #[test]
  fn prefers_default_video_track_over_non_default_even_if_lower_resolution() {
    let policy = TrackSelectionPolicy::default();
    let tracks = vec![
      WebmTrackSelectionInfo {
        id: 1,
        track_type: TrackType::Video,
        codec: MediaCodec::Vp9,
        enabled: true,
        default: true,
        commentary: false,
        hearing_impaired: false,
        pixel_count: 640 * 360,
      },
      WebmTrackSelectionInfo {
        id: 2,
        track_type: TrackType::Video,
        codec: MediaCodec::Vp9,
        enabled: true,
        default: false,
        commentary: false,
        hearing_impaired: false,
        pixel_count: 1920 * 1080,
      },
    ];

    let (video, _) = select_primary_track_ids(&tracks, policy);
    assert_eq!(video, Some(1));
  }

  #[test]
  fn avoids_commentary_audio_when_alternative_exists() {
    let policy = TrackSelectionPolicy::default();
    let tracks = vec![
      WebmTrackSelectionInfo {
        id: 1,
        track_type: TrackType::Audio,
        codec: MediaCodec::Opus,
        enabled: true,
        default: true,
        commentary: true,
        hearing_impaired: false,
        pixel_count: 0,
      },
      WebmTrackSelectionInfo {
        id: 2,
        track_type: TrackType::Audio,
        codec: MediaCodec::Opus,
        enabled: true,
        default: true,
        commentary: false,
        hearing_impaired: false,
        pixel_count: 0,
      },
    ];

    let (_, audio) = select_primary_track_ids(&tracks, policy);
    assert_eq!(audio, Some(2));
  }

  #[test]
  fn falls_back_to_commentary_audio_when_no_alternative_exists() {
    let policy = TrackSelectionPolicy::default();
    let tracks = vec![WebmTrackSelectionInfo {
      id: 99,
      track_type: TrackType::Audio,
      codec: MediaCodec::Opus,
      enabled: true,
      default: true,
      commentary: true,
      hearing_impaired: false,
      pixel_count: 0,
    }];

    let (_, audio) = select_primary_track_ids(&tracks, policy);
    assert_eq!(audio, Some(99));
  }

  fn webm_fixture_bytes(name: &str) -> Vec<u8> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
      .join("tests/fixtures/media")
      .join(name);
    std::fs::read(&path).expect("read WebM fixture")
  }

  struct TestRenderDelayGuard;

  impl TestRenderDelayGuard {
    fn set(ms: Option<u64>) -> Self {
      crate::render_control::set_test_render_delay_ms(ms);
      Self
    }
  }

  impl Drop for TestRenderDelayGuard {
    fn drop(&mut self) {
      crate::render_control::set_test_render_delay_ms(None);
    }
  }

  #[test]
  fn demux_respects_render_deadline() {
    // Use a delay larger than the overall timeout so a single deadline check reliably triggers a
    // timeout regardless of host speed/caching.
    let _guard = TestRenderDelayGuard::set(Some(50));

    let bytes = webm_fixture_bytes("vp9_opus.webm");
    let mut demuxer = WebmDemuxer::open(Cursor::new(bytes.as_slice())).expect("open webm");

    let deadline =
      crate::render_control::RenderDeadline::new(Some(Duration::from_millis(10)), None);
    let err = crate::render_control::with_deadline(Some(&deadline), || demuxer.next_packet())
      .expect_err("expected timeout");

    match &err {
      MediaError::Render(RenderError::Timeout { .. }) => {}
      other => panic!("expected render timeout, got {other:?}"),
    }

    let top: crate::error::Error = err.into();
    assert!(top.is_timeout(), "expected top-level timeout, got {top:?}");
    match top {
      crate::error::Error::Render(RenderError::Timeout { .. }) => {}
      other => panic!("expected top-level render timeout, got {other:?}"),
    }
  }

  #[test]
  fn rejects_tracks_with_content_encodings() {
    let meta = WebmTrackEncodingMeta {
      content_encodings: vec![WebmContentEncodingKind::Encryption],
    };
    let err = reject_unsupported_track_encodings(&meta).expect_err("expected unsupported error");
    let MediaError::Unsupported(msg) = err else {
      panic!("expected MediaError::Unsupported, got {err:?}");
    };
    assert!(
      msg.contains("encrypted"),
      "expected error message to mention encryption, got {msg}"
    );
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
        ..Default::default()
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

  #[test]
  fn vp9_keyframe_detection_reports_keyframe() {
    // frame_marker=0b10, profile=0, show_existing_frame=0, frame_type=0 (keyframe)
    let data = [0x82_u8];
    assert_eq!(vp9_is_keyframe(&data).unwrap(), true);
  }

  #[test]
  fn vp9_keyframe_detection_reports_interframe() {
    // frame_marker=0b10, profile=0, show_existing_frame=0, frame_type=1 (interframe)
    let data = [0x86_u8];
    assert_eq!(vp9_is_keyframe(&data).unwrap(), false);
  }

  #[test]
  fn vp9_keyframe_detection_reports_show_existing_frame_as_non_keyframe() {
    // frame_marker=0b10, profile=0, show_existing_frame=1
    let data = [0x88_u8];
    assert_eq!(vp9_is_keyframe(&data).unwrap(), false);
  }

  #[test]
  fn vp9_keyframe_detection_handles_profile3_bit_packing() {
    // frame_marker=0b10, profile=3 (11), reserved=0, show_existing_frame=0, frame_type=0
    let keyframe = [0xB0_u8];
    assert_eq!(vp9_is_keyframe(&keyframe).unwrap(), true);

    // frame_type=1
    let interframe = [0xB2_u8];
    assert_eq!(vp9_is_keyframe(&interframe).unwrap(), false);
  }

  #[test]
  fn vp9_keyframe_detection_rejects_invalid_frame_marker() {
    let err = vp9_is_keyframe(&[0x00_u8]).expect_err("invalid marker should error");
    assert!(matches!(err, MediaError::Demux(_)));
  }

  #[test]
  fn vp9_keyframe_detection_rejects_truncated_header() {
    let err = vp9_is_keyframe(&[]).expect_err("empty frame should error");
    assert!(matches!(err, MediaError::Demux(_)));
  }

  #[test]
  fn vp9_keyframe_detection_rejects_profile3_reserved_bit_set() {
    // frame_marker=0b10, profile=3 (11), reserved=1 (invalid)
    let err = vp9_is_keyframe(&[0xB8_u8]).expect_err("reserved bit set should error");
    let MediaError::Demux(msg) = err else {
      panic!("expected demux error, got {err:?}");
    };
    assert!(
      msg.contains("reserved"),
      "expected error mentioning reserved bit, got {msg:?}"
    );
  }

  #[test]
  fn rejects_oversized_webm_packet() {
    let err =
      check_webm_packet_size(7, MAX_WEBM_PACKET_BYTES + 1).expect_err("expected size cap error");
    let MediaError::Demux(msg) = err else {
      panic!("expected demux error, got {err:?}");
    };
    assert!(
      msg.contains(&MAX_WEBM_PACKET_BYTES.to_string()),
      "expected error mentioning cap, got {msg:?}"
    );
  }
}
