//! Media clocking primitives (master clock + per-stream timeline mapping).
//!
//! This module defines the clock abstraction used by media playback for A/V sync and
//! `HTMLMediaElement.currentTime` bookkeeping.
//!
//! Key idea: the UI/event-loop tick is only a wake-up mechanism; it is **not** a time source. Media
//! time is derived from a chosen master clock (audio device time when audio is present).
//!
//! For the full intended clocking model and drift-bug checklist, see `docs/media_clocking.md`.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::clock::Clock;

/// Abstraction over the master clock used for A/V sync and HTMLMediaElement timekeeping.
///
/// The clock origin is intentionally unspecified; callers should only compute deltas.
///
/// # Semantics
///
/// When this clock is backed by an audio output device (the preferred A/V sync master), `now()`
/// should represent the time of the audio that is being *heard* at the output (speaker), not merely
/// the time at which samples are being *written* into the device callback.
///
/// Backends that only observe callback time or output frame counters should compensate using a
/// best-effort output latency estimate (see `AudioBackend::output_info()` in `src/media/audio/`).
pub trait MediaClock: Send + Sync + 'static {
  fn now(&self) -> Duration;

  /// Whether this clock has started producing valid timestamps.
  ///
  /// System clocks are generally started immediately. Audio clocks often cannot provide a stable
  /// clock until the audio device has consumed (or committed to consuming) the first sample.
  ///
  /// A default implementation returns `true` so existing monotonic clocks do not need to override
  /// this method.
  fn is_started(&self) -> bool {
    true
  }
}

/// Adapter that exposes a [`crate::clock::Clock`] as a [`MediaClock`].
///
/// This is useful when media clocking needs to reuse the host's injectable clock
/// (e.g. deterministic JS tests with [`crate::clock::VirtualClock`]).
#[derive(Clone)]
pub struct ClockMediaClock {
  clock: Arc<dyn Clock>,
}

impl std::fmt::Debug for ClockMediaClock {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("ClockMediaClock")
      .field("clock", &"<dyn Clock>")
      .finish()
  }
}

impl ClockMediaClock {
  #[must_use]
  pub fn new(clock: Arc<dyn Clock>) -> Self {
    Self { clock }
  }
}

impl MediaClock for ClockMediaClock {
  fn now(&self) -> Duration {
    self.clock.now()
  }
}

/// Shared clock representing the output device's timebase (e.g. audio hardware clock).
///
/// The audio mixer can share a single instance across all audio streams.
pub type AudioDeviceClock = dyn MediaClock;

/// Default real-time implementation of [`AudioDeviceClock`], backed by [`Instant`].
///
/// This is a sensible fallback for non-audio environments, but it does **not** account for audio
/// output latency. When used as an audio master clock, it should be considered “time of samples
/// being written” rather than “time of samples being heard”.
#[derive(Debug)]
pub struct RealAudioDeviceClock {
  start: Instant,
}

/// Alias for callers that want a monotonic system-time clock as the base for media playback.
pub type SystemMediaClock = RealAudioDeviceClock;

impl Default for RealAudioDeviceClock {
  fn default() -> Self {
    Self {
      start: Instant::now(),
    }
  }
}

impl MediaClock for RealAudioDeviceClock {
  fn now(&self) -> Duration {
    self.start.elapsed()
  }
}

/// A per-stream media clock derived from a shared [`AudioDeviceClock`].
///
/// Each media element gets its own instance so `currentTime` can be tracked independently even
/// when all audio is mixed into a single output device.
pub struct AudioStreamClock {
  device_clock: Arc<AudioDeviceClock>,
  /// Output latency/preroll in nanoseconds.
  ///
  /// This is modeled as a constant delay between "the device clock position we can observe" and
  /// "the audio that is actually audible". The stream timeline will not advance until the device
  /// clock has advanced by at least this amount, so `currentTime` aligns to first audible audio.
  preroll_nanos: AtomicU64,
  /// Device timestamp corresponding to `start_media_time` (in nanoseconds).
  start_device_time: AtomicU64,
  /// Media timestamp corresponding to `start_device_time` (in nanoseconds).
  start_media_time: AtomicU64,
  /// Playback rate as IEEE-754 bits.
  ///
  /// Stored as f64 bits inside an atomic so audio/video threads can read it without locks.
  rate: AtomicU64,
  /// Last returned timestamp (in nanoseconds) used to clamp to monotonic if the device clock jumps
  /// backwards.
  last_now: AtomicU64,
}

