use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock, Weak};

use parking_lot::Mutex;

use super::{
  audio_engine_config, AudioBackend, AudioEngineConfig, AudioOutputInfo, AudioSink, AudioStreamConfig,
};
use crate::debug::trace::TraceHandle;
use crate::media::clock::MediaClock;

/// Identifier for a logical group of sinks (e.g. a browser tab).
///
/// Groups have their own volume and mute state that are applied on top of the per-sink volume and
/// the engine master volume.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct AudioGroupId(u64);

/// Group id used for sinks that aren't explicitly assigned to a group.
const DEFAULT_GROUP: AudioGroupId = AudioGroupId(0);

#[derive(Debug)]
struct AudioGroupState {
  volume_bits: AtomicU32,
  muted: AtomicBool,
}

impl AudioGroupState {
  fn new() -> Self {
    Self {
      volume_bits: AtomicU32::new(1.0f32.to_bits()),
      muted: AtomicBool::new(false),
    }
  }

  fn set_volume(&self, volume: f32) {
    let volume = sanitize_volume(volume);
    self.volume_bits.store(volume.to_bits(), Ordering::Relaxed);
  }

  fn volume(&self) -> f32 {
    f32::from_bits(self.volume_bits.load(Ordering::Relaxed))
  }

  fn set_muted(&self, muted: bool) {
    self.muted.store(muted, Ordering::Relaxed);
  }

  fn muted(&self) -> bool {
    self.muted.load(Ordering::Relaxed)
  }
}

#[derive(Debug)]
struct GroupingState {
  master_volume_bits: AtomicU32,
  master_muted: AtomicBool,
  next_group_id: AtomicU64,
  groups: Mutex<HashMap<AudioGroupId, Arc<AudioGroupState>>>,
  sinks: Mutex<Vec<Weak<AudioEngineSinkInner>>>,
}

impl GroupingState {
  fn new() -> Self {
    let mut groups = HashMap::new();
    groups.insert(DEFAULT_GROUP, Arc::new(AudioGroupState::new()));
    Self {
      master_volume_bits: AtomicU32::new(1.0f32.to_bits()),
      master_muted: AtomicBool::new(false),
      next_group_id: AtomicU64::new(1),
      groups: Mutex::new(groups),
      sinks: Mutex::new(Vec::new()),
    }
  }

  fn master_volume(&self) -> f32 {
    f32::from_bits(self.master_volume_bits.load(Ordering::Relaxed))
  }

  fn master_muted(&self) -> bool {
    self.master_muted.load(Ordering::Relaxed)
  }

  fn update_all_sink_volumes(&self) {
    let sinks: Vec<Arc<AudioEngineSinkInner>> = {
      let mut guard = self.sinks.lock();
      let mut strong = Vec::with_capacity(guard.len());
      guard.retain(|weak| {
        if let Some(inner) = weak.upgrade() {
          strong.push(inner);
          true
        } else {
          false
        }
      });
      strong
    };

    for sink in sinks {
      sink.apply_effective_volume();
    }
  }

  fn update_group_sink_volumes(&self, group: AudioGroupId) {
    let sinks: Vec<Arc<AudioEngineSinkInner>> = {
      let mut guard = self.sinks.lock();
      let mut strong = Vec::with_capacity(guard.len());
      guard.retain(|weak| {
        if let Some(inner) = weak.upgrade() {
          strong.push(inner);
          true
        } else {
          false
        }
      });
      strong
    };

    for sink in sinks {
      if sink.group == group {
        sink.apply_effective_volume();
      }
    }
  }
}

/// High-level audio engine that owns an output backend and its configuration.
///
/// This is the intended entry point for media playback code. It centralizes all tunables and
/// provides a consistent configuration surface across different backends.
///
/// Grouping semantics:
/// - Streams/sinks can be assigned to a logical [`AudioGroupId`] (e.g. browser tab).
/// - Each group has its own volume and mute state.
/// - A master volume/mute applies on top of every group.
/// - The final gain applied to each sink is: `final_gain = master * group * sink` (with mute
///   states forcing the gain to 0).
///
/// Important: muting must **not** behave like pausing. Even when `final_gain` is 0, the underlying
/// backend sinks must continue draining queued audio so device time and ended/backpressure
/// semantics remain correct.
pub struct AudioEngine {
  config: Arc<AudioEngineConfig>,
  backend: Arc<dyn AudioBackend>,
  device_clock: Arc<dyn MediaClock>,
  grouping: Arc<GroupingState>,
}

