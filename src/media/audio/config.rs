use parking_lot::ReentrantMutex;
use parking_lot::ReentrantMutexGuard;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::RwLock;
use std::time::Duration;

use super::limits::{MAX_BUFFERED_DURATION, MAX_CHANNELS, MAX_SAMPLE_RATE_HZ};

/// Centralized configuration for the audio output/mixing pipeline.
///
/// This struct is intentionally small and copyable so it can be:
/// - constructed from environment variables for quick tuning, and
/// - overridden deterministically in tests without mutating process env.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioEngineConfig {
  /// Maximum amount of audio a single stream is allowed to buffer locally.
  ///
  /// For the CPAL backend, this controls the per-sink ring-buffer capacity.
  pub per_stream_max_buffered_duration: Duration,

  /// Hard cap on the number of simultaneously active audio streams.
  pub global_max_streams: usize,

  /// Hard cap on total buffered audio across all streams, in bytes.
  ///
  /// This is intended to bound memory usage for untrusted pages that keep producing audio.
  pub global_buffer_budget_bytes: usize,

  /// Default sample rate used by non-device backends (null/WAV).
  pub default_sample_rate_hz: u32,

  /// Default channel count used by non-device backends (null/WAV).
  pub default_channels: u16,

  /// Amount of buffered audio required before a stream transitions from "starting" to "playing".
  pub preroll_buffer_threshold: Duration,

  /// Threshold below which a stream is considered to be "low on buffered audio".
  pub low_buffer_threshold: Duration,

  /// Debounce window for low-buffer notifications (prevents flapping).
  pub low_buffer_debounce: Duration,

  /// When all streams have been idle for at least this duration, the engine may stop/pause output.
  pub idle_timeout: Duration,
}

impl Default for AudioEngineConfig {
  fn default() -> Self {
    Self {
      // Keep defaults aligned with the pre-config audio backend behaviour (~2 seconds of audio).
      per_stream_max_buffered_duration: Duration::from_secs(2),
      // Conservative defaults; typical pages should have 1-2 concurrent streams (video + maybe UI
      // sounds).
      global_max_streams: 32,
      // Default budget large enough for 32 stereo streams at 48kHz with ~2 seconds of buffering:
      // 48_000 frames/s * 2 channels * 2 seconds * 4 bytes/sample ≈ 768 KiB/stream.
      global_buffer_budget_bytes: 32 * 1024 * 1024,
      default_sample_rate_hz: 48_000,
      default_channels: 2,
      preroll_buffer_threshold: Duration::from_millis(150),
      low_buffer_threshold: Duration::from_millis(80),
      low_buffer_debounce: Duration::from_millis(250),
      idle_timeout: Duration::from_secs(3),
    }
  }
}

impl AudioEngineConfig {
  pub const ENV_PER_STREAM_MAX_BUFFERED_MS: &'static str = "FASTR_AUDIO_STREAM_MAX_BUFFER_MS";
  pub const ENV_GLOBAL_MAX_STREAMS: &'static str = "FASTR_AUDIO_MAX_STREAMS";
  pub const ENV_GLOBAL_BUFFER_BUDGET: &'static str = "FASTR_AUDIO_BUFFER_BUDGET";
  pub const ENV_DEFAULT_SAMPLE_RATE_HZ: &'static str = "FASTR_AUDIO_DEFAULT_SAMPLE_RATE_HZ";
  pub const ENV_DEFAULT_CHANNELS: &'static str = "FASTR_AUDIO_DEFAULT_CHANNELS";
  pub const ENV_PREROLL_THRESHOLD_MS: &'static str = "FASTR_AUDIO_PREROLL_MS";
  pub const ENV_LOW_BUFFER_THRESHOLD_MS: &'static str = "FASTR_AUDIO_LOW_BUFFER_MS";
  pub const ENV_LOW_BUFFER_DEBOUNCE_MS: &'static str = "FASTR_AUDIO_LOW_BUFFER_DEBOUNCE_MS";
  pub const ENV_IDLE_TIMEOUT_MS: &'static str = "FASTR_AUDIO_IDLE_TIMEOUT_MS";

