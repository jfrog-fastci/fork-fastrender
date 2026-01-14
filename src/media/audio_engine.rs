use parking_lot::Mutex;
use crate::media::audio::AudioError;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

/// Default debounce before suspending the audio output stream after the last active audio stream
/// becomes silent.
pub const DEFAULT_IDLE_TIMEOUT: Duration = Duration::from_millis(1500);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputStreamState {
  Running,
  Suspended,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioEngineTelemetry {
  pub output_state: OutputStreamState,
  pub active_streams: usize,
  pub idle_for: Option<Duration>,
}

/// Backend abstraction for platform-specific audio output.
///
/// For CPAL this typically corresponds to starting/stopping (dropping) the output stream.
pub trait AudioBackend: Send + 'static {
  fn start_stream(&mut self) -> Result<(), AudioError>;
  fn stop_stream(&mut self);
}

impl<T: AudioBackend + ?Sized> AudioBackend for Box<T> {
  fn start_stream(&mut self) -> Result<(), AudioError> {
    (**self).start_stream()
  }

  fn stop_stream(&mut self) {
    (**self).stop_stream();
  }
}

struct AudioEngineInner<B: AudioBackend> {
  backend: Mutex<B>,
  idle_timeout: Duration,
  backend_running: AtomicBool,
  active_streams: AtomicUsize,
  start: Instant,
  /// Monotonic timestamp (nanoseconds since `start`) for the last observed non-silent activity.
  last_non_silent_nanos: AtomicU64,
}

/// A debounced audio output controller that suspends the backend stream when idle.
///
/// The engine is intentionally lightweight: it only tracks whether any audio streams are
/// currently producing non-silent samples, and stops the output backend after a short idle
/// period to avoid keeping the OS audio device open unnecessarily.
#[derive(Clone)]
pub struct AudioEngine<B: AudioBackend> {
  inner: Arc<AudioEngineInner<B>>,
}

impl<B: AudioBackend> AudioEngine<B> {
  pub fn new(backend: B) -> Self {
    Self::with_idle_timeout(backend, DEFAULT_IDLE_TIMEOUT)
  }

  pub fn with_idle_timeout(backend: B, idle_timeout: Duration) -> Self {
    let start = Instant::now();
    Self {
      inner: Arc::new(AudioEngineInner {
        backend: Mutex::new(backend),
        idle_timeout,
        backend_running: AtomicBool::new(false),
        active_streams: AtomicUsize::new(0),
        start,
        last_non_silent_nanos: AtomicU64::new(0),
      }),
    }
  }

  /// Create a new logical audio stream handle.
  ///
  /// The returned handle is initially inactive; call `set_active(true)` when it begins producing
  /// non-silent output (e.g. when a media element starts playing).
  pub fn register_stream(&self) -> AudioStreamHandle<B> {
    AudioStreamHandle {
      inner: Arc::downgrade(&self.inner),
      active: AtomicBool::new(false),
    }
  }

  pub fn tick(&self) {
    self.tick_at(Instant::now());
  }

  fn tick_at(&self, now: Instant) {
    tick_inner(&self.inner, now);
  }

  /// Spawn a background watchdog that periodically calls [`AudioEngine::tick`].
  ///
  /// This is useful for integrations that don't have a central "main loop" tick driving audio
  /// housekeeping. The thread exits automatically once the [`AudioEngine`] is dropped.
  pub fn spawn_idle_watcher(&self) {
    let interval = (self.inner.idle_timeout / 4)
      .max(Duration::from_millis(50))
      .min(Duration::from_millis(250));

    let weak = Arc::downgrade(&self.inner);
    std::thread::spawn(move || loop {
      std::thread::sleep(interval);
      let Some(inner) = weak.upgrade() else {
        break;
      };
      tick_inner(&inner, Instant::now());
    });
  }

  pub fn telemetry(&self) -> AudioEngineTelemetry {
    self.telemetry_at(Instant::now())
  }

  fn telemetry_at(&self, now: Instant) -> AudioEngineTelemetry {
    let output_state = if self.inner.backend_running.load(Ordering::Relaxed) {
      OutputStreamState::Running
    } else {
      OutputStreamState::Suspended
    };
    let active_streams = self.inner.active_streams.load(Ordering::Relaxed);
    let idle_for = if active_streams == 0 {
      let now_nanos = instant_to_nanos_u64(self.inner.start, now);
      let last = self.inner.last_non_silent_nanos.load(Ordering::Relaxed);
      Some(Duration::from_nanos(now_nanos.saturating_sub(last)))
    } else {
      None
    };
    AudioEngineTelemetry {
      output_state,
      active_streams,
      idle_for,
    }
  }
}