impl AudioEngine {
  /// Create an [`AudioEngine`] using a "best effort" backend selection policy.
  #[must_use]
  pub fn new_best_effort(config: Arc<AudioEngineConfig>) -> Self {
    let backend = <dyn AudioBackend>::new_best_effort_with_config(&config);
    Self::new_with_backend(config, Arc::from(backend))
  }

  /// Like [`Self::new_best_effort`], but wires up audio tracing spans into the provided handle.
  #[must_use]
  pub fn new_best_effort_with_trace(config: Arc<AudioEngineConfig>, trace: TraceHandle) -> Self {
    let backend = <dyn AudioBackend>::new_best_effort_with_config_and_trace(&config, trace);
    Self::new_with_backend(config, Arc::from(backend))
  }

  /// Construct an engine with an explicitly provided backend.
  ///
  /// This is primarily intended for deterministic unit tests.
  #[must_use]
  pub fn new_with_backend(config: Arc<AudioEngineConfig>, backend: Arc<dyn AudioBackend>) -> Self {
    let device_clock: Arc<dyn MediaClock> = Arc::new(backend.clock());
    Self {
      config,
      backend,
      device_clock,
      grouping: Arc::new(GroupingState::new()),
    }
  }

  /// Convenience constructor that uses the currently active configuration.
  ///
  /// By default this parses `FASTR_AUDIO_*` environment variables, but unit tests can install an
  /// override via [`super::set_audio_engine_config`].
  #[must_use]
  pub fn init_from_env() -> Self {
    Self::new_best_effort(audio_engine_config())
  }

  /// Like [`Self::init_from_env`], but uses the provided trace handle for backend tracing spans.
  #[must_use]
  pub fn init_from_env_with_trace(trace: TraceHandle) -> Self {
    Self::new_best_effort_with_trace(audio_engine_config(), trace)
  }

  /// Returns the process-global [`AudioEngine`] instance.
  ///
  /// This is initialized on first use using [`Self::init_from_env`].
  #[must_use]
  pub fn global() -> Arc<Self> {
    if let Some(engine) = ENGINE_OVERRIDE
      .get_or_init(|| Mutex::new(None))
      .lock()
      .clone()
    {
      return engine;
    }

    ENGINE
      .get_or_init(|| Arc::new(AudioEngine::init_from_env()))
      .clone()
  }

  /// Overrides [`Self::global`] for the lifetime of the returned guard.
  ///
  /// This is intended for unit tests that need deterministic backends/configs without mutating
  /// process environment variables.
  pub fn init_for_test(engine: AudioEngine) -> AudioEngineTestGuard {
    let lock = ENGINE_OVERRIDE.get_or_init(|| Mutex::new(None));
    let mut guard = lock.lock();
    let previous = guard.replace(Arc::new(engine));
    AudioEngineTestGuard { previous }
  }

  #[must_use]
  pub fn config(&self) -> &AudioEngineConfig {
    &self.config
  }

  #[must_use]
  pub fn backend(&self) -> &dyn AudioBackend {
    &*self.backend
  }

  #[must_use]
  pub fn output_config(&self) -> AudioStreamConfig {
    self.backend.output_config()
  }

  #[must_use]
  pub fn output_info(&self) -> AudioOutputInfo {
    self.backend.output_info()
  }

  #[must_use]
  pub fn device_clock(&self) -> Arc<dyn MediaClock> {
    self.device_clock.clone()
  }

  /// Create a new group (e.g. a browser tab).
  #[must_use]
  pub fn create_group(&self) -> AudioGroupId {
    let id = AudioGroupId(self.grouping.next_group_id.fetch_add(1, Ordering::Relaxed));
    self
      .grouping
      .groups
      .lock()
      .insert(id, Arc::new(AudioGroupState::new()));
    id
  }

  /// Create a new sink in the default group.
  ///
  /// This is intended for callers that do not care about grouping semantics.
  #[must_use]
  pub fn create_sink(&self) -> AudioSinkHandle {
    self.create_sink_in_group(DEFAULT_GROUP)
  }

