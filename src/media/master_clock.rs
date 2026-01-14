use crate::media::clock::MediaClock;
use parking_lot::Mutex as ParkingMutex;
use std::sync::Arc;
use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClockSource {
  Audio,
  System,
}

struct MasterClockInner {
  audio: Option<Arc<dyn MediaClock>>,
  current_source: ClockSource,
  // Offsets applied to the underlying clock timestamps to produce the master time.
  system_offset_nanos: i128,
  audio_offset_nanos: i128,
  // Last value returned by `MasterClock::now`, used to clamp monotonicity across clock glitches.
  last_master_nanos: i128,
}

/// A master clock that seamlessly switches between audio and system clock domains.
///
/// Video frame scheduling needs a single source of truth ("master clock"). When audio is present
/// and started it should be used as the master (for long-term A/V sync), but during startup and
/// buffering audio may not have started yet. This clock starts on the system clock, then switches
/// to audio once it becomes available and started.
///
/// When the clock source changes (audio becomes ready/unready, device restarts, tracks appear/
/// disappear) the returned media time remains continuous (no jumps), and monotonicity is enforced
/// via clamping.
pub struct MasterClock {
  system: Arc<dyn MediaClock>,
  inner: ParkingMutex<MasterClockInner>,
}

impl MasterClock {
  pub fn new(system: Arc<dyn MediaClock>) -> Self {
    Self {
      system,
      inner: ParkingMutex::new(MasterClockInner {
        audio: None,
        current_source: ClockSource::System,
        system_offset_nanos: 0,
        audio_offset_nanos: 0,
        last_master_nanos: 0,
      }),
    }
  }

  /// Returns the current master media time (monotonic, continuous across source changes).
  pub fn now(&self) -> Duration {
    let system_now_nanos = duration_to_nanos_i128(self.system.now());

    let mut inner = self.inner.lock();

    let desired_source = match &inner.audio {
      Some(audio) if audio.is_started() => ClockSource::Audio,
      _ => ClockSource::System,
    };

    // Switch sources if needed, computing a new offset so the master time is continuous.
    if desired_source != inner.current_source {
      let base_master_nanos = inner
        .master_nanos_for_current_source(system_now_nanos)
        .max(inner.last_master_nanos);

      match desired_source {
        ClockSource::Audio => {
          // Audio is present and started by construction of `desired_source`.
          let audio_now_nanos = duration_to_nanos_i128(inner.audio.as_ref().unwrap().now()); // fastrender-allow-unwrap
          inner.audio_offset_nanos = base_master_nanos.saturating_sub(audio_now_nanos);
        }
        ClockSource::System => {
          inner.system_offset_nanos = base_master_nanos.saturating_sub(system_now_nanos);
        }
      }

      inner.current_source = desired_source;
    }

    let candidate_nanos = inner.master_nanos_for_current_source(system_now_nanos);
    let clamped_nanos = candidate_nanos.max(inner.last_master_nanos);
    inner.last_master_nanos = clamped_nanos;

    nanos_i128_to_duration(clamped_nanos)
  }

  /// Sets (or clears) the audio clock used as the preferred master clock.
  ///
  /// If an audio clock is set and is started, the master clock will switch to it immediately. If
  /// the audio clock is not started, the master clock stays on the system clock until audio
  /// starts.
  pub fn set_audio_clock(&self, clock: Option<Arc<dyn MediaClock>>) {
    let system_now_nanos = duration_to_nanos_i128(self.system.now());

    let mut inner = self.inner.lock();

    // Sample the current master time before mutating `inner.audio`, so we can keep continuity even
    // when replacing/removing an audio clock while it is the current source.
    let base_master_nanos = inner
      .master_nanos_for_current_source(system_now_nanos)
      .max(inner.last_master_nanos);

    inner.audio = clock;

    let desired_source = match &inner.audio {
      Some(audio) if audio.is_started() => ClockSource::Audio,
      _ => ClockSource::System,
    };

    match desired_source {
      ClockSource::Audio => {
        let audio_now_nanos = duration_to_nanos_i128(inner.audio.as_ref().unwrap().now()); // fastrender-allow-unwrap
        inner.audio_offset_nanos = base_master_nanos.saturating_sub(audio_now_nanos);
      }
      ClockSource::System => {
        inner.system_offset_nanos = base_master_nanos.saturating_sub(system_now_nanos);
      }
    }

    inner.current_source = desired_source;
    // Do not advance `last_master_nanos` here; it should only reflect values returned by `now()`.
  }

