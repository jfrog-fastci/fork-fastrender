use serde::{Deserialize, Serialize};

/// Structured perf-log events emitted by the windowed `browser` when `FASTR_PERF_LOG` is enabled.
///
/// The log format is newline-delimited JSON (JSONL) where every line is a single serialized
/// [`BrowserPerfLogEvent`]. The schema is intentionally loose:
/// - Unknown event types deserialize as [`BrowserPerfLogEvent::Unknown`] (forward compatible).
/// - Unknown fields are ignored (forward compatible).
///
/// This module is shared by the producer (`browser`) and consumers (e.g.
/// `browser_perf_log_summary`) so that perf-log captures remain actionable without one-off scripts.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum BrowserPerfLogEvent {
  /// Emitted once per UI frame (egui/winit redraw).
  #[serde(alias = "ui_frame")]
  UiFrameTime {
    /// Time spent producing a UI frame (ms).
    #[serde(alias = "dt_ms", alias = "ui_frame_time_ms", alias = "frame_time")]
    frame_time_ms: f64,

    /// Optional monotonic timestamp (ms) for ordering across processes/threads.
    #[serde(default, alias = "ts", alias = "timestamp_ms")]
    ts_ms: Option<f64>,
  },

  /// Time to first paint for a navigation (ms).
  #[serde(alias = "ttfp")]
  TimeToFirstPaint {
    #[serde(alias = "ms", alias = "ttfp")]
    ttfp_ms: f64,

    #[serde(default, alias = "ts", alias = "timestamp_ms")]
    ts_ms: Option<f64>,
  },

  /// Generic latency measurement (ms) tagged by kind.
  ///
  /// Known kinds that the summary tool understands:
  /// - `scroll`
  /// - `resize`
  /// - `input`
  /// - `tab_switch`
  Latency {
    kind: String,

    #[serde(alias = "ms", alias = "dt_ms")]
    latency_ms: f64,

    #[serde(default, alias = "ts", alias = "timestamp_ms")]
    ts_ms: Option<f64>,
  },

  /// A periodic resource-usage sample of the browser process (or the UI process).
  #[serde(alias = "resource")]
  ResourceSample {
    /// Process CPU utilization over the sampling window (0-100).
    #[serde(default, alias = "cpu", alias = "cpu_pct", alias = "cpu_percent")]
    cpu_percent: Option<f64>,

    /// Resident set size in bytes.
    #[serde(default, alias = "rss", alias = "rss_b")]
    rss_bytes: Option<u64>,

    #[serde(default, alias = "ts", alias = "timestamp_ms")]
    ts_ms: Option<f64>,
  },

  /// Catch-all for forward-compatible parsing. Unknown events are ignored by aggregation tools.
  #[serde(other)]
  Unknown,
}