  /// Create a new sink within an existing group.
  #[must_use]
  pub fn create_sink_in_group(&self, group: AudioGroupId) -> AudioSinkHandle {
    let group_state = {
      let groups = self.grouping.groups.lock();
      groups
        .get(&group)
        .cloned()
        .expect("AudioGroupId must be created by AudioEngine::create_group (or be the engine default)") // fastrender-allow-unwrap
    };

    let backend_sink = self.backend.create_sink();
    let inner = Arc::new(AudioEngineSinkInner {
      backend_sink,
      grouping: Arc::downgrade(&self.grouping),
      group,
      group_state,
      sink_volume_bits: AtomicU32::new(1.0f32.to_bits()),
    });

    // Apply initial volume before publishing so the sink starts at the correct gain.
    inner.apply_effective_volume();

    self.grouping.sinks.lock().push(Arc::downgrade(&inner));
    AudioEngineSink { inner }
  }

  pub fn set_master_volume(&self, volume: f32) {
    let volume = sanitize_volume(volume);
    self
      .grouping
      .master_volume_bits
      .store(volume.to_bits(), Ordering::Relaxed);
    self.grouping.update_all_sink_volumes();
  }

  pub fn set_master_muted(&self, muted: bool) {
    self.grouping.master_muted.store(muted, Ordering::Relaxed);
    self.grouping.update_all_sink_volumes();
  }

  pub fn set_group_volume(&self, group: AudioGroupId, volume: f32) {
    let group_state = {
      let groups = self.grouping.groups.lock();
      groups.get(&group).cloned()
    };

    if let Some(state) = group_state {
      state.set_volume(volume);
      self.grouping.update_group_sink_volumes(group);
    }
  }

  pub fn set_group_muted(&self, group: AudioGroupId, muted: bool) {
    let group_state = {
      let groups = self.grouping.groups.lock();
      groups.get(&group).cloned()
    };

    if let Some(state) = group_state {
      state.set_muted(muted);
      self.grouping.update_group_sink_volumes(group);
    }
  }
}

/// A sink created by [`AudioEngine`] that applies master + group gain on top of its own volume.
pub struct AudioEngineSink {
  inner: Arc<AudioEngineSinkInner>,
}

/// Per-element sink handle returned by [`AudioEngine::create_sink`].
pub type AudioSinkHandle = AudioEngineSink;

impl std::fmt::Debug for AudioEngineSink {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("AudioEngineSink")
      .field("group", &self.inner.group)
      .finish()
  }
}

struct AudioEngineSinkInner {
  backend_sink: Box<dyn AudioSink>,
  grouping: Weak<GroupingState>,
  group: AudioGroupId,
  group_state: Arc<AudioGroupState>,
  sink_volume_bits: AtomicU32,
}

impl AudioEngineSinkInner {
  fn sink_volume(&self) -> f32 {
    f32::from_bits(self.sink_volume_bits.load(Ordering::Relaxed))
  }

  fn set_sink_volume(&self, volume: f32) {
    let volume = sanitize_volume(volume);
    self.sink_volume_bits.store(volume.to_bits(), Ordering::Relaxed);
  }

  fn apply_effective_volume(&self) {
    let Some(grouping) = self.grouping.upgrade() else {
      return;
    };

    let gain = if grouping.master_muted() || self.group_state.muted() {
      0.0
    } else {
      grouping.master_volume() * self.group_state.volume() * self.sink_volume()
    };

    self.backend_sink.set_volume(gain);
  }
}

impl AudioSink for AudioEngineSink {
  fn config(&self) -> AudioStreamConfig {
    self.inner.backend_sink.config()
  }

  fn push_interleaved_f32(&self, samples: &[f32]) -> usize {
    self.inner.backend_sink.push_interleaved_f32(samples)
  }

  fn set_volume(&self, volume: f32) {
    self.inner.set_sink_volume(volume);
    self.inner.apply_effective_volume();
  }
}

fn sanitize_volume(volume: f32) -> f32 {
  if volume.is_finite() {
    volume.clamp(0.0, 1.0)
  } else {
    0.0
  }
}

/// Guard that restores the previous [`AudioEngine::global`] override when dropped.
pub struct AudioEngineTestGuard {
  previous: Option<Arc<AudioEngine>>,
}

