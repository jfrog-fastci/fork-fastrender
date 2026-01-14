use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Once, OnceLock, Weak};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use std::panic::{catch_unwind, AssertUnwindSafe};

use parking_lot::{Mutex, RwLock};

use super::{
  frames_to_duration, next_device_id_for_name, AudioBackend, AudioClock, AudioDeviceInfo,
  AudioEngineConfig, AudioError, AudioOutputInfo, AudioSampleFormat, AudioSink, AudioStreamConfig,
  DeviceSelector,
};
use super::convert::sanitize_sample;
use crate::debug::trace::TraceHandle;
use crate::media::audio_engine::{
  AudioBackend as IdleBackend, AudioEngine as IdleEngine, AudioEngineTelemetry,
  AudioStreamHandle as IdleStreamHandle, DEFAULT_IDLE_TIMEOUT,
};
use super::limits::{MAX_BUFFERED_DURATION, MAX_CHANNELS, MAX_FRAMES_PER_PUSH, MAX_SAMPLE_RATE_HZ};
use super::panic_guard::{guard_output_callback, AudioSample};
use crate::media::audio::ring_buffer::{AudioRingBuffer, GainRamp};
use super::mixer_decision::{decide_mixer_callback_action, MixerCallbackAction};
use crate::media::audio_clock::InterpolatedAudioClock;
use super::restart::{AudioStreamFactory, ResilientStreamManager, RestartPolicy, RestartState};
use cpal::traits::{HostTrait, StreamTrait};

fn device_name_best_effort(device: &cpal::Device) -> String {
  use cpal::traits::DeviceTrait;
  device
    .name()
    .unwrap_or_else(|_| "<unknown output device>".to_string())
}

pub fn list_output_devices() -> Result<Vec<AudioDeviceInfo>, AudioError> {
  use cpal::traits::DeviceTrait;

  let host = cpal::default_host();
  let devices = host
    .output_devices()
    .map_err(|err| AudioError::OutputDeviceEnumerationFailed {
      source: Box::new(err),
    })?;

  let mut seen = std::collections::HashMap::<String, u32>::new();
  let mut out = Vec::new();
  for device in devices {
    let Ok(name) = device.name() else {
      continue;
    };
    let id = next_device_id_for_name(&mut seen, &name);
    out.push(AudioDeviceInfo { id, name });
  }
  Ok(out)
}

fn select_output_device(host: &cpal::Host, selector: &DeviceSelector) -> Result<cpal::Device, AudioError> {
  use cpal::traits::DeviceTrait;

  match selector {
    DeviceSelector::Default => host
      .default_output_device()
      .ok_or(AudioError::NoOutputDevice),
    DeviceSelector::Device(target) => {
      let devices = host
        .output_devices()
        .map_err(|err| AudioError::OutputDeviceEnumerationFailed {
          source: Box::new(err),
        })?;
      let mut seen = std::collections::HashMap::<String, u32>::new();
      for device in devices {
        let Ok(name) = device.name() else {
          continue;
        };
        let id = next_device_id_for_name(&mut seen, &name);
        if &id == target {
          return Ok(device);
        }
      }
      Err(AudioError::OutputDeviceNotFound {
        selector: selector.clone(),
      })
    }
  }
}

const STREAM_RESTART_POLL_INTERVAL: Duration = Duration::from_millis(50);
const STREAM_RESTART_MAX_ATTEMPTS: usize = 5;
const STREAM_RESTART_INITIAL_BACKOFF: Duration = Duration::from_millis(50);
const STREAM_RESTART_MAX_BACKOFF: Duration = Duration::from_millis(500);
// If CPAL stops invoking the output callback without triggering the error callback (can happen on
// some platforms during device loss), `InterpolatedAudioClock` will clamp and eventually stall the
// audio master clock. Detect this and force a restart.
const STREAM_STALL_TIMEOUT_MIN: Duration = Duration::from_millis(100);
const STREAM_STALL_TIMEOUT_MAX: Duration = Duration::from_secs(1);

static FALLBACK_WARN_ONCE: Once = Once::new();

/// Shared stream error state updated by CPAL's error callback.
struct StreamErrorState {
  pending: AtomicBool,
  count: AtomicU64,
}

impl StreamErrorState {
  fn new() -> Self {
    Self {
      pending: AtomicBool::new(false),
      count: AtomicU64::new(0),
    }
  }

  fn record(&self) {
    // Called from CPAL's error callback; keep it RT-safe (no allocation/locks).
    self.count.fetch_add(1, Ordering::Relaxed);
    self.pending.store(true, Ordering::Release);
  }
}

struct CpalStreamFactory {
  selector: DeviceSelector,
  expected: AudioStreamConfig,
  last_callback_frames: Arc<AtomicU32>,
  estimated_latency_nanos: Arc<AtomicU64>,
  mixer: Arc<MixerState>,
  clock: Arc<InterpolatedAudioClock>,
  diagnostics: Arc<CpalStreamDiagnostics>,
  errors: Arc<StreamErrorState>,
  trace: TraceHandle,
}

impl AudioStreamFactory for CpalStreamFactory {
  type Stream = cpal::Stream;
  type Error = AudioError;

  fn open_default_stream(&mut self) -> Result<Self::Stream, Self::Error> {
    let host = cpal::default_host();
    let device = match select_output_device(&host, &self.selector) {
      Ok(device) => device,
      // If the user-selected device disappears (hotplug), try to keep the browser usable by
      // switching to the host's default output device instead of immediately failing back to
      // silence.
      Err(AudioError::OutputDeviceNotFound { .. } | AudioError::OutputDeviceEnumerationFailed(_))
        if matches!(&self.selector, DeviceSelector::Device(_)) =>
      {
        let device = host
          .default_output_device()
          .ok_or(AudioError::NoOutputDevice)?;
        self.selector = DeviceSelector::Default;
        device
      }
      Err(err) => return Err(err),
    };
    let device_name = device_name_best_effort(&device);

    let (stream_config, cpal_sample_format, fixed_frames) = select_output_stream_config_matching(
      &device,
      &device_name,
      self.expected,
    )?;
    let config = AudioStreamConfig::new(stream_config.sample_rate.0, stream_config.channels);
    let sample_format = AudioSampleFormat::from(cpal_sample_format);

    self.last_callback_frames.store(0, Ordering::Relaxed);

    // Reset the latency estimate until the callback provides a better value.
    let initial_latency = fixed_frames
      .map(|frames| frames_to_duration(self.expected.sample_rate_hz, frames as u64))
      .unwrap_or_else(|| frames_to_duration(self.expected.sample_rate_hz, 1024));
    self
      .estimated_latency_nanos
      .store(duration_to_nanos_u64(initial_latency), Ordering::Relaxed);

    let stream = build_stream(
      &device,
      &device_name,
      &stream_config,
      cpal_sample_format,
      config,
      sample_format,
      self.mixer.clone(),
      self.clock.clone(),
      fixed_frames,
      self.last_callback_frames.clone(),
      self.estimated_latency_nanos.clone(),
      self.errors.clone(),
      self.diagnostics.clone(),
      self.trace.clone(),
    )?;
    stream
      .play()
      .map_err(|err| AudioError::StreamPlayFailed {
        device_name: device_name.clone(),
        source: Box::new(err),
      })?;
    Ok(stream)
  }
}

/// CPAL-based audio output backend (cross-platform).
///
/// Clocking notes:
/// - The exposed `AudioClock::OutputFrames` is derived from the number of frames written into the
///   CPAL output callback.
/// - This is a best-effort clock and does not currently model backend/device output latency, so it
///   may be ahead of “what the user hears” by a roughly constant buffer duration.
///
/// See `docs/media_clocking.md` for the intended A/V sync model (audio as master clock, tick as
/// wake-up only).

