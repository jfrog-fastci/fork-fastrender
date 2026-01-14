use crate::media::clock::{MediaClock, PlaybackClock, PlaybackState};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

fn duration_to_nanos_u64(duration: Duration) -> u64 {
  u64::try_from(duration.as_nanos()).unwrap_or(u64::MAX)
}

/// Thread-safe playback state shared between HTMLMediaElement bindings and the media decode loop.
///
/// This wraps [`PlaybackClock`] (which tracks play/pause/rate) and adds a lightweight seek
/// notification mechanism so background decode threads can react promptly to seeks.
#[derive(Debug)]
pub struct MediaPlaybackControl {
  clock: PlaybackClock,
  seek_seq: AtomicU64,
  seek_target_nanos: AtomicU64,
}

impl MediaPlaybackControl {
  /// Creates a new playback controller pinned to `0s` and initially **paused**.
  pub fn new(master_clock: Arc<dyn MediaClock>) -> Self {
    let clock = PlaybackClock::new(master_clock, Duration::ZERO);
    // HTMLMediaElement starts out paused.
    clock.pause();
    Self {
      clock,
      seek_seq: AtomicU64::new(0),
      seek_target_nanos: AtomicU64::new(0),
    }
  }

  /// Returns the current playback state (playing vs paused).
  #[must_use]
  pub fn state(&self) -> PlaybackState {
    self.clock.state()
  }

  /// Returns the current media timeline time.
  #[must_use]
  pub fn now(&self) -> Duration {
    self.clock.now()
  }

  pub fn play(&self) {
    self.clock.play();
  }

  pub fn pause(&self) {
    self.clock.pause();
  }

  pub fn rate(&self) -> f64 {
    self.clock.rate()
  }

  pub fn set_rate(&self, rate: f64) {
    self.clock.set_rate(rate);
  }

  /// Seeks to `time` and notifies listeners.
  ///
  /// This updates the underlying clock immediately and increments an internal seek counter so
  /// background media workers can seek their demux/decoder state.
  pub fn seek(&self, time: Duration) {
    self.clock.seek(time);
    self
      .seek_target_nanos
      .store(duration_to_nanos_u64(time), Ordering::Relaxed);
    let _ = self.seek_seq.fetch_add(1, Ordering::Relaxed);
  }

  /// Returns a monotonically increasing seek sequence number.
  #[must_use]
  pub fn seek_seq(&self) -> u64 {
    self.seek_seq.load(Ordering::Relaxed)
  }

  /// Returns the most recent seek target.
  #[must_use]
  pub fn seek_target(&self) -> Duration {
    Duration::from_nanos(self.seek_target_nanos.load(Ordering::Relaxed))
  }
}

