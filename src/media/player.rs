//! Minimal per-element media player for WebM/VP9 video.
//!
//! This module is intentionally small: it is a single-stream (video-only) player that:
//! - demuxes VP9 packets from WebM (`WebmDemuxer`)
//! - decodes VP9 into RGBA8 frames (`codecs::vp9`)
//! - schedules presentation using `av_sync::decide_video_frame`
//! - exposes the most recently presented frame via a non-blocking `current_frame()` API.

use crate::media::av_sync::{decide_video_frame, AvSyncConfig, AvSyncDecision};
use crate::media::codecs::vp9::Vp9Decoder;
use crate::media::demux::webm::WebmDemuxer;
use crate::media::{MediaCodec, MediaError, MediaResult};
use crate::paint::display_list::ImageData;
use parking_lot::{Mutex, RwLock};
use std::collections::VecDeque;
use std::io::{Read, Seek};
use std::sync::Arc;
use std::time::{Duration, Instant};

const MAX_DEMUX_PACKETS_PER_TICK: usize = 128;
const MAX_VIDEO_FRAMES_PER_TICK: usize = 128;

// `dyn Read + Seek` is not a valid trait object (only one non-auto trait is allowed). Define a
// helper supertrait so we can store a boxed reader that supports both.
trait ReadSeek: Read + Seek {}
impl<T: Read + Seek> ReadSeek for T {}

/// Construction options for [`MediaPlayer`].
#[derive(Debug, Clone)]
pub struct MediaPlayerOptions {
  /// A/V sync thresholds.
  pub av_sync: AvSyncConfig,
  /// Number of threads to allow the VP9 decoder to use (libvpx internal threading).
  pub decode_threads: u32,
}

impl Default for MediaPlayerOptions {
  fn default() -> Self {
    let decode_threads = std::thread::available_parallelism()
      .map(|n| n.get() as u32)
      .unwrap_or(1)
      // libvpx internal threading has diminishing returns; keep it small and predictable.
      .min(4)
      .max(1);

    Self {
      av_sync: AvSyncConfig::from_env(),
      decode_threads,
    }
  }
}

struct QueuedVideoFrame {
  pts: Duration,
  image: Arc<ImageData>,
}

struct PlayerState {
  demuxer: WebmDemuxer<Box<dyn ReadSeek + Send>>,
  video_track_id: u64,
  vp9: Vp9Decoder,
  decode_threads: u32,
  video_queue: VecDeque<QueuedVideoFrame>,
  reached_eof: bool,

  playing: bool,
  /// Timeline time at `anchor_instant`.
  anchor_media_time: Duration,
  /// Instant corresponding to `anchor_media_time` while playing.
  anchor_instant: Instant,

  av_sync: AvSyncConfig,
}

impl PlayerState {
  fn timeline_now(&self, now: Instant) -> Duration {
    if !self.playing {
      return self.anchor_media_time;
    }

    let delta = now
      .checked_duration_since(self.anchor_instant)
      .unwrap_or(Duration::ZERO);
    self.anchor_media_time.saturating_add(delta)
  }
}

/// A minimal WebM/VP9 media player that can be ticked by a host and queried by paint.
///
/// The player is designed so that paint-facing APIs (`current_frame`, `next_wake_after`) are
/// non-blocking and never perform I/O or decode work.
pub struct MediaPlayer {
  state: Mutex<PlayerState>,
  current_frame: RwLock<Option<Arc<ImageData>>>,
  next_wake_after: Mutex<Option<Duration>>,
}

impl MediaPlayer {
  /// Open a WebM resource and initialize a VP9 decoder.
  pub fn open_webm(reader: impl Read + Seek + Send + 'static) -> MediaResult<Self> {
    Self::open_webm_with_options(reader, MediaPlayerOptions::default())
  }

  pub fn open_webm_with_options(
    reader: impl Read + Seek + Send + 'static,
    options: MediaPlayerOptions,
  ) -> MediaResult<Self> {
    let demuxer = WebmDemuxer::open(Box::new(reader) as Box<dyn ReadSeek + Send>)?;

    let video_track_id = demuxer
      .tracks()
      .iter()
      .find(|t| t.codec == MediaCodec::Vp9)
      .map(|t| t.id)
      .ok_or(MediaError::Unsupported("WebM file does not contain a VP9 video track"))?;

    let decode_threads = options.decode_threads.max(1);
    let vp9 = Vp9Decoder::new(decode_threads)?;

    Ok(Self {
      state: Mutex::new(PlayerState {
        demuxer,
        video_track_id,
        vp9,
        decode_threads,
        video_queue: VecDeque::new(),
        reached_eof: false,
        playing: false,
        anchor_media_time: Duration::ZERO,
        anchor_instant: Instant::now(),
        av_sync: options.av_sync,
      }),
      current_frame: RwLock::new(None),
      next_wake_after: Mutex::new(None),
    })
  }

  /// Start/resume playback.
  pub fn play(&self) {
    let now = Instant::now();
    let mut state = self.state.lock();
    if state.playing {
      return;
    }
    state.anchor_instant = now;
    state.playing = true;
    *self.next_wake_after.lock() = Some(Duration::ZERO);
  }

  /// Pause playback.
  pub fn pause(&self) {
    let now = Instant::now();
    let mut state = self.state.lock();
    if !state.playing {
      return;
    }
    state.anchor_media_time = state.timeline_now(now);
    state.anchor_instant = now;
    state.playing = false;
    *self.next_wake_after.lock() = None;
  }