  /// Parse audio configuration from a provided environment-variable map.
  ///
  /// Any parse failure falls back to the default value and emits a warning.
  #[must_use]
  pub fn from_env_map(raw: &HashMap<String, String>) -> Self {
    let mut cfg = Self::default();

    cfg.per_stream_max_buffered_duration = parse_duration_ms_positive(
      raw.get(Self::ENV_PER_STREAM_MAX_BUFFERED_MS),
      Self::ENV_PER_STREAM_MAX_BUFFERED_MS,
      cfg.per_stream_max_buffered_duration,
    );
    cfg.global_max_streams = parse_usize_positive(
      raw.get(Self::ENV_GLOBAL_MAX_STREAMS),
      Self::ENV_GLOBAL_MAX_STREAMS,
      cfg.global_max_streams,
    );
    cfg.global_buffer_budget_bytes = parse_bytes_positive(
      raw.get(Self::ENV_GLOBAL_BUFFER_BUDGET),
      Self::ENV_GLOBAL_BUFFER_BUDGET,
      cfg.global_buffer_budget_bytes,
    );
    cfg.default_sample_rate_hz = parse_u32_positive(
      raw.get(Self::ENV_DEFAULT_SAMPLE_RATE_HZ),
      Self::ENV_DEFAULT_SAMPLE_RATE_HZ,
      cfg.default_sample_rate_hz,
    );
    cfg.default_channels = parse_u16_positive(
      raw.get(Self::ENV_DEFAULT_CHANNELS),
      Self::ENV_DEFAULT_CHANNELS,
      cfg.default_channels,
    );
    cfg.preroll_buffer_threshold = parse_duration_ms_non_negative(
      raw.get(Self::ENV_PREROLL_THRESHOLD_MS),
      Self::ENV_PREROLL_THRESHOLD_MS,
      cfg.preroll_buffer_threshold,
    );
    cfg.low_buffer_threshold = parse_duration_ms_non_negative(
      raw.get(Self::ENV_LOW_BUFFER_THRESHOLD_MS),
      Self::ENV_LOW_BUFFER_THRESHOLD_MS,
      cfg.low_buffer_threshold,
    );
    cfg.low_buffer_debounce = parse_duration_ms_non_negative(
      raw.get(Self::ENV_LOW_BUFFER_DEBOUNCE_MS),
      Self::ENV_LOW_BUFFER_DEBOUNCE_MS,
      cfg.low_buffer_debounce,
    );
    cfg.idle_timeout = parse_duration_ms_non_negative(
      raw.get(Self::ENV_IDLE_TIMEOUT_MS),
      Self::ENV_IDLE_TIMEOUT_MS,
      cfg.idle_timeout,
    );

    // Clamp configuration sourced from env vars (untrusted input surface) so the audio backend
    // cannot allocate unbounded memory even if the user/environment is hostile.
    cfg.per_stream_max_buffered_duration = cfg.per_stream_max_buffered_duration.min(MAX_BUFFERED_DURATION);
    cfg.default_sample_rate_hz = cfg
      .default_sample_rate_hz
      .clamp(1, MAX_SAMPLE_RATE_HZ);
    cfg.default_channels = cfg.default_channels.clamp(1, MAX_CHANNELS);

    cfg
  }

  /// Parse audio configuration directly from the process environment.
  ///
  /// Prefer [`Self::from_env_map`] when you already have an env snapshot (e.g. tests).
  #[must_use]
  pub fn from_env() -> Self {
    let raw = std::env::vars()
      .filter(|(k, _)| k.starts_with("FASTR_AUDIO_"))
      .collect::<HashMap<_, _>>();
    Self::from_env_map(&raw)
  }
}

static DEFAULT_CONFIG: OnceLock<Arc<AudioEngineConfig>> = OnceLock::new();
static ACTIVE_CONFIG: OnceLock<RwLock<Arc<AudioEngineConfig>>> = OnceLock::new();
static ACTIVE_CONFIG_OVERRIDE_LOCK: OnceLock<ReentrantMutex<()>> = OnceLock::new();

fn default_config() -> Arc<AudioEngineConfig> {
  DEFAULT_CONFIG
    .get_or_init(|| Arc::new(AudioEngineConfig::from_env()))
    .clone()
}

