use parking_lot::Mutex;
use std::path::PathBuf;
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
  fn start_stream(&mut self) -> Result<(), String>;
  fn stop_stream(&mut self);
}

impl<T: AudioBackend + ?Sized> AudioBackend for Box<T> {
  fn start_stream(&mut self) -> Result<(), String> {
    (**self).start_stream()
  }

  fn stop_stream(&mut self) {
    (**self).stop_stream();
  }
}

struct AudioEngineInner<B: AudioBackend> {
  backend: B,
  idle_timeout: Duration,
  backend_running: bool,
  active_streams: usize,
  last_non_silent: Option<Instant>,
}

/// A debounced audio output controller that suspends the backend stream when idle.
///
/// The engine is intentionally lightweight: it only tracks whether any audio streams are
/// currently producing non-silent samples, and stops the output backend after a short idle
/// period to avoid keeping the OS audio device open unnecessarily.
#[derive(Clone)]
pub struct AudioEngine<B: AudioBackend> {
  inner: Arc<Mutex<AudioEngineInner<B>>>,
}

impl<B: AudioBackend> AudioEngine<B> {
  pub fn new(backend: B) -> Self {
    Self::with_idle_timeout(backend, DEFAULT_IDLE_TIMEOUT)
  }

  pub fn with_idle_timeout(backend: B, idle_timeout: Duration) -> Self {
    Self {
      inner: Arc::new(Mutex::new(AudioEngineInner {
        backend,
        idle_timeout,
        backend_running: false,
        active_streams: 0,
        last_non_silent: None,
      })),
    }
  }

  /// Create a new logical audio stream handle.
  ///
  /// The returned handle is initially inactive; call `set_active(true)` when it begins producing
  /// non-silent output (e.g. when a media element starts playing).
  pub fn register_stream(&self) -> AudioStreamHandle<B> {
    AudioStreamHandle {
      inner: Arc::downgrade(&self.inner),
      active: false,
    }
  }

  pub fn tick(&self) {
    self.tick_at(Instant::now());
  }

  fn tick_at(&self, now: Instant) {
    let mut inner = self.inner.lock();
    if inner.active_streams > 0 {
      // Treat "has active streams" as non-silent activity for idle tracking purposes.
      inner.last_non_silent = Some(now);
      return;
    }

    if !inner.backend_running {
      return;
    }

    let Some(last) = inner.last_non_silent else {
      // Shouldn't happen (we set the timestamp on activity transitions), but stay conservative.
      inner.last_non_silent = Some(now);
      return;
    };

    let idle_for = now.checked_duration_since(last).unwrap_or(Duration::ZERO);
    if idle_for >= inner.idle_timeout {
      inner.backend.stop_stream();
      inner.backend_running = false;
    }
  }

  pub fn telemetry(&self) -> AudioEngineTelemetry {
    self.telemetry_at(Instant::now())
  }

  fn telemetry_at(&self, now: Instant) -> AudioEngineTelemetry {
    let inner = self.inner.lock();
    let output_state = if inner.backend_running {
      OutputStreamState::Running
    } else {
      OutputStreamState::Suspended
    };
    let idle_for = if inner.active_streams == 0 {
      inner
        .last_non_silent
        .and_then(|last| now.checked_duration_since(last))
    } else {
      None
    };
    AudioEngineTelemetry {
      output_state,
      active_streams: inner.active_streams,
      idle_for,
    }
  }
}

pub struct AudioStreamHandle<B: AudioBackend> {
  inner: Weak<Mutex<AudioEngineInner<B>>>,
  active: bool,
}

impl<B: AudioBackend> AudioStreamHandle<B> {
  pub fn set_active(&mut self, active: bool) -> Result<(), String> {
    self.set_active_at(active, Instant::now())
  }

  fn set_active_at(&mut self, active: bool, now: Instant) -> Result<(), String> {
    if self.active == active {
      return Ok(());
    }

    let Some(inner) = self.inner.upgrade() else {
      // Engine dropped; nothing to do.
      self.active = active;
      return Ok(());
    };
    let mut inner = inner.lock();

    if active {
      if inner.active_streams == 0 && !inner.backend_running {
        inner.backend.start_stream()?;
        inner.backend_running = true;
      }
      inner.active_streams = inner.active_streams.saturating_add(1);
      inner.last_non_silent = Some(now);
      self.active = true;
      return Ok(());
    }

    if inner.active_streams > 0 {
      inner.active_streams -= 1;
    }
    if inner.active_streams == 0 {
      // Start the idle timer from when we became silent (pause/end/drain).
      inner.last_non_silent = Some(now);
    }
    self.active = false;
    Ok(())
  }
}

impl<B: AudioBackend> Drop for AudioStreamHandle<B> {
  fn drop(&mut self) {
    if !self.active {
      return;
    }

    // Best-effort: dropping a handle should never panic.
    let _ = self.set_active_at(false, Instant::now());
  }
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
  fn start_stream(&mut self) -> Result<(), String> {
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
  fn start_stream(&mut self) -> Result<(), String> {
    if self.file.is_some() {
      return Ok(());
    }
    let file = std::fs::File::create(&self.path).map_err(|err| err.to_string())?;
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
    fn start_stream(&mut self) -> Result<(), String> {
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
    let mut stream = engine.register_stream();
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
    let mut stream2 = engine.register_stream();
    stream2.set_active_at(true, t2).unwrap();
    assert_eq!(probe.started(), 2);
    assert!(probe.is_running());
  }
}