impl std::fmt::Debug for AudioStreamClock {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("AudioStreamClock")
      .field("device_clock", &"<dyn MediaClock>")
      .field("preroll", &self.preroll())
      .field(
        "start_device_time",
        &Duration::from_nanos(self.start_device_time.load(Ordering::Relaxed)),
      )
      .field(
        "start_media_time",
        &Duration::from_nanos(self.start_media_time.load(Ordering::Relaxed)),
      )
      .field("rate", &self.rate())
      .finish()
  }
}

impl AudioStreamClock {
  pub fn new(device_clock: Arc<AudioDeviceClock>, start_media_time: Duration) -> Self {
    Self::new_with_preroll(device_clock, start_media_time, Duration::ZERO)
  }

  /// Creates a new stream clock with an output preroll/latency model.
  ///
  /// The stream timeline will remain pinned to `start_media_time` until the device clock has
  /// advanced by `preroll`. This keeps `now()` aligned to the first audio that should be audible
  /// (instead of advancing during initial buffering/output latency).
  pub fn new_with_preroll(
    device_clock: Arc<AudioDeviceClock>,
    start_media_time: Duration,
    preroll: Duration,
  ) -> Self {
    let preroll_nanos = duration_to_nanos_u64(preroll);
    let start_device_time = device_clock
      .now()
      .saturating_add(Duration::from_nanos(preroll_nanos));
    let start_media_nanos = duration_to_nanos_u64(start_media_time);
    Self {
      device_clock,
      preroll_nanos: AtomicU64::new(preroll_nanos),
      start_device_time: AtomicU64::new(duration_to_nanos_u64(start_device_time)),
      start_media_time: AtomicU64::new(start_media_nanos),
      rate: AtomicU64::new(1.0_f64.to_bits()),
      last_now: AtomicU64::new(start_media_nanos),
    }
  }

  /// Returns the configured preroll/latency model.
  #[must_use]
  pub fn preroll(&self) -> Duration {
    Duration::from_nanos(self.preroll_nanos.load(Ordering::Relaxed))
  }

  /// (Re-)configures the output preroll/latency model.
  ///
  /// This keeps `now()` continuous by re-anchoring the mapping at the current device time.
  pub fn set_preroll(&self, preroll: Duration) {
    let preroll_nanos = duration_to_nanos_u64(preroll);
    self.preroll_nanos.store(preroll_nanos, Ordering::Relaxed);

    // Keep `now()` continuous at the moment we update the preroll.
    let media_now = self.now();
    let device_now = self.device_clock.now();

    let media_now_nanos = duration_to_nanos_u64(media_now);
    self
      .start_media_time
      .store(media_now_nanos, Ordering::Relaxed);
    self.last_now.store(media_now_nanos, Ordering::Relaxed);

    let start_device_time = duration_to_nanos_u64(device_now).saturating_add(preroll_nanos);
    self
      .start_device_time
      .store(start_device_time, Ordering::Relaxed);
  }

  /// Returns `Some(device_time)` once the stream timeline has started advancing (i.e. once the
  /// device clock has advanced past the preroll threshold).
  #[must_use]
  pub fn playback_started_at(&self) -> Option<Duration> {
    let device_now = duration_to_nanos_u64(self.device_clock.now());
    let start_device = self.start_device_time.load(Ordering::Relaxed);
    if device_now >= start_device {
      Some(Duration::from_nanos(start_device))
    } else {
      None
    }
  }

  pub fn rate(&self) -> f64 {
    f64::from_bits(self.rate.load(Ordering::Relaxed))
  }

  /// Adjust the playback rate while keeping the returned `now()` continuous.
  pub fn set_rate(&self, new_rate: f64) {
    // Keep behaviour deterministic and panic-free; treat invalid rates as 0 (paused).
    let new_rate = if new_rate.is_finite() && new_rate > 0.0 {
      new_rate
    } else {
      0.0
    };

    // Capture current mapping so the rate change does not introduce a discontinuity.
    let media_now = self.now();
    let device_now = self.device_clock.now();
    let preroll_nanos = self.preroll_nanos.load(Ordering::Relaxed);

    let media_now_nanos = duration_to_nanos_u64(media_now);
    self.start_device_time.store(
      duration_to_nanos_u64(device_now).saturating_add(preroll_nanos),
      Ordering::Relaxed,
    );
    self
      .start_media_time
      .store(media_now_nanos, Ordering::Relaxed);
    self.last_now.store(media_now_nanos, Ordering::Relaxed);
    self.rate.store(new_rate.to_bits(), Ordering::Relaxed);
  }