  pub fn current_source(&self) -> ClockSource {
    self.inner.lock().current_source
  }
}

impl MediaClock for MasterClock {
  fn now(&self) -> Duration {
    MasterClock::now(self)
  }

  fn is_started(&self) -> bool {
    // A MasterClock always has a system clock fallback, so it can produce a usable timeline
    // immediately even if the audio clock is not started yet.
    true
  }
}

impl MasterClockInner {
  fn master_nanos_for_current_source(&self, system_now_nanos: i128) -> i128 {
    match self.current_source {
      ClockSource::System => system_now_nanos.saturating_add(self.system_offset_nanos),
      ClockSource::Audio => self
        .audio
        .as_ref()
        .map(|audio| {
          let audio_now_nanos = duration_to_nanos_i128(audio.now());
          audio_now_nanos.saturating_add(self.audio_offset_nanos)
        })
        // If the audio clock disappears while we are still on it, fall back to the last value we
        // returned. The next `now()` call will switch to the system clock and compute a new
        // offset.
        .unwrap_or(self.last_master_nanos),
    }
  }
}

fn duration_to_nanos_i128(duration: Duration) -> i128 {
  let nanos = duration.as_nanos();
  if nanos > i128::MAX as u128 {
    i128::MAX
  } else {
    nanos as i128
  }
}