const DEFAULT_GAIN_RAMP_DURATION_MS: u32 = 10;
const DISC_STATE_NONE: u32 = 0;
const DISC_STATE_FADE_OUT: u32 = 1;
const DISC_STATE_WAIT_DATA: u32 = 2;

fn gain_ramp_frames(sample_rate_hz: u32) -> u32 {
  let frames = (u64::from(sample_rate_hz).saturating_mul(u64::from(DEFAULT_GAIN_RAMP_DURATION_MS))
    / 1000) as u32;
  frames.max(1)
}

enum StreamCommand {
  Start {
    reply: std::sync::mpsc::Sender<Result<(), AudioError>>,
  },
  Stop,
  Shutdown,
}

#[derive(Clone)]
struct CpalStreamControlBackend {
  command_tx: std::sync::mpsc::Sender<StreamCommand>,
}

impl IdleBackend for CpalStreamControlBackend {
  fn start_stream(&mut self) -> Result<(), AudioError> {
    let (tx, rx) = std::sync::mpsc::channel();
    self
      .command_tx
      .send(StreamCommand::Start { reply: tx })
      .map_err(|_| AudioError::BackendThreadTerminated { backend: "cpal" })?;
    rx.recv()
      .map_err(|_| AudioError::BackendThreadTerminated { backend: "cpal" })?
  }

  fn stop_stream(&mut self) {
    let _ = self.command_tx.send(StreamCommand::Stop);
  }
}
pub struct CpalAudioBackend {
  config: AudioStreamConfig,
  max_buffered_duration: Duration,
  fixed_callback_frames: Option<u32>,
  last_callback_frames: Arc<AtomicU32>,
  estimated_latency_nanos: Arc<AtomicU64>,
  mixer: Arc<MixerState>,
  clock: Arc<InterpolatedAudioClock>,
  diagnostics: Arc<CpalStreamDiagnostics>,
  fell_back_to_null: Arc<AtomicBool>,
  fallback_start: Arc<OnceLock<Instant>>,
  idle_engine: IdleEngine<CpalStreamControlBackend>,
  // `cpal::Stream` is neither `Send` nor `Sync`, so it cannot live inside a `Send + Sync`
  // `AudioBackend` implementation. Keep the stream on a dedicated thread and control its lifetime
  // via a shutdown channel + join handle.
  command_tx: std::sync::mpsc::Sender<StreamCommand>,
  stream_thread: Mutex<Option<JoinHandle<()>>>,
}

impl CpalAudioBackend {
  pub fn new() -> Result<Self, AudioError> {
    Self::new_with_device(DeviceSelector::Default)
  }

  pub fn new_with_device(selector: DeviceSelector) -> Result<Self, AudioError> {
    Self::new_with_config_and_device_and_trace(
      &super::audio_engine_config(),
      selector,
      TraceHandle::default(),
    )
  }

  pub fn new_with_config(engine_cfg: &AudioEngineConfig) -> Result<Self, AudioError> {
    Self::new_with_config_and_device_and_trace(
      engine_cfg,
      DeviceSelector::Default,
      TraceHandle::default(),
    )
  }

  pub(crate) fn new_with_config_and_trace(
    engine_cfg: &AudioEngineConfig,
    trace: TraceHandle,
  ) -> Result<Self, AudioError> {
    Self::new_with_config_and_device_and_trace(engine_cfg, DeviceSelector::Default, trace)
  }

  /// Returns whether the CPAL output callback has ever panicked.
  ///
  /// This is a sticky flag intended for telemetry and recovery logic; it is never reset.
  #[must_use]
  pub fn callback_panicked(&self) -> bool {
    self.diagnostics.panic_in_callback.load(Ordering::Relaxed)
  }

