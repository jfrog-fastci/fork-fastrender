use serde::{Deserialize, Serialize};

/// Structured perf-log events emitted by the windowed `browser` when `FASTR_PERF_LOG` is enabled.
///
/// The log format is newline-delimited JSON (JSONL) where every line is a single serialized event.
///
/// Over time we have had two schemas:
/// - **V1 (legacy)**: `{ "type": "ui_frame_time", ... }`
/// - **V2 (current)**: `{ "event": "frame", "schema_version": 2, ... }` (see `src/bin/browser.rs`)
///
/// This enum is intentionally **loose and forward compatible**:
/// - Unknown event kinds deserialize as `Unknown*`.
/// - Unknown fields are ignored.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum BrowserPerfLogEvent {
  V2(BrowserPerfLogEventV2),
  V1(BrowserPerfLogEventV1),
  /// Catch-all for forward compatibility (valid JSON that doesn't match known schemas).
  Unknown(serde_json::Value),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InputKind {
  Keyboard,
  MouseWheel,
  PointerMove,
  Button,
  #[serde(other)]
  Unknown,
}

/// Current `browser` perf-log schema (`event=...`).
///
/// The variants include only the fields needed by aggregation tools; unknown fields are ignored.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum BrowserPerfLogEventV2 {
  Frame {
    #[serde(default)]
    ui_frame_ms: Option<f64>,
    #[serde(default)]
    fps: Option<f64>,
  },
  Input {
    #[serde(default)]
    input_kind: Option<InputKind>,
    #[serde(default)]
    input_to_present_ms: Option<f64>,
  },
  Resize {
    #[serde(default)]
    resize_to_present_ms: Option<f64>,
  },
  Ttfp {
    #[serde(default)]
    ttfp_ms: Option<f64>,
  },
  CpuSummary {
    #[serde(default)]
    cpu_percent_recent: Option<f64>,
  },
  IdleSample {
    #[serde(default)]
    idle_fps: Option<f32>,
  },
  FrameUpload {
    #[serde(default)]
    upload_last_ms: Option<f64>,
    #[serde(default)]
    upload_total_ms: Option<f64>,
    #[serde(default)]
    overwritten_frames: Option<u64>,
    #[serde(default)]
    uploads: Option<u32>,
    #[serde(default)]
    uploaded_bytes: Option<u64>,
  },
  #[serde(other)]
  Unknown,
}

/// Legacy perf-log schema (`type=...`) kept for backwards compatibility with older captures/tests.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum BrowserPerfLogEventV1 {
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