  /// Resets the mapping so that `now()` returns `new_media_time` at the current device time.
  pub fn seek(&self, new_media_time: Duration) {
    let device_now = self.device_clock.now();
    let preroll_nanos = self.preroll_nanos.load(Ordering::Relaxed);
    let new_media_nanos = duration_to_nanos_u64(new_media_time);
    self.start_device_time.store(
      duration_to_nanos_u64(device_now).saturating_add(preroll_nanos),
      Ordering::Relaxed,
    );
    self
      .start_media_time
      .store(new_media_nanos, Ordering::Relaxed);
    self.last_now.store(new_media_nanos, Ordering::Relaxed);
  }

  fn compute_now_nanos(&self) -> u64 {
    let device_now = duration_to_nanos_u64(self.device_clock.now());
    let start_device = self.start_device_time.load(Ordering::Relaxed);
    let start_media = self.start_media_time.load(Ordering::Relaxed);
    let rate = self.rate();

    let delta_device_nanos = device_now.saturating_sub(start_device);
    let scaled_delta_nanos = scale_nanos(delta_device_nanos, rate);
    let candidate = start_media.saturating_add(scaled_delta_nanos);

    // Clamp to monotonic if the device clock jumps backwards or the mapping changes unexpectedly.
    let mut last = self.last_now.load(Ordering::Relaxed);
    loop {
      if candidate <= last {
        return last;
      }

      match self.last_now.compare_exchange_weak(
        last,
        candidate,
        Ordering::Relaxed,
        Ordering::Relaxed,
      ) {
        Ok(_) => return candidate,
        Err(observed) => last = observed,
      }
    }
  }
}

impl MediaClock for AudioStreamClock {
  fn now(&self) -> Duration {
    Duration::from_nanos(self.compute_now_nanos())
  }
}

/// Whether a [`PlaybackClock`] is currently advancing (`Playing`) or frozen (`Paused`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlaybackState {
  Playing,
  Paused,
}

/// Mapping from a chosen *master clock* (audio device clock or system monotonic clock) to a media
/// **timeline time** that supports pause/seek/playbackRate.
///
/// This is the canonical mapping described in `docs/media_clocking.md`:
///
/// ```text
/// if playing:
///   timeline_now = base_timeline_time + (master_now - base_master_time) * playback_rate
/// else:
///   timeline_now = base_timeline_time
/// ```
///
/// ## Precision notes
///
/// Playback rate is stored as an `f64` multiplier. To avoid accumulating floating-point error over
/// time (drift), the rate is applied to the **absolute** master-clock delta from a stored origin and
/// rounded once to the nearest nanosecond, similar to [`AudioStreamClock`].
///
/// The clock exposes a lightweight [`PlaybackState`] so callers can query whether it is currently
/// advancing (`Playing`) or frozen (`Paused`).
///
/// Note: `playbackRate` of 0 is treated as a valid value (the timeline simply does not advance while
/// `playing` remains `true`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlaybackState {
  Playing,
  Paused,
}

pub struct PlaybackClock {
  master_clock: Arc<dyn MediaClock>,

  /// Master timestamp corresponding to `base_timeline_time` (nanoseconds).
  base_master_time: AtomicU64,
  /// Timeline timestamp corresponding to `base_master_time` (nanoseconds).
  base_timeline_time: AtomicU64,
  /// Playback rate as IEEE-754 bits.
  rate: AtomicU64,
  /// Whether the timeline is currently advancing.
  playing: AtomicBool,
  /// Last returned timestamp (nanoseconds), used to clamp to monotonic while playing.
  ///
  /// This is reset on explicit seeks (including backwards seeks).
  last_now: AtomicU64,
}

impl std::fmt::Debug for PlaybackClock {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("PlaybackClock")
      .field("master_clock", &"<dyn MediaClock>")
      .field(
        "base_master_time",
        &Duration::from_nanos(self.base_master_time.load(Ordering::Relaxed)),
      )
      .field(
        "base_timeline_time",
        &Duration::from_nanos(self.base_timeline_time.load(Ordering::Relaxed)),
      )
      .field("rate", &self.rate())
      .field("playing", &self.playing.load(Ordering::Relaxed))
      .finish()
  }
}