  fn new_with_config_and_device_and_trace(
    engine_cfg: &AudioEngineConfig,
    selector: DeviceSelector,
    trace: TraceHandle,
  ) -> Result<Self, AudioError> {
    // This comes from process-wide configuration (env vars), so clamp it defensively. The queue and
    // sink buffers must never be able to allocate unbounded memory.
    let max_buffered_duration = engine_cfg
      .per_stream_max_buffered_duration
      .min(MAX_BUFFERED_DURATION);
    let diagnostics = Arc::new(CpalStreamDiagnostics::new());
    let fell_back_to_null = Arc::new(AtomicBool::new(false));
    let fallback_start = Arc::new(OnceLock::new());

    // `cpal::Stream` is not `Send`/`Sync`, so it cannot live inside a `Send + Sync`
    // `AudioBackend` implementation. Keep the stream on a dedicated thread and control its
    // lifetime via a shutdown channel + join handle.
    type ReadyState = (
      AudioStreamConfig,
      Option<u32>,
      Arc<AtomicU32>,
      Arc<AtomicU64>,
      Arc<MixerState>,
      Arc<InterpolatedAudioClock>,
    );
    let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<ReadyState, AudioError>>();
    let (command_tx, command_rx) = std::sync::mpsc::channel::<StreamCommand>();

    let diagnostics_thread = diagnostics.clone();
    let thread_fell_back_to_null = fell_back_to_null.clone();
    let thread_fallback_start = fallback_start.clone();
    let thread = std::thread::spawn(move || {
      let selector = selector;
      let init = (|| -> Result<(ReadyState, Arc<StreamErrorState>), AudioError> {
        let host = cpal::default_host();
        let device = select_output_device(&host, &selector)?;
        let device_name = device_name_best_effort(&device);
        let (stream_config, _cpal_sample_format) =
          select_output_stream_config(&device, &device_name)?;
        let config = AudioStreamConfig::new(stream_config.sample_rate.0, stream_config.channels);
        let fixed_callback_frames = match stream_config.buffer_size {
          cpal::BufferSize::Fixed(frames) => Some(frames),
          cpal::BufferSize::Default => None,
        };
        let last_callback_frames = Arc::new(AtomicU32::new(0));

        // Start with a conservative estimate; the callback will refine this using timestamps (when
        // available) or observed callback sizes.
        let initial_latency = fixed_callback_frames
          .map(|frames| frames_to_duration(config.sample_rate_hz, frames as u64))
          .unwrap_or_else(|| frames_to_duration(config.sample_rate_hz, 1024));
        let estimated_latency_nanos =
          Arc::new(AtomicU64::new(duration_to_nanos_u64(initial_latency)));

        let clock = Arc::new(InterpolatedAudioClock::new(config.sample_rate_hz));
        let mixer = Arc::new(MixerState::new(config));
        let errors = Arc::new(StreamErrorState::new());
        Ok((
          (
            config,
            fixed_callback_frames,
            last_callback_frames,
            estimated_latency_nanos,
            mixer,
            clock,
          ),
          errors,
        ))
      })();

      let (ready, errors) = match init {
        Ok(ok) => ok,
        Err(err) => {
          let _ = ready_tx.send(Err(err));
          return;
        }
      };

      let _ = ready_tx.send(Ok(ready.clone()));

      let (config, fixed_callback_frames, last_callback_frames, estimated_latency_nanos, mixer, clock) =
        ready;
      let sample_rate_hz = config.sample_rate_hz;
      let clock_for_fallback = clock.clone();
      let last_callback_frames_watchdog = last_callback_frames.clone();
      let policy = RestartPolicy {
        max_attempts: STREAM_RESTART_MAX_ATTEMPTS,
        initial_backoff: STREAM_RESTART_INITIAL_BACKOFF,
        max_backoff: STREAM_RESTART_MAX_BACKOFF,
      };
      let factory = CpalStreamFactory {
        selector,
        expected: config,
        last_callback_frames,
        estimated_latency_nanos,
        mixer,
        clock,
        diagnostics: diagnostics_thread,
        errors: errors.clone(),
        trace,
      };
      // Start suspended: do not open an OS audio device until we actually have active audio.
      let mut manager = ResilientStreamManager::new(factory, policy, Instant::now());
      let mut suspended = true;

      let mut last_frames_written = clock_for_fallback.frames_written();
      let mut last_progress_at = Instant::now();
      // CPAL can take a little time before the first callback arrives after (re)opening a stream.
      // Use a more conservative stall timeout until we observe callback progress.
      let mut awaiting_first_callback =
        last_callback_frames_watchdog.load(Ordering::Relaxed) == 0;
      let mut consecutive_unhealthy_restarts: usize = 0;

      let enter_fallback = |now: Instant| {
        let played = clock_for_fallback.now_at(now);
        let start = now.checked_sub(played).unwrap_or(now);
        let _ = thread_fallback_start.set(start);
        thread_fell_back_to_null.store(true, Ordering::Release);
        FALLBACK_WARN_ONCE.call_once(|| {
          eprintln!(
            "warning: CPAL output stream failed and could not be restarted; falling back to NullAudioBackend (silence)"
          );
        });
      };

      loop {
        if suspended {
          let cmd = match command_rx.recv() {
            Ok(cmd) => cmd,
            Err(_) => break,
          };
          match cmd {
            StreamCommand::Start { reply } => {
              suspended = false;
              let now = Instant::now();
              manager.request_restart(now);
              let out = manager.tick(now);
              if out.opened_stream {
                // Reset stall watchdog state when resuming from an intentional idle suspend. The
                // callback playhead does not advance while suspended, so without this we can
                // immediately treat the long idle window as a "callback stall" and churn
                // start/stop/restart the output stream.
                last_progress_at = now;
                last_frames_written = clock_for_fallback.frames_written();
                awaiting_first_callback = true;
              }
              let _ = reply.send(Ok(()));
            }
            StreamCommand::Stop => {}
            StreamCommand::Shutdown => break,
          }
          continue;
        }

        let now = Instant::now();

        // Detect output callback stalls even if CPAL never invokes the error callback (e.g. some
        // device-loss/hotplug scenarios). Without this, `InterpolatedAudioClock` will clamp and
        // eventually stop advancing, hanging media playback.
        if matches!(manager.state(), RestartState::Running) {
          let frames_written = clock_for_fallback.frames_written();
          if frames_written != last_frames_written {
            last_frames_written = frames_written;
            last_progress_at = now;
            consecutive_unhealthy_restarts = 0;
            awaiting_first_callback = false;
          } else {
            // Prefer the observed callback size once we have it (e.g. after a stream restart where
            // the device selected a different buffer size).
            let callback_frames_hint = {
              let v = last_callback_frames_watchdog.load(Ordering::Relaxed);
              if v != 0 { Some(v) } else { fixed_callback_frames }
            };
            let stall_timeout = if awaiting_first_callback {
              STREAM_STALL_TIMEOUT_MAX
            } else {
              let callback_duration = callback_frames_hint
                .map(|frames| frames_to_duration(sample_rate_hz, frames as u64))
                .unwrap_or(STREAM_RESTART_POLL_INTERVAL);
              let mut stall_timeout = callback_duration.saturating_mul(10);
              if stall_timeout < STREAM_STALL_TIMEOUT_MIN {
                stall_timeout = STREAM_STALL_TIMEOUT_MIN;
              }
              if stall_timeout > STREAM_STALL_TIMEOUT_MAX {
                stall_timeout = STREAM_STALL_TIMEOUT_MAX;
              }
              stall_timeout
            };
            if now.duration_since(last_progress_at) >= stall_timeout {
              consecutive_unhealthy_restarts =
                consecutive_unhealthy_restarts.saturating_add(1);
              if consecutive_unhealthy_restarts >= STREAM_RESTART_MAX_ATTEMPTS {
                enter_fallback(now);
                break;
              }
              manager.request_restart(now);
              last_progress_at = now;
            }
          }
        }

        // Stream error callback can fire repeatedly even after we request a restart; don't reset the
        // restart backoff state while we're already in the middle of restarting.
        if errors.pending.swap(false, Ordering::AcqRel)
          && matches!(manager.state(), RestartState::Running)
        {
          consecutive_unhealthy_restarts =
            consecutive_unhealthy_restarts.saturating_add(1);
          if consecutive_unhealthy_restarts >= STREAM_RESTART_MAX_ATTEMPTS {
            enter_fallback(now);
            break;
          }
          manager.request_restart(now);
          last_progress_at = now;
        }

        let out = manager.tick(now);
        if out.opened_stream {
          // Give the new stream time to start invoking callbacks before the stall watchdog kicks in.
          last_progress_at = now;
          last_frames_written = clock_for_fallback.frames_written();
          awaiting_first_callback = true;
        }
        if out.entered_fallback {
          enter_fallback(now);
          break;
        }

        let sleep = manager
          .next_attempt_at()
          .and_then(|next| next.checked_duration_since(now))
          .map(|dur| dur.min(STREAM_RESTART_POLL_INTERVAL))
          .unwrap_or(STREAM_RESTART_POLL_INTERVAL);

        match command_rx.recv_timeout(sleep) {
          Ok(cmd) => match cmd {
            StreamCommand::Start { reply } => {
              let _ = reply.send(Ok(()));
            }
            StreamCommand::Stop => {
              suspended = true;
              manager.request_restart(Instant::now());
            }
            StreamCommand::Shutdown => break,
          },
          Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
          Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
        }
      }
    });

    let (
      config,
      fixed_callback_frames,
      last_callback_frames,
      estimated_latency_nanos,
      mixer,
      clock,
    ) = match ready_rx.recv() {
      Ok(Ok(ok)) => ok,
      Ok(Err(err)) => {
        let _ = thread.join();
        return Err(err);
      }
       Err(_) => {
         let _ = thread.join();
         return Err(AudioError::BackendThreadTerminated { backend: "cpal" });
       }
     };

    let idle_backend = CpalStreamControlBackend {
      command_tx: command_tx.clone(),
    };
    let idle_engine = IdleEngine::with_idle_timeout(idle_backend, DEFAULT_IDLE_TIMEOUT);
    // Ensure the stream is suspended after the debounce window even when the embedder doesn't have a
    // central tick loop.
    idle_engine.spawn_idle_watcher();

    Ok(Self {
      config,
      max_buffered_duration,
      fixed_callback_frames,
      last_callback_frames,
      estimated_latency_nanos,
      mixer,
      clock,
      diagnostics,
      fell_back_to_null,
      fallback_start,
      idle_engine,
      command_tx,
      stream_thread: Mutex::new(Some(thread)),
    })
  }

  fn report_warnings_once(&self) {
    self.diagnostics.report_warnings_once();
  }

  /// Snapshot of the debounced output-stream lifecycle state (useful for telemetry/debugging).
  pub fn idle_telemetry(&self) -> AudioEngineTelemetry {
    self.idle_engine.telemetry()
  }
}

impl Drop for CpalAudioBackend {
  fn drop(&mut self) {
    let _ = self.command_tx.send(StreamCommand::Shutdown);
    if let Some(handle) = self.stream_thread.lock().take() {
      // Avoid panicking if the backend is dropped on its own stream thread.
      if handle.thread().id() != std::thread::current().id() {
        let _ = handle.join();
      }
    }
  }
}