pub struct AudioStreamHandle<B: AudioBackend> {
  inner: Weak<AudioEngineInner<B>>,
  active: AtomicBool,
}

impl<B: AudioBackend> AudioStreamHandle<B> {
  pub fn set_active(&self, active: bool) -> Result<(), AudioError> {
    self.set_active_at(active, Instant::now())
  }

  fn set_active_at(&self, active: bool, now: Instant) -> Result<(), AudioError> {
    if active {
      self.set_active_true(now)
    } else {
      self.set_active_false(now);
      Ok(())
    }
  }

  fn set_active_true(&self, now: Instant) -> Result<(), AudioError> {
    let Some(inner) = self.inner.upgrade() else {
      self.active.store(true, Ordering::Relaxed);
      return Ok(());
    };

    let changed = self
      .active
      .compare_exchange(false, true, Ordering::AcqRel, Ordering::Relaxed)
      .is_ok();
    if changed {
      inner.active_streams.fetch_add(1, Ordering::Relaxed);
    }

    inner
      .last_non_silent_nanos
      .store(instant_to_nanos_u64(inner.start, now), Ordering::Relaxed);

    maybe_start_backend(&inner)
  }

  fn set_active_false(&self, now: Instant) {
    let Some(inner) = self.inner.upgrade() else {
      self.active.store(false, Ordering::Relaxed);
      return;
    };

    if self
      .active
      .compare_exchange(true, false, Ordering::AcqRel, Ordering::Relaxed)
      .is_err()
    {
      return;
    }

    let now_nanos = instant_to_nanos_u64(inner.start, now);

    let prev = inner
      .active_streams
      .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |value| {
        value.checked_sub(1)
      })
      .unwrap_or(0);

    if prev <= 1 {
      // This was the last active stream; start the idle timer.
      inner.last_non_silent_nanos.store(now_nanos, Ordering::Relaxed);
    }
  }
}

impl<B: AudioBackend> Drop for AudioStreamHandle<B> {
  fn drop(&mut self) {
    // Best-effort: dropping a handle should never panic.
    let _ = self.set_active(false);
  }
}

fn maybe_start_backend<B: AudioBackend>(inner: &AudioEngineInner<B>) -> Result<(), AudioError> {
  if inner.backend_running.load(Ordering::Relaxed) {
    return Ok(());
  }
  if inner.active_streams.load(Ordering::Relaxed) == 0 {
    return Ok(());
  }

  // Attempt to claim stream start so only one caller performs backend initialization.
  if inner
    .backend_running
    .compare_exchange(false, true, Ordering::AcqRel, Ordering::Relaxed)
    .is_err()
  {
    return Ok(());
  }

  // Avoid starting the backend if we've already become idle again.
  if inner.active_streams.load(Ordering::Relaxed) == 0 {
    inner.backend_running.store(false, Ordering::Relaxed);
    return Ok(());
  }

  let mut backend = inner.backend.lock();
  match backend.start_stream() {
    Ok(()) => Ok(()),
    Err(err) => {
      inner.backend_running.store(false, Ordering::Relaxed);
      Err(err)
    }
  }
}

fn tick_inner<B: AudioBackend>(inner: &AudioEngineInner<B>, now: Instant) {
  if inner.active_streams.load(Ordering::Relaxed) > 0 {
    inner
      .last_non_silent_nanos
      .store(instant_to_nanos_u64(inner.start, now), Ordering::Relaxed);
    // Ensure the backend is running while streams are active. This also provides a retry path if
    // `start_stream` previously failed and there are no further `set_active(true)` calls (for
    // example, when audio has already been buffered).
    let _ = maybe_start_backend(inner);
    return;
  }

  if !inner.backend_running.load(Ordering::Relaxed) {
    return;
  }

  let now_nanos = instant_to_nanos_u64(inner.start, now);
  let last = inner.last_non_silent_nanos.load(Ordering::Relaxed);
  let idle_for = Duration::from_nanos(now_nanos.saturating_sub(last));
  if idle_for < inner.idle_timeout {
    return;
  }

  // Stop is non-real-time; double-check state after taking the lock to avoid races with
  // concurrent `set_active(true)`.
  let mut backend = inner.backend.lock();
  if inner.active_streams.load(Ordering::Relaxed) == 0 {
    backend.stop_stream();
    inner.backend_running.store(false, Ordering::Relaxed);
  }
}

fn duration_to_nanos_u64(duration: Duration) -> u64 {
  u64::try_from(duration.as_nanos()).unwrap_or(u64::MAX)
}