  /// Best-effort seek to `time_ns` in the media timeline.
  pub fn seek(&self, time_ns: u64) -> MediaResult<()> {
    let now = Instant::now();

    // Prepare a fresh decoder first; if it fails we keep the old decoder state intact.
    let (decode_threads, av_sync) = {
      let state = self.state.lock();
      (state.decode_threads, state.av_sync)
    };
    let new_decoder = Vp9Decoder::new(decode_threads)?;

    let mut state = self.state.lock();
    state.demuxer.seek(time_ns)?;
    state.reached_eof = false;
    state.video_queue.clear();
    state.vp9 = new_decoder;

    state.anchor_media_time = Duration::from_nanos(time_ns);
    state.anchor_instant = now;
    // Preserve play/pause state; timeline mapping is updated above.
    state.av_sync = av_sync;

    *self.current_frame.write() = None;
    *self.next_wake_after.lock() = Some(Duration::ZERO);
    Ok(())
  }

  /// Returns the most recently presented decoded frame.
  ///
  /// This method is non-blocking: it never performs I/O, decode work, or waiting.
  pub fn current_frame(&self) -> Option<Arc<ImageData>> {
    self.current_frame.read().clone()
  }

  /// Returns the requested wake-up delay after the last [`tick`](Self::tick) call.
  pub fn next_wake_after(&self) -> Option<Duration> {
    *self.next_wake_after.lock()
  }

  /// Advance demux/decode/presentation state.
  ///
  /// Callers are expected to call this periodically when the player is in `play()` state.
  pub fn tick(&self) -> MediaResult<()> {
    let now = Instant::now();

    let mut wake_after: Option<Duration> = None;
    let mut last_presented: Option<Arc<ImageData>> = None;

    {
      let mut state = self.state.lock();

      if !state.playing {
        *self.next_wake_after.lock() = None;
        return Ok(());
      }

      let timeline_now = state.timeline_now(now);

      let mut demux_packets = 0usize;
      let mut processed_frames = 0usize;

      loop {
        if processed_frames >= MAX_VIDEO_FRAMES_PER_TICK || demux_packets >= MAX_DEMUX_PACKETS_PER_TICK {
          // Avoid spinning forever on malformed streams.
          wake_after = Some(Duration::ZERO);
          break;
        }

        // Ensure we have at least one decoded video frame to consider.
        while state.video_queue.is_empty() && !state.reached_eof && demux_packets < MAX_DEMUX_PACKETS_PER_TICK
        {
          demux_packets += 1;
          let Some(pkt) = state.demuxer.next_packet()? else {
            state.reached_eof = true;
            break;
          };

          if pkt.track_id != state.video_track_id {
            // Skip non-video packets (e.g. Opus audio).
            continue;
          }

          let decoded = state.vp9.decode(&pkt)?;
          for frame in decoded {
            let image = Arc::new(ImageData::new_pixels(frame.width, frame.height, frame.rgba8));
            state.video_queue.push_back(QueuedVideoFrame {
              pts: Duration::from_nanos(frame.pts_ns),
              image,
            });
          }
        }

        let Some(front) = state.video_queue.front() else {
          break;
        };

        match decide_video_frame(front.pts, timeline_now, &state.av_sync) {
          AvSyncDecision::Present => {
            processed_frames += 1;
            if let Some(frame) = state.video_queue.pop_front() {
              last_presented = Some(frame.image);
            }
          }
          AvSyncDecision::Drop => {
            processed_frames += 1;
            let _ = state.video_queue.pop_front();
          }
          AvSyncDecision::Hold { wake_after: wa } => {
            wake_after = Some(wa);
            break;
          }
        }
      }
    }

    if let Some(frame) = last_presented {
      *self.current_frame.write() = Some(frame);
    }
    *self.next_wake_after.lock() = wake_after;
    Ok(())
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use std::collections::hash_map::DefaultHasher;
  use std::hash::{Hash, Hasher};
  use std::io::Cursor;

  fn fixture_bytes() -> Vec<u8> {
    let path = crate::testing::fixture_path("pages/fixtures/media_playback/assets/test_vp9_opus.webm");
    std::fs::read(&path).unwrap_or_else(|e| panic!("read fixture {}: {e}", path.display()))
  }

  fn hash_frame(img: &ImageData) -> u64 {
    let mut hasher = DefaultHasher::new();
    img.width.hash(&mut hasher);
    img.height.hash(&mut hasher);
    img.pixels.hash(&mut hasher);
    hasher.finish()
  }

  #[test]
  fn player_decodes_a_frame() {
    let bytes = fixture_bytes();
    let player = MediaPlayer::open_webm(Cursor::new(bytes)).expect("open webm");

    player.play();

    for _ in 0..200 {
      player.tick().expect("tick");
      if player.current_frame().is_some() {
        return;
      }
    }

    panic!("expected current_frame() to become Some after ticking");
  }

  #[test]
  fn player_advances_frames_over_time() {
    let bytes = fixture_bytes();
    let player = MediaPlayer::open_webm(Cursor::new(bytes)).expect("open webm");

    player.play();

    // Wait for the first frame.
    let first = {
      let mut frame = None;
      for _ in 0..500 {
        player.tick().expect("tick");
        frame = player.current_frame();
        if frame.is_some() {
          break;
        }
      }
      frame.expect("first frame")
    };
    let first_hash = hash_frame(&first);

    // Keep ticking (sleeping based on `next_wake_after` so the real clock advances) until we see a
    // different frame.
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(2) {
      player.tick().expect("tick");
      if let Some(frame) = player.current_frame() {
        let h = hash_frame(&frame);
        if h != first_hash {
          return;
        }
      }

      if let Some(wake) = player.next_wake_after() {
        if !wake.is_zero() {
          std::thread::sleep(wake.min(Duration::from_millis(10)));
        }
      } else {
        // Avoid a tight spin loop if the player did not request a wake time.
        std::thread::sleep(Duration::from_millis(1));
      }
    }

    panic!("expected frame to change within 2s of playback");
  }
}