impl AudioBackend for CpalAudioBackend {
  fn output_config(&self) -> AudioStreamConfig {
    self.report_warnings_once();
    self.config
  }

  fn output_info(&self) -> AudioOutputInfo {
    self.report_warnings_once();
    if self.fell_back_to_null.load(Ordering::Acquire) {
      return AudioOutputInfo {
        config: self.config,
        callback_frames: None,
        estimated_output_latency: Duration::ZERO,
        backend_name: "null",
      };
    }

    // Prefer the observed callback size once available, since a restarted stream may choose a
    // different buffer size than the initial stream config.
    let last = self.last_callback_frames.load(Ordering::Relaxed);
    let callback_frames = if last != 0 {
      Some(last)
    } else {
      self.fixed_callback_frames
    };

    AudioOutputInfo {
      config: self.config,
      callback_frames,
      estimated_output_latency: Duration::from_nanos(
        self.estimated_latency_nanos.load(Ordering::Relaxed),
      ),
      backend_name: "cpal",
    }
  }

  fn clock(&self) -> AudioClock {
    self.report_warnings_once();
    if self.fell_back_to_null.load(Ordering::Acquire) {
      let start = self
        .fallback_start
        .get()
        .copied()
        .unwrap_or_else(Instant::now);
      return AudioClock::Instant {
        start,
        sample_rate_hz: self.config.sample_rate_hz,
      };
    }
    AudioClock::OutputFrames {
      clock: self.clock.clone(),
    }
  }

  fn create_sink(&self) -> Box<dyn AudioSink> {
    self.report_warnings_once();
    let activity = self.idle_engine.register_stream();
    let sink = Arc::new(SinkState::new(
      self.config,
      self.max_buffered_duration,
      activity,
    ));
    self.mixer.register_sink(&sink);
    Box::new(CpalAudioSink { state: sink })
  }
}

/// Thread-safe, allocation-free diagnostics shared between the non-RT host code and the RT output
/// callback.
///
/// This is intentionally minimal:
/// - All state updates in the RT callback are atomic and never allocate.
/// - Warnings are printed at most once from a non-RT context.
struct CpalStreamDiagnostics {
  /// Sticky flag: set to true if the CPAL output callback panics and we had to recover by outputting
  /// silence.
  panic_in_callback: AtomicBool,
  /// Non-zero if the CPAL stream error callback was invoked.
  ///
  /// We do not store the full error string because that would require allocation in the RT error
  /// callback. Instead we record a simple code and print a generic warning once from a non-RT
  /// context.
  stream_error_code: AtomicU32,
  reported_panic: AtomicBool,
  reported_stream_error: AtomicBool,
}

impl CpalStreamDiagnostics {
  fn new() -> Self {
    Self {
      panic_in_callback: AtomicBool::new(false),
      stream_error_code: AtomicU32::new(0),
      reported_panic: AtomicBool::new(false),
      reported_stream_error: AtomicBool::new(false),
    }
  }

  fn set_panic_in_callback(&self) {
    self.panic_in_callback.store(true, Ordering::Relaxed);
  }

  fn set_stream_error(&self, code: u32) {
    // Keep the first error code so we have at least some signal about what happened.
    let _ = self.stream_error_code.compare_exchange(
      0,
      code.max(1),
      Ordering::Relaxed,
      Ordering::Relaxed,
    );
  }

  fn report_warnings_once(&self) {
    if self.panic_in_callback.load(Ordering::Relaxed)
      && !self.reported_panic.swap(true, Ordering::Relaxed)
    {
      eprintln!("warning: CPAL output callback panicked; audio output has been silenced");
    }

    let code = self.stream_error_code.load(Ordering::Relaxed);
    if code != 0 && !self.reported_stream_error.swap(true, Ordering::Relaxed) {
      eprintln!("warning: CPAL output stream reported an error (code {code})");
    }
  }
}

struct MixerState {
  config: AudioStreamConfig,
  sinks: RwLock<Vec<Weak<SinkState>>>,
}

#[derive(Debug, Clone, Copy, Default)]
struct MixStats {
  streams: u64,
  buffered_frames: u64,
  underruns: u64,
}

impl MixerState {
  fn new(config: AudioStreamConfig) -> Self {
    Self {
      config,
      sinks: RwLock::new(Vec::new()),
    }
  }

  fn register_sink(&self, sink: &Arc<SinkState>) {
    let mut sinks = self.sinks.write();
    sinks.retain(|weak| weak.upgrade().is_some());
    sinks.push(Arc::downgrade(sink));
  }

  /// Decide whether the current callback cycle needs full mixing work.
  ///
  /// When no sinks are audible (all muted/empty), we can output silence without touching the per-sample
  /// mixing hot path. When outputting silence we still drain **fully muted** sinks so they don't
  /// accumulate buffered audio.
  fn mix_for_callback(&self, output_samples: usize) -> MixerCallbackAction {
    let sinks = self.sinks.read();
    let mut has_sinks = false;

    for weak in sinks.iter() {
      let Some(sink) = weak.upgrade() else {
        continue;
      };
      has_sinks = true;

      // Fully muted sinks never make the output audible; skip them for the decision. They'll be
      // drained in `mix_into` (mixing path) or via `pop_discard` (silence path).
      if sink.is_fully_muted() {
        continue;
      }

      if sink.maybe_audible.load(Ordering::Relaxed) {
        return MixerCallbackAction::Mix;
      }
    }

    let action = decide_mixer_callback_action(has_sinks, false, true);
    if action == MixerCallbackAction::SilenceAndDrain {
      let channels = self.channels_usize().max(1);
      // Keep frame alignment when draining so channel interleaving stays consistent.
      let output_samples = output_samples - (output_samples % channels);

      if output_samples != 0 {
        for weak in sinks.iter() {
          let Some(sink) = weak.upgrade() else {
            continue;
          };

          if sink.is_fully_muted() {
            sink.buffer.pop_discard(output_samples);
            sink.maybe_audible.store(false, Ordering::Relaxed);
            if sink.buffer.is_empty() {
              // Best-effort: if the engine is already torn down, ignore.
              let _ = sink.activity.set_active(false);
            }
          }
        }
      }
    }

    action
  }

  fn mix_into(&self, dst: &mut [f32]) {
    let channels = self.channels_usize().max(1);
    let to_drain = dst.len() - (dst.len() % channels);

    let sinks = self.sinks.read();
    for weak in sinks.iter() {
      let Some(sink) = weak.upgrade() else {
        continue;
      };

      if sink.is_fully_muted() {
        sink.buffer.pop_discard(to_drain);
        sink.maybe_audible.store(false, Ordering::Relaxed);
        if sink.buffer.is_empty() {
          // Best-effort: if the engine is already torn down, ignore.
          let _ = sink.activity.set_active(false);
        }
        continue;
      }

      sink.mix_into(dst);
      if sink.buffer.is_empty() {
        // Best-effort: if the engine is already torn down, ignore.
        let _ = sink.activity.set_active(false);
      }
    }
  }

  /// Discard buffered audio from all sinks.
  ///
  /// This is intended for situations where the output callback must output silence but still needs
  /// to keep internal sink buffers from building up backpressure (e.g. after a callback panic).
  fn drain_all_sinks(&self, output_samples: usize) {
    let channels = self.channels_usize().max(1);
    let to_drain = output_samples - (output_samples % channels);
    if to_drain == 0 {
      return;
    }

    let sinks = self.sinks.read();
    for weak in sinks.iter() {
      let Some(sink) = weak.upgrade() else {
        continue;
      };

      sink.buffer.pop_discard(to_drain);
      sink.maybe_audible.store(false, Ordering::Relaxed);
      if sink.buffer.is_empty() {
        // Best-effort: if the engine is already torn down, ignore.
        let _ = sink.activity.set_active(false);
      }
    }
  }