impl PlaybackClock {
  /// Creates a new playback clock with the given master clock and initial timeline time.
  ///
  /// The clock starts in the **playing** state (timeline advances as the master clock advances).
  pub fn new(master_clock: Arc<dyn MediaClock>, start_time: Duration) -> Self {
    let master_now = master_clock.now();
    let start_nanos = duration_to_nanos_u64(start_time);
    Self {
      master_clock,
      base_master_time: AtomicU64::new(duration_to_nanos_u64(master_now)),
      base_timeline_time: AtomicU64::new(start_nanos),
      rate: AtomicU64::new(1.0_f64.to_bits()),
      playing: AtomicBool::new(true),
      last_now: AtomicU64::new(start_nanos),
    }
  }

  #[must_use]
  pub fn state(&self) -> PlaybackState {
    if self.playing.load(Ordering::Relaxed) {
      PlaybackState::Playing
    } else {
      PlaybackState::Paused
    }
  }

  pub fn play(&self) {
    // Capture current frozen timeline time so the transition is continuous.
    let timeline_now = self.now();
    let master_now = self.master_clock.now();
    let timeline_now_nanos = duration_to_nanos_u64(timeline_now);
    self
      .base_master_time
      .store(duration_to_nanos_u64(master_now), Ordering::Relaxed);
    self
      .base_timeline_time
      .store(timeline_now_nanos, Ordering::Relaxed);
    self.last_now.store(timeline_now_nanos, Ordering::Relaxed);
    self.playing.store(true, Ordering::Relaxed);
  }

  pub fn pause(&self) {
    // Freeze at the current timeline time.
    let timeline_now = self.now();
    let master_now = self.master_clock.now();
    let timeline_now_nanos = duration_to_nanos_u64(timeline_now);
    self
      .base_master_time
      .store(duration_to_nanos_u64(master_now), Ordering::Relaxed);
    self
      .base_timeline_time
      .store(timeline_now_nanos, Ordering::Relaxed);
    self.last_now.store(timeline_now_nanos, Ordering::Relaxed);
    self.playing.store(false, Ordering::Relaxed);
  }

  /// Jumps the timeline to `new_time` and continues from there (if playing).
  pub fn seek(&self, new_time: Duration) {
    let master_now = self.master_clock.now();
    let new_nanos = duration_to_nanos_u64(new_time);
    self
      .base_master_time
      .store(duration_to_nanos_u64(master_now), Ordering::Relaxed);
    self.base_timeline_time.store(new_nanos, Ordering::Relaxed);
    // Explicit seeks are allowed to go backwards, so reset the monotonic clamp.
    self.last_now.store(new_nanos, Ordering::Relaxed);
  }

  pub fn rate(&self) -> f64 {
    f64::from_bits(self.rate.load(Ordering::Relaxed))
  }

  /// Adjusts the playback rate while keeping `now()` continuous.
  pub fn set_rate(&self, new_rate: f64) {
    let new_rate = if new_rate.is_finite() && new_rate > 0.0 {
      new_rate
    } else {
      0.0
    };

    // Capture current mapping so the rate change does not introduce a discontinuity.
    let timeline_now = self.now();
    let master_now = self.master_clock.now();

    let timeline_now_nanos = duration_to_nanos_u64(timeline_now);
    self
      .base_master_time
      .store(duration_to_nanos_u64(master_now), Ordering::Relaxed);
    self
      .base_timeline_time
      .store(timeline_now_nanos, Ordering::Relaxed);
    self.last_now.store(timeline_now_nanos, Ordering::Relaxed);
    self.rate.store(new_rate.to_bits(), Ordering::Relaxed);
  }

  fn compute_now_nanos(&self) -> u64 {
    let base_timeline = self.base_timeline_time.load(Ordering::Relaxed);

    if !self.playing.load(Ordering::Relaxed) {
      return base_timeline;
    }

    let master_now = duration_to_nanos_u64(self.master_clock.now());
    let base_master = self.base_master_time.load(Ordering::Relaxed);
    let rate = self.rate();

    let delta_master_nanos = master_now.saturating_sub(base_master);
    let scaled_delta_nanos = scale_nanos(delta_master_nanos, rate);
    let candidate = base_timeline.saturating_add(scaled_delta_nanos);

    // Clamp to monotonic while playing if the master clock jumps backwards.
    let mut last = self.last_now.load(Ordering::Relaxed);
    loop {
      if candidate <= last {
        return last;
      }

      match self.last_now.compare_exchange_weak(
        last,
        candidate,
        Ordering::Relaxed,
        Ordering::Relaxed,
      ) {
        Ok(_) => return candidate,
        Err(observed) => last = observed,
      }
    }
  }