/// Return the currently active audio-engine configuration.
///
/// Defaults to [`AudioEngineConfig::from_env`], but can be overridden via
/// [`set_audio_engine_config`] / [`with_audio_engine_config`].
pub fn audio_engine_config() -> Arc<AudioEngineConfig> {
  let lock = ACTIVE_CONFIG.get_or_init(|| RwLock::new(default_config()));
  lock
    .read()
    .unwrap_or_else(|poisoned| poisoned.into_inner())
    .clone()
}

/// Guard that restores the previous audio configuration when dropped.
pub struct AudioEngineConfigGuard {
  previous: Arc<AudioEngineConfig>,
  #[allow(dead_code)]
  _override_lock: ReentrantMutexGuard<'static, ()>,
}

impl Drop for AudioEngineConfigGuard {
  fn drop(&mut self) {
    if let Some(lock) = ACTIVE_CONFIG.get() {
      let mut guard = lock
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
      *guard = self.previous.clone();
    }
  }
}

/// Install the provided config as the active audio-engine config for the duration of the returned
/// guard.
///
/// This is intended for tests that need deterministic configuration without mutating the process
/// environment.
pub fn set_audio_engine_config(config: Arc<AudioEngineConfig>) -> AudioEngineConfigGuard {
  let override_lock = ACTIVE_CONFIG_OVERRIDE_LOCK
    .get_or_init(|| ReentrantMutex::new(()))
    .lock();
  let lock = ACTIVE_CONFIG.get_or_init(|| RwLock::new(default_config()));
  let mut guard = lock
    .write()
    .unwrap_or_else(|poisoned| poisoned.into_inner());
  let previous = guard.clone();
  *guard = config;
  AudioEngineConfigGuard {
    previous,
    _override_lock: override_lock,
  }
}

/// Convenience helper to run a closure with a temporary audio-engine config override.
pub fn with_audio_engine_config<T>(config: Arc<AudioEngineConfig>, f: impl FnOnce() -> T) -> T {
  let guard = set_audio_engine_config(config);
  let result = f();
  drop(guard);
  result
}

fn warn_parse(name: &str, raw: &str, fallback: &str) {
  eprintln!(
    "warning: invalid {name}={raw:?}; falling back to default ({fallback})"
  );
}

fn clean_numeric(raw: &str) -> String {
  raw.trim().chars().filter(|ch| *ch != '_').collect()
}

fn parse_u64(raw: &str) -> Option<u64> {
  let cleaned = clean_numeric(raw);
  if cleaned.is_empty() {
    return None;
  }
  cleaned.parse::<u64>().ok()
}

fn parse_usize_positive(value: Option<&String>, name: &str, default: usize) -> usize {
  let Some(value) = value else { return default };
  let Some(parsed) = parse_u64(value) else {
    warn_parse(name, value, &default.to_string());
    return default;
  };
  let Some(parsed) = usize::try_from(parsed).ok() else {
    warn_parse(name, value, &default.to_string());
    return default;
  };
  if parsed == 0 {
    warn_parse(name, value, &default.to_string());
    return default;
  }
  parsed
}

fn parse_u32_positive(value: Option<&String>, name: &str, default: u32) -> u32 {
  let Some(value) = value else { return default };
  let Some(parsed) = parse_u64(value) else {
    warn_parse(name, value, &default.to_string());
    return default;
  };
  let Some(parsed) = u32::try_from(parsed).ok() else {
    warn_parse(name, value, &default.to_string());
    return default;
  };
  if parsed == 0 {
    warn_parse(name, value, &default.to_string());
    return default;
  }
  parsed
}

fn parse_u16_positive(value: Option<&String>, name: &str, default: u16) -> u16 {
  let Some(value) = value else { return default };
  let Some(parsed) = parse_u64(value) else {
    warn_parse(name, value, &default.to_string());
    return default;
  };
  let Some(parsed) = u16::try_from(parsed).ok() else {
    warn_parse(name, value, &default.to_string());
    return default;
  };
  if parsed == 0 {
    warn_parse(name, value, &default.to_string());
    return default;
  }
  parsed
}