  fn stats_for_output_len(&self, output_samples_len: usize) -> MixStats {
    let channels = self.channels_usize();
    let mut stats = MixStats {
      streams: 0,
      buffered_frames: u64::MAX,
      underruns: 0,
    };

    let sinks = self.sinks.read();
    for weak in sinks.iter() {
      let Some(sink) = weak.upgrade() else {
        continue;
      };
      stats.streams = stats.streams.saturating_add(1);

      let available_samples = sink.buffer.buffered_samples();
      let buffered_frames = (available_samples / channels) as u64;
      stats.buffered_frames = stats.buffered_frames.min(buffered_frames);
      if available_samples < output_samples_len {
        stats.underruns = stats.underruns.saturating_add(1);
      }
    }

    if stats.streams == 0 || stats.buffered_frames == u64::MAX {
      stats.buffered_frames = 0;
    }

    stats
  }

  fn channels_usize(&self) -> usize {
    usize::from(self.config.channels.max(1))
  }
}

struct SinkState {
  config: AudioStreamConfig,
  buffer: AudioRingBuffer,
  activity: IdleStreamHandle<CpalStreamControlBackend>,
  volume_target_bits: AtomicU32,
  discontinuity_state: AtomicU32,
  ramp_target_bits: AtomicU32,
  ramp_current_bits: AtomicU32,
  ramp_step_bits: AtomicU32,
  ramp_remaining_frames: AtomicU32,
  ramp_frames: u32,
  maybe_audible: AtomicBool,
}

impl SinkState {
  fn new(
    config: AudioStreamConfig,
    max_buffered: Duration,
    activity: IdleStreamHandle<CpalStreamControlBackend>,
  ) -> Self {
    let channels = usize::from(config.channels.max(1));
    let frames = super::duration_to_frames_ceil(config.sample_rate_hz, max_buffered);
    let frames = usize::try_from(frames).unwrap_or(usize::MAX);
    let capacity = frames.saturating_mul(channels).max(1);
    let ramp_frames = gain_ramp_frames(config.sample_rate_hz);
    Self {
      config,
      buffer: AudioRingBuffer::new(capacity),
      activity,
      volume_target_bits: AtomicU32::new(1.0f32.to_bits()),
      discontinuity_state: AtomicU32::new(DISC_STATE_NONE),
      ramp_target_bits: AtomicU32::new(1.0f32.to_bits()),
      ramp_current_bits: AtomicU32::new(1.0f32.to_bits()),
      ramp_step_bits: AtomicU32::new(0.0f32.to_bits()),
      ramp_remaining_frames: AtomicU32::new(0),
      ramp_frames,
      maybe_audible: AtomicBool::new(false),
    }
  }

  #[inline]
  fn gain_nonzero_for_hint(&self, volume_target: f32) -> bool {
    let current_bits = self.ramp_current_bits.load(Ordering::Relaxed);
    let current = f32::from_bits(current_bits);
    volume_target > 0.0 || (current.is_finite() && current > 0.0)
  }

  #[inline]
  fn is_fully_muted(&self) -> bool {
    let target = f32::from_bits(self.volume_target_bits.load(Ordering::Relaxed));
    let target = if target.is_finite() { target } else { 0.0 };
    if self.gain_nonzero_for_hint(target) {
      return false;
    }
    // If we're mid-ramp, keep processing so we converge to the final state.
    self.ramp_remaining_frames.load(Ordering::Relaxed) == 0
  }

  fn set_volume(&self, volume: f32) {
    let volume = if volume.is_finite() {
      volume.clamp(0.0, 1.0)
    } else {
      0.0
    };
    self
      .volume_target_bits
      .store(volume.to_bits(), Ordering::Relaxed);
    let gain_nonzero = self.gain_nonzero_for_hint(volume);
    if gain_nonzero && self.buffer.has_data() {
      self.maybe_audible.store(true, Ordering::Relaxed);
    } else {
      self.maybe_audible.store(false, Ordering::Relaxed);
    }
  }

  fn notify_discontinuity(&self) {
    // Trigger a short fade-out of any currently queued audio. Once the fade completes, the sink
    // transitions into a "wait for new data" state where it will fade back in from zero on the
    // next non-empty buffer.
    self
      .discontinuity_state
      .store(DISC_STATE_FADE_OUT, Ordering::Relaxed);

    // If there's nothing to fade out, immediately arm the fade-in. This handles cases where the
    // discontinuity occurs while the stream is paused/empty and the mixer would otherwise never
    // call into `mix_into` (silence fast-path).
    if !self.buffer.has_data() {
      self.discontinuity_state.store(DISC_STATE_WAIT_DATA, Ordering::Relaxed);
      self.force_ramp_to_zero();
      self.maybe_audible.store(false, Ordering::Relaxed);
    }
  }

  #[inline]
  fn force_ramp_to_zero(&self) {
    let zero_bits = 0.0f32.to_bits();
    self.ramp_target_bits.store(zero_bits, Ordering::Relaxed);
    self.ramp_current_bits.store(zero_bits, Ordering::Relaxed);
    self.ramp_step_bits.store(0.0f32.to_bits(), Ordering::Relaxed);
    self.ramp_remaining_frames.store(0, Ordering::Relaxed);
  }

