use diagnostics::Diagnostic;
use serde::Serialize;
use std::time::Duration;

pub const JSON_SCHEMA_VERSION: u32 = 1;

pub fn duration_ms(duration: Duration) -> f64 {
  // `Duration::as_secs_f64` is monotonic and portable.
  // Round to microsecond precision to keep output reasonably stable/readable.
  let ms = duration.as_secs_f64() * 1000.0;
  (ms * 1000.0).round() / 1000.0
}

#[derive(Debug, Clone, Copy, Serialize)]
pub struct BenchStats {
  pub mean_ms: f64,
  pub median_ms: f64,
  pub min_ms: f64,
  pub max_ms: f64,
}

pub fn stats(times_ms: &[f64]) -> BenchStats {
  if times_ms.is_empty() {
    return BenchStats {
      mean_ms: 0.0,
      median_ms: 0.0,
      min_ms: 0.0,
      max_ms: 0.0,
    };
  }

  let mut sorted = times_ms.to_vec();
  sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));

  let sum: f64 = sorted.iter().sum();
  let len = sorted.len() as f64;
  let mean_ms = sum / len;

  let median_ms = if sorted.len() % 2 == 1 {
    sorted[sorted.len() / 2]
  } else {
    let hi = sorted.len() / 2;
    let lo = hi - 1;
    (sorted[lo] + sorted[hi]) / 2.0
  };

  BenchStats {
    mean_ms,
    median_ms,
    min_ms: *sorted.first().unwrap_or(&0.0),
    max_ms: *sorted.last().unwrap_or(&0.0),
  }
}

#[derive(Debug, Clone, Serialize)]
pub struct BenchJsonOutput {
  pub schema_version: u32,
  pub command: &'static str,

  /// Diagnostics produced while compiling (or loading) the benchmark.
  ///
  /// This is always emitted (possibly empty) so consumers can depend on a stable output shape.
  pub diagnostics: Vec<Diagnostic>,

  /// Non-diagnostic error message for failures that don't naturally map to diagnostics (e.g.
  /// spawning the benchmark process).
  pub error: Option<String>,

  pub entry: String,
  pub args: Vec<String>,

  pub warmup: u32,
  pub iters: u32,
  pub timeout_ms: u64,

  pub compile_time_ms: f64,

  pub run_times_ms: Vec<f64>,
  pub run_exit_codes: Vec<i32>,

  #[serde(flatten)]
  pub stats: BenchStats,

  /// If the bench command completed, this matches the command exit status.
  /// (On errors, this is a non-zero best-effort value.)
  pub exit_code: u8,
}