fn parse_duration_ms_positive(value: Option<&String>, name: &str, default: Duration) -> Duration {
  let Some(value) = value else { return default };
  let Some(ms) = parse_u64(value) else {
    warn_parse(name, value, &format!("{}ms", default.as_millis()));
    return default;
  };
  if ms == 0 {
    warn_parse(name, value, &format!("{}ms", default.as_millis()));
    return default;
  }
  Duration::from_millis(ms)
}

fn parse_duration_ms_non_negative(value: Option<&String>, name: &str, default: Duration) -> Duration {
  let Some(value) = value else { return default };
  let Some(ms) = parse_u64(value) else {
    warn_parse(name, value, &format!("{}ms", default.as_millis()));
    return default;
  };
  Duration::from_millis(ms)
}

fn parse_bytes_positive(value: Option<&String>, name: &str, default: usize) -> usize {
  let Some(value) = value else { return default };
  let trimmed = value.trim();
  if trimmed.is_empty() {
    warn_parse(name, value, &default.to_string());
    return default;
  }
  let Some(bytes) = parse_byte_size(trimmed) else {
    warn_parse(name, value, &default.to_string());
    return default;
  };
  if bytes == 0 {
    warn_parse(name, value, &default.to_string());
    return default;
  }
  bytes
}

fn parse_byte_size(raw: &str) -> Option<usize> {
  let s = raw.trim().to_ascii_lowercase();
  let unit_start = s
    .find(|c: char| c.is_ascii_alphabetic())
    .unwrap_or_else(|| s.len());
  let (num, unit) = s.split_at(unit_start);
  let cleaned = clean_numeric(num);
  let value: u64 = cleaned.parse().ok()?;
  let factor: u64 = match unit {
    "" | "b" => 1,
    "k" | "kb" | "kib" => 1024,
    "m" | "mb" | "mib" => 1024 * 1024,
    "g" | "gb" | "gib" => 1024 * 1024 * 1024,
    _ => return None,
  };
  let bytes = value.checked_mul(factor)?;
  usize::try_from(bytes).ok()
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn env_parsing_invalid_numbers_fall_back_to_defaults() {
    let defaults = AudioEngineConfig::default();
    let cfg = AudioEngineConfig::from_env_map(&HashMap::from([
      (
        AudioEngineConfig::ENV_PER_STREAM_MAX_BUFFERED_MS.to_string(),
        "not-a-number".to_string(),
      ),
      (
        AudioEngineConfig::ENV_GLOBAL_MAX_STREAMS.to_string(),
        "oops".to_string(),
      ),
      (
        AudioEngineConfig::ENV_GLOBAL_BUFFER_BUDGET.to_string(),
        "???".to_string(),
      ),
      (
        AudioEngineConfig::ENV_DEFAULT_SAMPLE_RATE_HZ.to_string(),
        "NaN".to_string(),
      ),
      (
        AudioEngineConfig::ENV_DEFAULT_CHANNELS.to_string(),
        "two".to_string(),
      ),
    ]));

    assert_eq!(cfg.per_stream_max_buffered_duration, defaults.per_stream_max_buffered_duration);
    assert_eq!(cfg.global_max_streams, defaults.global_max_streams);
    assert_eq!(cfg.global_buffer_budget_bytes, defaults.global_buffer_budget_bytes);
    assert_eq!(cfg.default_sample_rate_hz, defaults.default_sample_rate_hz);
    assert_eq!(cfg.default_channels, defaults.default_channels);
  }

  #[test]
  fn env_parsing_overflow_and_negative_values_fall_back_to_defaults() {
    let defaults = AudioEngineConfig::default();
    let cfg = AudioEngineConfig::from_env_map(&HashMap::from([
      (
        AudioEngineConfig::ENV_GLOBAL_MAX_STREAMS.to_string(),
        u128::MAX.to_string(),
      ),
      (
        AudioEngineConfig::ENV_DEFAULT_SAMPLE_RATE_HZ.to_string(),
        "9999999999999999999999999".to_string(),
      ),
      (
        AudioEngineConfig::ENV_DEFAULT_CHANNELS.to_string(),
        "-2".to_string(),
      ),
      (
        AudioEngineConfig::ENV_GLOBAL_BUFFER_BUDGET.to_string(),
        "-1".to_string(),
      ),
    ]));

    assert_eq!(cfg.global_max_streams, defaults.global_max_streams);
    assert_eq!(cfg.default_sample_rate_hz, defaults.default_sample_rate_hz);
    assert_eq!(cfg.default_channels, defaults.default_channels);
    assert_eq!(cfg.global_buffer_budget_bytes, defaults.global_buffer_budget_bytes);
  }

  #[test]
  fn env_parsing_accepts_byte_suffixes_and_underscores() {
    let cfg = AudioEngineConfig::from_env_map(&HashMap::from([(
      AudioEngineConfig::ENV_GLOBAL_BUFFER_BUDGET.to_string(),
      "64_mb".to_string(),
    )]));

    assert_eq!(cfg.global_buffer_budget_bytes, 64 * 1024 * 1024);
  }

  #[test]
  fn env_parsing_clamps_untrusted_limits() {
    let cfg = AudioEngineConfig::from_env_map(&HashMap::from([
      (
        AudioEngineConfig::ENV_PER_STREAM_MAX_BUFFERED_MS.to_string(),
        "60000".to_string(), // 60s (above MAX_BUFFERED_DURATION)
      ),
      (
        AudioEngineConfig::ENV_DEFAULT_SAMPLE_RATE_HZ.to_string(),
        (u64::from(MAX_SAMPLE_RATE_HZ) + 1).to_string(),
      ),
      (
        AudioEngineConfig::ENV_DEFAULT_CHANNELS.to_string(),
        (u32::from(MAX_CHANNELS) + 1).to_string(),
      ),
    ]));

    assert_eq!(cfg.per_stream_max_buffered_duration, MAX_BUFFERED_DURATION);
    assert_eq!(cfg.default_sample_rate_hz, MAX_SAMPLE_RATE_HZ);
    assert_eq!(cfg.default_channels, MAX_CHANNELS);
  }

  #[test]
  fn env_parsing_reads_duration_thresholds() {
    let cfg = AudioEngineConfig::from_env_map(&HashMap::from([
      (
        AudioEngineConfig::ENV_IDLE_TIMEOUT_MS.to_string(),
        "1500".to_string(),
      ),
      (
        AudioEngineConfig::ENV_PREROLL_THRESHOLD_MS.to_string(),
        "123".to_string(),
      ),
      (
        AudioEngineConfig::ENV_LOW_BUFFER_THRESHOLD_MS.to_string(),
        "45".to_string(),
      ),
      (
        AudioEngineConfig::ENV_LOW_BUFFER_DEBOUNCE_MS.to_string(),
        "6_000".to_string(),
      ),
    ]));

    assert_eq!(cfg.idle_timeout, Duration::from_millis(1500));
    assert_eq!(cfg.preroll_buffer_threshold, Duration::from_millis(123));
    assert_eq!(cfg.low_buffer_threshold, Duration::from_millis(45));
    assert_eq!(cfg.low_buffer_debounce, Duration::from_millis(6000));
  }

  #[test]
  fn env_parsing_invalid_thresholds_fall_back_to_defaults() {
    let defaults = AudioEngineConfig::default();
    let cfg = AudioEngineConfig::from_env_map(&HashMap::from([
      (
        AudioEngineConfig::ENV_IDLE_TIMEOUT_MS.to_string(),
        "-1".to_string(),
      ),
      (
        AudioEngineConfig::ENV_PREROLL_THRESHOLD_MS.to_string(),
        "not-a-number".to_string(),
      ),
      (
        AudioEngineConfig::ENV_LOW_BUFFER_THRESHOLD_MS.to_string(),
        "-5".to_string(),
      ),
      (
        AudioEngineConfig::ENV_LOW_BUFFER_DEBOUNCE_MS.to_string(),
        "oops".to_string(),
      ),
    ]));

    assert_eq!(cfg.idle_timeout, defaults.idle_timeout);
    assert_eq!(cfg.preroll_buffer_threshold, defaults.preroll_buffer_threshold);
    assert_eq!(cfg.low_buffer_threshold, defaults.low_buffer_threshold);
    assert_eq!(cfg.low_buffer_debounce, defaults.low_buffer_debounce);
  }
}