  fn mix_into(&self, dst: &mut [f32]) {
    let channels = usize::from(self.config.channels.max(1));
    if channels == 0 || dst.is_empty() {
      return;
    }

    let mut discontinuity_state = self.discontinuity_state.load(Ordering::Relaxed);
    if discontinuity_state > DISC_STATE_WAIT_DATA {
      discontinuity_state = DISC_STATE_NONE;
    }

    // If the buffer is empty, ensure the hint is cleared so the callback can fast-path to silence.
    if !self.buffer.has_data() {
      if discontinuity_state == DISC_STATE_FADE_OUT {
        self.discontinuity_state.store(DISC_STATE_WAIT_DATA, Ordering::Relaxed);
        self.force_ramp_to_zero();
      }
      self.maybe_audible.store(false, Ordering::Relaxed);
      return;
    }

    if discontinuity_state == DISC_STATE_WAIT_DATA {
      // Fresh data has arrived after a discontinuity; clear the state so the sink ramps back up
      // from zero (ramp state is forced to 0 when entering WAIT_DATA).
      self
        .discontinuity_state
        .store(DISC_STATE_NONE, Ordering::Relaxed);
      discontinuity_state = DISC_STATE_NONE;
    }

    let volume_target_bits = self.volume_target_bits.load(Ordering::Relaxed);
    let volume_target = f32::from_bits(volume_target_bits);
    let volume_target = if volume_target.is_finite() { volume_target } else { 0.0 };

    let desired_target_bits = if discontinuity_state == DISC_STATE_FADE_OUT {
      0.0f32.to_bits()
    } else {
      volume_target_bits
    };
    let mut ramp_target_bits = self.ramp_target_bits.load(Ordering::Relaxed);

    let mut current = f32::from_bits(self.ramp_current_bits.load(Ordering::Relaxed));
    let mut target = f32::from_bits(ramp_target_bits);
    let mut step = f32::from_bits(self.ramp_step_bits.load(Ordering::Relaxed));
    let mut remaining = self.ramp_remaining_frames.load(Ordering::Relaxed);

    if desired_target_bits != ramp_target_bits {
      ramp_target_bits = desired_target_bits;
      target = f32::from_bits(desired_target_bits);

      if (current - target).abs() <= f32::EPSILON {
        current = target;
        step = 0.0;
        remaining = 0;
      } else {
        remaining = self.ramp_frames;
        step = (target - current) / remaining as f32;
      }
    }

    let mut ramp = GainRamp {
      current_gain: current,
      target_gain: target,
      step,
      frames_remaining: remaining,
    };

    self.buffer.pop_add_into_ramped(dst, channels, &mut ramp);

    self
      .ramp_target_bits
      .store(ramp_target_bits, Ordering::Relaxed);
    self
      .ramp_current_bits
      .store(ramp.current_gain.to_bits(), Ordering::Relaxed);
    self
      .ramp_step_bits
      .store(ramp.step.to_bits(), Ordering::Relaxed);
    self
      .ramp_remaining_frames
      .store(ramp.frames_remaining, Ordering::Relaxed);

    let mut has_data = self.buffer.has_data();
    let mut gain_nonzero = (ramp.target_gain.is_finite() && ramp.target_gain > 0.0)
      || (ramp.current_gain.is_finite() && ramp.current_gain > 0.0);

    // After a discontinuity, fade out to silence, then discard any remaining queued samples so the
    // next playback segment starts from a clean slate and can fade back in from zero.
    if discontinuity_state == DISC_STATE_FADE_OUT {
      let fade_complete =
        ramp.frames_remaining == 0 && ramp.current_gain == 0.0 && ramp.target_gain == 0.0;

      if fade_complete {
        self.buffer.pop_discard(usize::MAX);
        self.discontinuity_state.store(DISC_STATE_WAIT_DATA, Ordering::Relaxed);
        self.force_ramp_to_zero();
        has_data = self.buffer.has_data();
        gain_nonzero = self.gain_nonzero_for_hint(volume_target);
      } else if !has_data {
        // Ran out of buffered audio before the fade completed; ensure the next non-empty push
        // fades in from silence rather than resuming at a partial gain.
        self.discontinuity_state.store(DISC_STATE_WAIT_DATA, Ordering::Relaxed);
        self.force_ramp_to_zero();
        has_data = self.buffer.has_data();
        gain_nonzero = self.gain_nonzero_for_hint(volume_target);
      }
    }
    self
      .maybe_audible
      .store(has_data && gain_nonzero, Ordering::Relaxed);
  }
}

struct CpalAudioSink {
  state: Arc<SinkState>,
}

impl AudioSink for CpalAudioSink {
  fn config(&self) -> AudioStreamConfig {
    self.state.config
  }

  fn push_interleaved_f32(&self, samples: &[f32]) -> usize {
    let channels = usize::from(self.state.config.channels.max(1));
    let usable_len = samples.len() - (samples.len() % channels);
    let frames = usable_len / channels;
    let frames = frames.min(MAX_FRAMES_PER_PUSH);
    let capped_len = frames * channels;
    if capped_len == 0 {
      return 0;
    }

    // If the sink may produce audible output, set the hint before publishing samples so the
    // callback can avoid racing between the ring-buffer write becoming visible and the hint update.
    let volume_bits = self.state.volume_target_bits.load(Ordering::Relaxed);
    let volume = f32::from_bits(volume_bits);
    let volume = if volume.is_finite() { volume } else { 0.0 };
    if self.state.gain_nonzero_for_hint(volume) {
      self.state.maybe_audible.store(true, Ordering::Relaxed);
    }

    let written = self.state.buffer.push(&samples[..capped_len]);

    // Re-assert after the write so a concurrent callback maintenance pass can't clobber us.
    if written != 0 {
      let volume_bits = self.state.volume_target_bits.load(Ordering::Relaxed);
      let volume = f32::from_bits(volume_bits);
      let volume = if volume.is_finite() { volume } else { 0.0 };
      if self.state.gain_nonzero_for_hint(volume) {
        self.state.maybe_audible.store(true, Ordering::Relaxed);
      }
      // Mark this sink as active so the output stream is started (if needed) and kept alive long
      // enough to play out (or drain) the buffered samples.
      let _ = self.state.activity.set_active(true);
    }

    written
  }

  fn set_volume(&self, volume: f32) {
    self.state.set_volume(volume);
  }

  fn notify_discontinuity(&self) {
    self.state.notify_discontinuity();
  }
}

fn select_output_stream_config(
  device: &cpal::Device,
  device_name: &str,
) -> Result<(cpal::StreamConfig, cpal::SampleFormat), AudioError> {
  use cpal::traits::DeviceTrait;

  let mut best: Option<(cpal::SupportedStreamConfig, (u8, u8, u8))> = None;

  if let Ok(configs) = device.supported_output_configs() {
    for range in configs {
      let fmt_score = match range.sample_format() {
        cpal::SampleFormat::F32 => 3,
        cpal::SampleFormat::I16 => 2,
        cpal::SampleFormat::U16 => 1,
        _ => 0,
      };
      if fmt_score == 0 {
        continue;
      }

      let channels = range.channels();
      if channels == 0 || channels > MAX_CHANNELS {
        continue;
      }
      let channel_score = match channels {
        2 => 2,
        1 => 1,
        _ => 0,
      };

      let min_rate = range.min_sample_rate().0;
      let max_rate = range.max_sample_rate().0;
      let chosen_rate = if min_rate <= 48_000 && 48_000 <= max_rate {
        48_000
      } else if min_rate <= 44_100 && 44_100 <= max_rate {
        44_100
      } else {
        let capped_max = max_rate.min(MAX_SAMPLE_RATE_HZ);
        if capped_max < min_rate {
          continue;
        }
        capped_max
      };
      if chosen_rate == 0 {
        continue;
      }
      let rate_score = if chosen_rate == 48_000 {
        2
      } else if chosen_rate == 44_100 {
        1
      } else {
        0
      };

      let cfg = range.with_sample_rate(cpal::SampleRate(chosen_rate));
      let score = (fmt_score, channel_score, rate_score);

      match best.as_ref() {
        Some((_, best_score)) if *best_score >= score => {}
        _ => best = Some((cfg, score)),
      }
    }
  }

  let supported = if let Some((cfg, _)) = best {
    cfg
  } else {
    device
      .default_output_config()
      .map_err(|err| AudioError::DefaultOutputConfigFailed {
        device_name: device_name.to_string(),
        source: Box::new(err),
      })?
  };

  let sample_format = supported.sample_format();
  let config: cpal::StreamConfig = supported.into();
  if config.channels == 0 || config.channels > MAX_CHANNELS {
    return Err(AudioError::invalid_spec(format!(
      "unsupported output channel count {}",
      config.channels
    )));
  }
  if config.sample_rate.0 == 0 || config.sample_rate.0 > MAX_SAMPLE_RATE_HZ {
    return Err(AudioError::invalid_spec(format!(
      "unsupported output sample rate {}",
      config.sample_rate.0
    )));
  }
  Ok((config, sample_format))
}