fn nanos_i128_to_duration(nanos: i128) -> Duration {
  if nanos <= 0 {
    return Duration::ZERO;
  }

  let nanos = nanos as u128;
  let secs = nanos / 1_000_000_000;
  let subsec_nanos = (nanos % 1_000_000_000) as u32;
  if secs > u64::MAX as u128 {
    Duration::MAX
  } else {
    Duration::new(secs as u64, subsec_nanos)
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use std::sync::atomic::AtomicBool;
  use std::sync::atomic::AtomicU64;
  use std::sync::atomic::Ordering;
  use std::time::Instant;

  #[derive(Debug, Default)]
  struct TestClock {
    now_nanos: AtomicU64,
    started: AtomicBool,
  }

  impl TestClock {
    fn new(now: Duration, started: bool) -> Self {
      Self {
        now_nanos: AtomicU64::new(duration_to_nanos_u64(now)),
        started: AtomicBool::new(started),
      }
    }

    fn set_now(&self, now: Duration) {
      self.now_nanos
        .store(duration_to_nanos_u64(now), Ordering::Relaxed);
    }

    fn advance(&self, delta: Duration) {
      let delta = duration_to_nanos_u64(delta);
      let _ = self
        .now_nanos
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
          Some(current.saturating_add(delta))
        });
    }

    fn set_started(&self, started: bool) {
      self.started.store(started, Ordering::Relaxed);
    }
  }

  impl MediaClock for TestClock {
    fn now(&self) -> Duration {
      Duration::from_nanos(self.now_nanos.load(Ordering::Relaxed))
    }

    fn is_started(&self) -> bool {
      self.started.load(Ordering::Relaxed)
    }
  }

  fn duration_to_nanos_u64(duration: Duration) -> u64 {
    u64::try_from(duration.as_nanos()).unwrap_or(u64::MAX)
  }

  #[test]
  fn system_to_audio_switch_is_continuous() {
    let system = Arc::new(TestClock::new(Duration::ZERO, true));
    let master = MasterClock::new(system.clone());

    system.advance(Duration::from_secs(5));
    let before = master.now();
    assert_eq!(before, Duration::from_secs(5));
    assert_eq!(master.current_source(), ClockSource::System);

    // Audio starts with a completely different origin.
    let audio = Arc::new(TestClock::new(Duration::from_secs(100), true));
    master.set_audio_clock(Some(audio.clone()));

    let after = master.now();
    assert_eq!(after, before);
    assert_eq!(master.current_source(), ClockSource::Audio);

    // Advance both clocks like real time would.
    audio.advance(Duration::from_secs(3));
    system.advance(Duration::from_secs(3));
    assert_eq!(master.now(), Duration::from_secs(8));
  }

  #[test]
  fn audio_to_system_switch_is_continuous() {
    let system = Arc::new(TestClock::new(Duration::ZERO, true));
    let master = MasterClock::new(system.clone());

    system.advance(Duration::from_secs(5));
    assert_eq!(master.now(), Duration::from_secs(5));

    let audio = Arc::new(TestClock::new(Duration::from_secs(100), true));
    master.set_audio_clock(Some(audio.clone()));

    audio.advance(Duration::from_secs(2));
    system.advance(Duration::from_secs(2));
    let before_detach = master.now();
    assert_eq!(before_detach, Duration::from_secs(7));
    assert_eq!(master.current_source(), ClockSource::Audio);

    master.set_audio_clock(None);
    assert_eq!(master.current_source(), ClockSource::System);

    let after_detach = master.now();
    assert_eq!(after_detach, before_detach);

    system.advance(Duration::from_secs(3));
    assert_eq!(master.now(), Duration::from_secs(10));
  }

  #[test]
  fn switching_to_a_behind_clock_never_regresses() {
    let system = Arc::new(TestClock::new(Duration::from_secs(10), true));
    let master = MasterClock::new(system.clone());

    let first = master.now();
    assert_eq!(first, Duration::from_secs(10));

    // Simulate the system clock briefly going backwards (or reporting a smaller timestamp due to
    // measurement jitter).
    system.set_now(Duration::from_secs(9));

    // Attach an audio clock that is far behind the last reported master time.
    let audio = Arc::new(TestClock::new(Duration::ZERO, true));
    master.set_audio_clock(Some(audio.clone()));

    let after_switch = master.now();
    assert_eq!(after_switch, first);

    audio.advance(Duration::from_secs(1));
    assert_eq!(master.now(), Duration::from_secs(11));
  }

  #[test]
  fn audio_is_ignored_until_started() {
    let system = Arc::new(TestClock::new(Duration::ZERO, true));
    let master = MasterClock::new(system.clone());

    system.advance(Duration::from_secs(2));
    assert_eq!(master.now(), Duration::from_secs(2));

    let audio = Arc::new(TestClock::new(Duration::from_secs(100), false));
    master.set_audio_clock(Some(audio.clone()));
    assert_eq!(master.current_source(), ClockSource::System);

    system.advance(Duration::from_secs(1));
    assert_eq!(master.now(), Duration::from_secs(3));

    audio.set_started(true);
    // Audio time hasn't advanced yet; switching should preserve continuity.
    assert_eq!(master.now(), Duration::from_secs(3));
    assert_eq!(master.current_source(), ClockSource::Audio);
  }

  #[test]
  fn output_audio_clock_is_ignored_until_started() {
    use crate::media::audio::AudioClock as BackendAudioClock;
    use crate::media::audio_clock::InterpolatedAudioClock;

    let system = Arc::new(TestClock::new(Duration::ZERO, true));
    let master = MasterClock::new(system.clone());

    system.advance(Duration::from_secs(2));
    assert_eq!(master.now(), Duration::from_secs(2));
    assert_eq!(master.current_source(), ClockSource::System);

    // Attach an output-frame based clock; it should not be considered started until the first audio
    // callback is observed.
    let device_clock = Arc::new(InterpolatedAudioClock::new(1000));
    let audio = Arc::new(BackendAudioClock::OutputFrames {
      clock: device_clock.clone(),
    });
    master.set_audio_clock(Some(audio));
    assert_eq!(master.current_source(), ClockSource::System);

    system.advance(Duration::from_secs(1));
    assert_eq!(master.now(), Duration::from_secs(3));
    assert_eq!(master.current_source(), ClockSource::System);

    // Simulate the first backend callback. Use 0 frames so the audio clock time stays stable and
    // the switch can be asserted deterministically.
    device_clock.on_callback_end_at(Instant::now(), 0, None);

    assert_eq!(master.now(), Duration::from_secs(3));
    assert_eq!(master.current_source(), ClockSource::Audio);
  }
}