  /// Returns the current media timeline time.
  pub fn now(&self) -> Duration {
    Duration::from_nanos(self.compute_now_nanos())
  }
}

impl MediaClock for PlaybackClock {
  fn now(&self) -> Duration {
    PlaybackClock::now(self)
  }

  fn is_started(&self) -> bool {
    self.master_clock.is_started()
  }
}
fn duration_to_nanos_u64(duration: Duration) -> u64 {
  // Duration::as_nanos returns u128.
  u64::try_from(duration.as_nanos()).unwrap_or(u64::MAX)
}

fn scale_nanos(nanos: u64, rate: f64) -> u64 {
  if !(rate.is_finite()) || rate <= 0.0 {
    return 0;
  }
  // Keep the mapping stable by rounding once (from the same absolute origin) instead of
  // incrementally accumulating floating-point error.
  let scaled = (nanos as f64) * rate;
  if !(scaled.is_finite()) || scaled <= 0.0 {
    return 0;
  }
  // Round to the nearest nanosecond.
  u64::try_from(scaled.round() as u128).unwrap_or(u64::MAX)
}
#[cfg(test)]
mod tests {
  use super::*;

  #[derive(Debug, Default)]
  struct FakeDeviceClock {
    now_nanos: AtomicU64,
  }

  impl FakeDeviceClock {
    fn set_now(&self, now: Duration) {
      self
        .now_nanos
        .store(duration_to_nanos_u64(now), Ordering::Relaxed);
    }