fn select_output_stream_config_matching(
  device: &cpal::Device,
  device_name: &str,
  expected: AudioStreamConfig,
) -> Result<(cpal::StreamConfig, cpal::SampleFormat, Option<u32>), AudioError> {
  use cpal::traits::DeviceTrait;

  let mut best: Option<(cpal::SupportedStreamConfig, u8)> = None;

  if let Ok(configs) = device.supported_output_configs() {
    for range in configs {
      if range.channels() != expected.channels {
        continue;
      }

      let min_rate = range.min_sample_rate().0;
      let max_rate = range.max_sample_rate().0;
      if !(min_rate <= expected.sample_rate_hz && expected.sample_rate_hz <= max_rate) {
        continue;
      }

      let fmt_score = match range.sample_format() {
        cpal::SampleFormat::F32 => 3,
        cpal::SampleFormat::I16 => 2,
        cpal::SampleFormat::U16 => 1,
        _ => 0,
      };
      if fmt_score == 0 {
        continue;
      }

      let cfg = range.with_sample_rate(cpal::SampleRate(expected.sample_rate_hz));
      match best.as_ref() {
        Some((_, best_score)) if *best_score >= fmt_score => {}
        _ => best = Some((cfg, fmt_score)),
      }
    }
  }

  let supported = if let Some((cfg, _)) = best {
    cfg
  } else {
    let cfg = device
      .default_output_config()
      .map_err(|err| AudioError::DefaultOutputConfigFailed {
        device_name: device_name.to_string(),
        source: Box::new(err),
      })?;

    if cfg.channels() != expected.channels || cfg.sample_rate().0 != expected.sample_rate_hz {
      return Err(AudioError::StreamConfigMismatch {
        expected_channels: usize::from(expected.channels),
        expected_sample_rate_hz: expected.sample_rate_hz,
        channels: usize::from(cfg.channels()),
        sample_rate_hz: cfg.sample_rate().0,
      });
    }
    cfg
  };

  let sample_format = supported.sample_format();
  let config: cpal::StreamConfig = supported.into();
  if config.channels == 0 || config.channels > MAX_CHANNELS {
    return Err(AudioError::invalid_spec(format!(
      "unsupported output channel count {}",
      config.channels
    )));
  }
  if config.sample_rate.0 == 0 || config.sample_rate.0 > MAX_SAMPLE_RATE_HZ {
    return Err(AudioError::invalid_spec(format!(
      "unsupported output sample rate {}",
      config.sample_rate.0
    )));
  }

  let fixed_frames = match config.buffer_size {
    cpal::BufferSize::Fixed(frames) => Some(frames),
    cpal::BufferSize::Default => None,
  };
  Ok((config, sample_format, fixed_frames))
}

fn build_stream(
  device: &cpal::Device,
  device_name: &str,
  config: &cpal::StreamConfig,
  cpal_sample_format: cpal::SampleFormat,
  stream_config: AudioStreamConfig,
  sample_format: AudioSampleFormat,
  mixer: Arc<MixerState>,
  clock: Arc<InterpolatedAudioClock>,
  fixed_callback_frames: Option<u32>,
  last_callback_frames: Arc<AtomicU32>,
  estimated_latency_nanos: Arc<AtomicU64>,
  errors: Arc<StreamErrorState>,
  diagnostics: Arc<CpalStreamDiagnostics>,
  trace: TraceHandle,
) -> Result<cpal::Stream, AudioError> {
  match cpal_sample_format {
    cpal::SampleFormat::F32 => build_stream_typed::<f32>(
      device,
      device_name,
      config,
      stream_config,
      sample_format,
      mixer,
      clock,
      fixed_callback_frames,
      last_callback_frames,
      estimated_latency_nanos,
      errors.clone(),
      diagnostics.clone(),
      trace.clone(),
    ),
    cpal::SampleFormat::I16 => build_stream_typed::<i16>(
      device,
      device_name,
      config,
      stream_config,
      sample_format,
      mixer,
      clock,
      fixed_callback_frames,
      last_callback_frames,
      estimated_latency_nanos,
      errors.clone(),
      diagnostics.clone(),
      trace.clone(),
    ),
    cpal::SampleFormat::U16 => build_stream_typed::<u16>(
      device,
      device_name,
      config,
      stream_config,
      sample_format,
      mixer,
      clock,
      fixed_callback_frames,
      last_callback_frames,
      estimated_latency_nanos,
      errors,
      diagnostics,
      trace,
    ),
    other => Err(AudioError::UnsupportedSampleFormat {
      device_name: device_name.to_string(),
      sample_format: AudioSampleFormat::from(other),
    }),
  }
}