fn instant_to_nanos_u64(start: Instant, now: Instant) -> u64 {
  duration_to_nanos_u64(now.checked_duration_since(start).unwrap_or(Duration::ZERO))
}

/// A backend that does nothing; useful for headless runs and unit tests.
#[derive(Debug, Default)]
pub struct NullAudioBackend {
  running: bool,
}

impl NullAudioBackend {
  pub fn new() -> Self {
    Self::default()
  }

  pub fn is_running(&self) -> bool {
    self.running
  }
}

impl AudioBackend for NullAudioBackend {
  fn start_stream(&mut self) -> Result<(), AudioError> {
    self.running = true;
    Ok(())
  }

  fn stop_stream(&mut self) {
    self.running = false;
  }
}

/// A backend that "plays" audio by opening a WAV file while active.
///
/// This is intended for deterministic testing / offline debugging where "audio output" should not
/// touch the OS audio device. The current implementation only manages the file resource lifetime;
/// writing actual samples is owned by the audio mixer layer.
#[derive(Debug)]
pub struct WavAudioBackend {
  path: PathBuf,
  file: Option<std::fs::File>,
}

impl WavAudioBackend {
  pub fn new(path: impl Into<PathBuf>) -> Self {
    Self {
      path: path.into(),
      file: None,
    }
  }

  pub fn is_open(&self) -> bool {
    self.file.is_some()
  }
}

impl AudioBackend for WavAudioBackend {
  fn start_stream(&mut self) -> Result<(), AudioError> {
    if self.file.is_some() {
      return Ok(());
    }
    let file = std::fs::File::create(&self.path).map_err(|err| {
      AudioError::invalid_spec(format!(
        "wav backend failed to create file '{}': {err}",
        self.path.display()
      ))
    })?;
    self.file = Some(file);
    Ok(())
  }

  fn stop_stream(&mut self) {
    self.file.take();
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

  #[derive(Clone, Default)]
  struct FakeBackend {
    started: Arc<AtomicUsize>,
    stopped: Arc<AtomicUsize>,
    running: Arc<AtomicBool>,
  }

  impl FakeBackend {
    fn started(&self) -> usize {
      self.started.load(Ordering::SeqCst)
    }

    fn stopped(&self) -> usize {
      self.stopped.load(Ordering::SeqCst)
    }

    fn is_running(&self) -> bool {
      self.running.load(Ordering::SeqCst)
    }
  }

  impl AudioBackend for FakeBackend {
    fn start_stream(&mut self) -> Result<(), AudioError> {
      self.started.fetch_add(1, Ordering::SeqCst);
      self.running.store(true, Ordering::SeqCst);
      Ok(())
    }

    fn stop_stream(&mut self) {
      self.stopped.fetch_add(1, Ordering::SeqCst);
      self.running.store(false, Ordering::SeqCst);
    }
  }

  #[test]
  fn idle_suspend_resume_debounced() {
    let idle_timeout = Duration::from_millis(10);
    let backend = FakeBackend::default();
    let probe = backend.clone();
    let engine = AudioEngine::with_idle_timeout(backend, idle_timeout);

    let t0 = Instant::now();
    let stream = engine.register_stream();
    stream.set_active_at(true, t0).unwrap();
    assert_eq!(probe.started(), 1);
    assert!(probe.is_running());

    let telemetry = engine.telemetry_at(t0);
    assert_eq!(telemetry.output_state, OutputStreamState::Running);
    assert_eq!(telemetry.active_streams, 1);

    let t1 = t0 + Duration::from_millis(1);
    stream.set_active_at(false, t1).unwrap();
    assert_eq!(engine.telemetry_at(t1).active_streams, 0);

    // Not idle long enough yet.
    engine.tick_at(t1 + Duration::from_millis(5));
    assert_eq!(probe.stopped(), 0);
    assert!(probe.is_running());

    // Past idle timeout -> backend stream should be suspended.
    engine.tick_at(t1 + idle_timeout + Duration::from_millis(1));
    assert_eq!(probe.stopped(), 1);
    assert!(!probe.is_running());
    assert_eq!(
      engine.telemetry_at(t1 + idle_timeout + Duration::from_millis(1)).output_state,
      OutputStreamState::Suspended
    );

    // New activity should restart output immediately.
    let t2 = t1 + idle_timeout + Duration::from_millis(2);
    let stream2 = engine.register_stream();
    stream2.set_active_at(true, t2).unwrap();
    assert_eq!(probe.started(), 2);
    assert!(probe.is_running());
  }
}