    fn advance(&self, delta: Duration) {
      let delta_nanos = duration_to_nanos_u64(delta);
      let _ = self
        .now_nanos
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
          Some(current.saturating_add(delta_nanos))
        });
    }
  }

  impl MediaClock for FakeDeviceClock {
    fn now(&self) -> Duration {
      Duration::from_nanos(self.now_nanos.load(Ordering::Relaxed))
    }
  }

  #[test]
  fn maps_device_clock_to_media_clock() {
    let device_clock = Arc::new(FakeDeviceClock::default());
    device_clock.set_now(Duration::from_secs(10));

    let stream_clock = AudioStreamClock::new(device_clock.clone(), Duration::from_secs(5));

    device_clock.advance(Duration::from_secs(3));
    assert_eq!(stream_clock.now(), Duration::from_secs(8));
  }

  #[test]
  fn clamps_monotonic_if_device_clock_goes_backwards() {
    let device_clock = Arc::new(FakeDeviceClock::default());
    device_clock.set_now(Duration::from_secs(0));

    let stream_clock = AudioStreamClock::new(device_clock.clone(), Duration::from_secs(0));

    device_clock.set_now(Duration::from_secs(5));
    assert_eq!(stream_clock.now(), Duration::from_secs(5));

    // Device clock jumps backwards: stream clock must not.
    device_clock.set_now(Duration::from_secs(4));
    assert_eq!(stream_clock.now(), Duration::from_secs(5));

    device_clock.set_now(Duration::from_secs(6));
    assert_eq!(stream_clock.now(), Duration::from_secs(6));
  }

  #[test]
  fn seek_resets_start_times() {
    let device_clock = Arc::new(FakeDeviceClock::default());
    device_clock.set_now(Duration::from_secs(0));

    let stream_clock = AudioStreamClock::new(device_clock.clone(), Duration::from_secs(0));

    device_clock.set_now(Duration::from_secs(5));
    assert_eq!(stream_clock.now(), Duration::from_secs(5));

    // Seek back to 1s.
    stream_clock.seek(Duration::from_secs(1));
    assert_eq!(stream_clock.now(), Duration::from_secs(1));

    // 1s of device time advances 1s of media time at rate 1.
    device_clock.set_now(Duration::from_secs(6));
    assert_eq!(stream_clock.now(), Duration::from_secs(2));
  }

  #[test]
  fn changing_rate_is_continuous_and_does_not_drift() {
    let device_clock = Arc::new(FakeDeviceClock::default());
    device_clock.set_now(Duration::from_secs(0));

    let stream_clock = AudioStreamClock::new(device_clock.clone(), Duration::from_secs(0));

    device_clock.set_now(Duration::from_secs(10));
    assert_eq!(stream_clock.now(), Duration::from_secs(10));

    // Slow down to half speed; current time should remain continuous.
    stream_clock.set_rate(0.5);
    assert_eq!(stream_clock.now(), Duration::from_secs(10));

    device_clock.set_now(Duration::from_secs(12));
    assert_eq!(stream_clock.now(), Duration::from_secs(11));

    // Drift test: rate=1.25 (5/4) should map 1ms -> 1.25ms exactly.
    let drift_device_clock = Arc::new(FakeDeviceClock::default());
    drift_device_clock.set_now(Duration::from_secs(0));
    let drift_clock = AudioStreamClock::new(drift_device_clock.clone(), Duration::from_secs(0));
    drift_clock.set_rate(1.25);

    for i in 0..10_000_u64 {
      drift_device_clock.set_now(Duration::from_millis(i));
      let expected_nanos = i.saturating_mul(1_250_000);
      assert_eq!(drift_clock.now(), Duration::from_nanos(expected_nanos));
    }
  }

  // --- PlaybackClock tests ---

  impl MediaClock for VirtualClock {
    fn now(&self) -> Duration {
      VirtualClock::now(self)
    }
  }

  fn ms(ms: u64) -> Duration {
    Duration::from_millis(ms)
  }

  #[test]
  fn play_pause_resume() {
    let base = Arc::new(VirtualClock::new());
    let clock = PlaybackClock::new(base.clone(), Duration::ZERO);

    assert_eq!(clock.now(), Duration::ZERO);
    assert_eq!(clock.state(), PlaybackState::Playing);

    base.advance(ms(500));
    assert_eq!(clock.now(), ms(500));

    clock.pause();
    assert_eq!(clock.state(), PlaybackState::Paused);
    assert_eq!(clock.now(), ms(500));

    base.advance(ms(500));
    assert_eq!(clock.now(), ms(500));

    clock.play();
    assert_eq!(clock.state(), PlaybackState::Playing);
    assert_eq!(clock.now(), ms(500));
    base.advance(ms(500));
    assert_eq!(clock.now(), ms(1000));
  }

  #[test]
  fn seek_sets_position_immediately() {
    let base = Arc::new(VirtualClock::new());
    let clock = PlaybackClock::new(base.clone(), Duration::ZERO);

    clock.seek(Duration::from_secs(10));
    assert_eq!(clock.now(), Duration::from_secs(10));

    base.advance(Duration::from_secs(2));
    assert_eq!(clock.now(), Duration::from_secs(12));

    clock.seek(Duration::from_secs(20));
    assert_eq!(clock.now(), Duration::from_secs(20));
    base.advance(Duration::from_secs(1));
    assert_eq!(clock.now(), Duration::from_secs(21));
  }

  #[test]
  fn playback_rate_scales_time() {
    let base = Arc::new(VirtualClock::new());
    let clock = PlaybackClock::new(base.clone(), Duration::ZERO);

    clock.set_rate(2.0);
    base.advance(Duration::from_secs(1));
    assert_eq!(clock.now(), Duration::from_secs(2));

    // Change rate while playing should not jump.
    clock.set_rate(0.5);
    assert_eq!(clock.now(), Duration::from_secs(2));
    base.advance(Duration::from_secs(2));
    assert_eq!(clock.now(), Duration::from_secs(3));
  }

  #[test]
  fn no_drift_over_many_small_steps() {
    let base = Arc::new(VirtualClock::new());
    let clock = PlaybackClock::new(base.clone(), Duration::ZERO);

    let step = ms(16);
    for _ in 0..10_000 {
      base.advance(step);
      let _ = clock.now();
    }

    assert_eq!(clock.now(), Duration::from_secs(160));
  }

  #[test]
  fn playback_clock_pause_seek_rate() {
    let master = Arc::new(VirtualClock::new());
    master.set_now(Duration::from_secs(0));
    let clock = PlaybackClock::new(master.clone(), Duration::from_secs(0));

    master.advance(Duration::from_secs(1));
    assert_eq!(clock.now(), Duration::from_secs(1));

    clock.pause();
    master.advance(Duration::from_secs(5));
    assert_eq!(clock.now(), Duration::from_secs(1));

    clock.play();
    master.advance(Duration::from_millis(500));
    assert_eq!(clock.now(), Duration::from_millis(1500));

    clock.seek(Duration::from_secs(10));
    assert_eq!(clock.now(), Duration::from_secs(10));
    master.advance(Duration::from_secs(1));
    assert_eq!(clock.now(), Duration::from_secs(11));

    clock.set_rate(2.0);
    assert_eq!(clock.now(), Duration::from_secs(11));
    master.advance(Duration::from_millis(100));
    assert_eq!(clock.now(), Duration::from_millis(11_200));
  }
}