fn build_stream_typed<T>(
  device: &cpal::Device,
  device_name: &str,
  config: &cpal::StreamConfig,
  stream_config: AudioStreamConfig,
  sample_format: AudioSampleFormat,
  mixer: Arc<MixerState>,
  clock: Arc<InterpolatedAudioClock>,
  fixed_callback_frames: Option<u32>,
  last_callback_frames: Arc<AtomicU32>,
  estimated_latency_nanos: Arc<AtomicU64>,
  errors: Arc<StreamErrorState>,
  diagnostics: Arc<CpalStreamDiagnostics>,
  trace: TraceHandle,
) -> Result<cpal::Stream, AudioError>
where
  T: OutputSample + AudioSample + cpal::SizedSample,
{
  use cpal::traits::DeviceTrait;

  let channels = mixer.channels_usize();
  let mut mix_buf: Vec<f32> =
    vec![0.0; preallocate_mix_buffer_len(config, channels, fixed_callback_frames)];
  let mut playback_origin = None;
  let mut playback_origin_offset = Duration::ZERO;
  let sample_rate_hz = mixer.config.sample_rate_hz;
  let trace_enabled = trace.is_enabled();

  let err_cb = {
    let diagnostics = diagnostics.clone();
    let errors = errors.clone();
    move |_err| {
      // Avoid printing from the (likely RT) error callback. Record the event and let non-RT code
      // print at most once.
      //
      // This callback is invoked from CPAL/host code; ensure no panics can unwind across the
      // callback boundary.
      if catch_unwind(AssertUnwindSafe(|| {
        diagnostics.set_stream_error(1);
        errors.record();
      }))
      .is_err()
      {
        diagnostics.set_panic_in_callback();
        errors.record();
      }
    }
  };

  let stream = device
    .build_output_stream(
      config,
      move |output: &mut [T], info| {
        let frames = if channels == 0 { 0u64 } else { (output.len() / channels) as u64 };
        let mut clock_updated = false;

        let did_panic = guard_output_callback(output, &diagnostics.panic_in_callback, |output| {
          super::thread_priority::promote_current_thread_for_audio();

          // If we've already panicked once, keep outputting silence to avoid repeated unwinds from
          // unpredictable RT callback state. Still advance the clock/counters so A/V sync doesn't
          // stall completely.
          if diagnostics.panic_in_callback.load(Ordering::Relaxed) {
            output.fill(T::SILENCE);
            mixer.drain_all_sinks(output.len());
            if channels != 0 {
              let frames_u32 = u32::try_from(frames).unwrap_or(u32::MAX);
              last_callback_frames.store(frames_u32, Ordering::Relaxed);
              clock.on_callback_end_at(Instant::now(), frames_u32, None);
              clock_updated = true;
              if fixed_callback_frames.is_none() {
                let latency = frames_to_duration(sample_rate_hz, frames);
                estimated_latency_nanos.store(duration_to_nanos_u64(latency), Ordering::Relaxed);
              }
            }
            return;
          }

          let mut callback_span = if trace_enabled {
            let mut span = trace.span("audio.callback", "audio");
            span.arg_u64("frames", frames);
            Some(span)
          } else {
            None
          };

          if let Some(span) = callback_span.as_mut() {
            let stats = mixer.stats_for_output_len(output.len());
            span.arg_u64("streams", stats.streams);
            span.arg_u64("underruns", stats.underruns);
            span.arg_u64("buffered_frames", stats.buffered_frames);
          }

          let ts = info.timestamp();
          let latency = ts.playback.duration_since(&ts.callback);

          match mixer.mix_for_callback(output.len()) {
            MixerCallbackAction::Mix => {
              // CPAL can (rarely) provide variable callback buffer sizes. Never resize or allocate
              // in the callback; instead, process in bounded chunks using the preallocated mix
              // buffer.
              let mix_span = if trace_enabled {
                Some(trace.span("audio.mix", "audio"))
              } else {
                None
              };

              for out_chunk in output.chunks_mut(mix_buf.len()) {
                let mix = &mut mix_buf[..out_chunk.len()];
                mix.fill(0.0);
                mixer.mix_into(mix);
                for (out, sample) in out_chunk.iter_mut().zip(mix.iter()) {
                  *out = T::from_mixed_f32(*sample);
                }
              }
              drop(mix_span);
            }
            MixerCallbackAction::Silence | MixerCallbackAction::SilenceAndDrain => {
              // Fill output with silence without doing any per-sample mixing work.
              output.fill(T::SILENCE);
            }
          }

          if channels != 0 {
            let frames_u32 = u32::try_from(frames).unwrap_or(u32::MAX);
            last_callback_frames.store(frames_u32, Ordering::Relaxed);
            let callback_end = Instant::now();

            // Prefer CPAL's device timestamps (when monotonic) as the base time, falling back to a
            // pure frame counter when unavailable.
            let device_time_at_end = {
              let playback = ts.playback;
              let frame_counter_time = frames_to_duration(sample_rate_hz, clock.frames_written());
              let buffer_duration = frames_to_duration(sample_rate_hz, frames);

              match playback_origin.as_ref() {
                Some(origin) => match playback.duration_since(origin) {
                  Some(since_origin) => {
                    // If playback timestamps jitter backwards, the derived device timeline could go
                    // backwards and stall the interpolated clock (which is monotonic by design).
                    // Clamp so we never lag behind the frame counter.
                    let frames_end = frame_counter_time.saturating_add(buffer_duration);
                    let device_end = playback_origin_offset
                      .saturating_add(since_origin)
                      .saturating_add(buffer_duration);
                    Some(device_end.max(frames_end))
                  }
                  None => {
                    // Playback timestamps went backwards (device restart/glitch). Re-anchor and
                    // align to the frame counter so timestamp-based clocking can resume without a
                    // discontinuity.
                    playback_origin = Some(playback);
                    playback_origin_offset = frame_counter_time;
                    Some(frame_counter_time.saturating_add(buffer_duration))
                  }
                },
                None => {
                  playback_origin = Some(playback);
                  playback_origin_offset = frame_counter_time;
                  Some(frame_counter_time.saturating_add(buffer_duration))
                }
              }
            };

            clock.on_callback_end_at(callback_end, frames_u32, device_time_at_end);
            clock_updated = true;

            // Best-effort latency estimate:
            // - prefer CPAL timestamps when available (callback vs playback instant),
            // - otherwise fall back to observed callback buffer size (only when buffer size isn't fixed).
            if let Some(latency) = latency {
              estimated_latency_nanos.store(duration_to_nanos_u64(latency), Ordering::Relaxed);
            } else if fixed_callback_frames.is_none() {
              let latency = frames_to_duration(sample_rate_hz, frames);
              estimated_latency_nanos.store(duration_to_nanos_u64(latency), Ordering::Relaxed);
            }
          }
        });

        if did_panic && !clock_updated && channels != 0 {
          // This is part of the RT output callback; never let panics unwind across the boundary,
          // even in this best-effort post-panic clock update.
          let _ = catch_unwind(AssertUnwindSafe(|| {
            let frames_u32 = u32::try_from(frames).unwrap_or(u32::MAX);
            last_callback_frames.store(frames_u32, Ordering::Relaxed);
            clock.on_callback_end_at(Instant::now(), frames_u32, None);

            if fixed_callback_frames.is_none() {
              let latency = frames_to_duration(sample_rate_hz, frames);
              estimated_latency_nanos.store(duration_to_nanos_u64(latency), Ordering::Relaxed);
            }
          }));
        }
      },
      err_cb,
      None,
    )
    .map_err(|err| AudioError::StreamBuildFailed {
      device_name: device_name.to_string(),
      config: stream_config,
      sample_format,
      source: Box::new(err),
    })?;

  Ok(stream)
}

fn preallocate_mix_buffer_len(
  config: &cpal::StreamConfig,
  channels: usize,
  fixed_callback_frames: Option<u32>,
) -> usize {
  // When the host requests `BufferSize::Default`, the actual callback size is backend-dependent and
  // may vary. Choose a conservative upper bound to ensure we never allocate in the RT callback.
  //
  // The callback still handles larger slices by processing in chunks of this size (no allocations).
  const DEFAULT_MAX_FRAMES: usize = 8192;
  const MAX_FRAMES_CAP: usize = 32_768;

  let frames = fixed_callback_frames
    .map(|frames| frames as usize)
    .unwrap_or_else(|| match config.buffer_size {
      cpal::BufferSize::Fixed(frames) => frames as usize,
      cpal::BufferSize::Default => DEFAULT_MAX_FRAMES,
    });

  let frames = frames.clamp(1, MAX_FRAMES_CAP);
  frames.saturating_mul(channels.max(1)).max(1)
}
fn duration_to_nanos_u64(duration: Duration) -> u64 {
  u64::try_from(duration.as_nanos()).unwrap_or(u64::MAX)
}

fn f32_to_i16(value: f32) -> i16 {
  let value = sanitize_sample(value);
  (value * i16::MAX as f32) as i16
}

fn f32_to_u16(value: f32) -> u16 {
  let value = sanitize_sample(value);
  let shifted = value * 0.5 + 0.5;
  // Round so that `0.0` maps to the unsigned PCM equilibrium value (`0x8000`).
  (shifted * u16::MAX as f32 + 0.5) as u16
}

trait OutputSample: cpal::Sample + cpal::SizedSample {
  fn from_mixed_f32(value: f32) -> Self;
}

impl OutputSample for f32 {
  fn from_mixed_f32(value: f32) -> Self {
    sanitize_sample(value)
  }
}

impl OutputSample for i16 {
  fn from_mixed_f32(value: f32) -> Self {
    f32_to_i16(value)
  }
}

impl OutputSample for u16 {
  fn from_mixed_f32(value: f32) -> Self {
    f32_to_u16(value)
  }
}

#[cfg(test)]
mod tests {
  use std::sync::atomic::Ordering;

  use super::{f32_to_i16, f32_to_u16, CpalStreamDiagnostics};
  use crate::media::audio::convert::sanitize_sample;

  #[test]
  fn sanitize_handles_nan_and_clamps() {
    assert_eq!(sanitize_sample(f32::NAN), 0.0);
    assert_eq!(sanitize_sample(2.0), 1.0);
    assert_eq!(sanitize_sample(-2.0), -1.0);
  }

  #[test]
  fn converts_f32_to_i16() {
    assert_eq!(f32_to_i16(0.0), 0);
    assert_eq!(f32_to_i16(1.0), i16::MAX);
    assert_eq!(f32_to_i16(-1.0), -i16::MAX);
  }

  #[test]
  fn converts_f32_to_u16() {
    assert_eq!(f32_to_u16(0.0), 1 << 15);
    assert_eq!(f32_to_u16(1.0), u16::MAX);
    assert_eq!(f32_to_u16(-1.0), 0);
  }

  #[test]
  fn cpal_backend_panic_flag_starts_false() {
    let diag = CpalStreamDiagnostics::new();
    assert!(!diag.panic_in_callback.load(Ordering::Relaxed));
  }
}