impl Drop for AudioEngineTestGuard {
  fn drop(&mut self) {
    let lock = ENGINE_OVERRIDE.get_or_init(|| Mutex::new(None));
    *lock.lock() = self.previous.take();
  }
}

static ENGINE: OnceLock<Arc<AudioEngine>> = OnceLock::new();
static ENGINE_OVERRIDE: OnceLock<Mutex<Option<Arc<AudioEngine>>>> = OnceLock::new();

#[cfg(test)]
mod tests {
  use super::*;
  use crate::media::audio::NullAudioBackend;
  use crate::testing::global_test_lock;

  fn all_near(samples: &[f32], expected: f32) -> bool {
    samples
      .iter()
      .all(|sample| (*sample - expected).abs() < 1e-6)
  }

  #[test]
  fn audio_groups_muted_group_outputs_silence_but_still_drains_and_advances_clock() {
    let backend = Arc::new(NullAudioBackend::new_deterministic());
    let engine = AudioEngine::new_with_backend(audio_engine_config(), backend.clone());

    let group = engine.create_group();
    let sink = engine.create_sink_in_group(group);

    let cfg = engine.output_config();
    let channels = usize::from(cfg.channels.max(1));
    let frames = 512;
    let samples = vec![1.0f32; frames * channels];

    assert_eq!(sink.push_interleaved_f32(&samples), samples.len());

    let frames_before = backend.clock().frames();

    engine.set_group_muted(group, true);
    let out0 = backend.render(frames);
    assert!(all_near(&out0, 0.0));

    let frames_after = backend.clock().frames();
    assert_eq!(frames_after - frames_before, frames as u64);

    // Unmute: previously queued audio should have been drained while muted, so nothing should play.
    engine.set_group_muted(group, false);
    let out1 = backend.render(frames);
    assert!(all_near(&out1, 0.0));
  }

  #[test]
  fn audio_groups_master_mute_outputs_silence_but_still_drains_and_advances_clock() {
    let backend = Arc::new(NullAudioBackend::new_deterministic_with_defaults(48_000, 1));
    let engine = AudioEngine::new_with_backend(audio_engine_config(), backend.clone());

    let sink = engine.create_sink();

    let frames = 256;
    let samples = vec![1.0f32; frames];
    assert_eq!(sink.push_interleaved_f32(&samples), samples.len());

    let frames_before = backend.clock().frames();

    engine.set_master_muted(true);
    let out0 = backend.render(frames);
    assert!(all_near(&out0, 0.0));

    let frames_after = backend.clock().frames();
    assert_eq!(frames_after - frames_before, frames as u64);

    // Unmute: previously queued audio should have been drained while muted.
    engine.set_master_muted(false);
    let out1 = backend.render(frames);
    assert!(all_near(&out1, 0.0));
  }

  #[test]
  fn audio_groups_volume_is_master_times_group_times_sink() {
    let backend = Arc::new(NullAudioBackend::new_deterministic_with_defaults(48_000, 1));
    let engine = AudioEngine::new_with_backend(audio_engine_config(), backend.clone());

    let group = engine.create_group();
    let sink = engine.create_sink_in_group(group);

    engine.set_master_volume(0.5);
    engine.set_group_volume(group, 0.5);
    sink.set_volume(0.5);

    let frames = 32;
    let samples = vec![1.0f32; frames];
    assert_eq!(sink.push_interleaved_f32(&samples), samples.len());

    let out = backend.render(frames);
    assert!(all_near(&out, 0.125));
  }

  #[test]
  fn audio_engine_global_returns_singleton() {
    let _lock = global_test_lock();
    let a = AudioEngine::global();
    let b = AudioEngine::global();
    assert!(Arc::ptr_eq(&a, &b));
  }

  #[test]
  fn audio_engine_test_override_is_scoped_and_restores_global() {
    let _lock = global_test_lock();
    let base = AudioEngine::global();

    {
      let backend = Arc::new(NullAudioBackend::new_deterministic());
      let engine = AudioEngine::new_with_backend(audio_engine_config(), backend);
      let _guard = AudioEngine::init_for_test(engine);

      let overridden = AudioEngine::global();
      assert!(!Arc::ptr_eq(&base, &overridden));
    }

    let after = AudioEngine::global();
    assert!(Arc::ptr_eq(&base, &after));
  }
}
