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
  /// Legacy value used by older schema versions/tests.
  #[serde(rename = "mouse")]
  Mouse,
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
    t_ms: Option<u64>,
    #[serde(default)]
    ts_ms: Option<u64>,
    #[serde(default)]
    ui_frame_ms: Option<f64>,
    #[serde(default)]
    fps: Option<f64>,
  },
  Input {
    #[serde(default)]
    t_ms: Option<u64>,
    #[serde(default)]
    ts_ms: Option<u64>,
    #[serde(default, alias = "kind")]
    input_kind: Option<InputKind>,
    #[serde(default)]
    input_to_present_ms: Option<f64>,
  },
  TabSwitch {
    #[serde(default)]
    t_ms: Option<u64>,
    #[serde(default)]
    ts_ms: Option<u64>,
    #[serde(default)]
    latency_ms: Option<u64>,
  },
  Resize {
    #[serde(default)]
    t_ms: Option<u64>,
    #[serde(default)]
    ts_ms: Option<u64>,
    #[serde(default)]
    resize_to_present_ms: Option<f64>,
  },
  Ttfp {
    #[serde(default)]
    t_ms: Option<u64>,
    #[serde(default)]
    ts_ms: Option<u64>,
    #[serde(default)]
    ttfp_ms: Option<f64>,
  },
  CpuSummary {
    #[serde(default)]
    t_ms: Option<u64>,
    #[serde(default)]
    ts_ms: Option<u64>,
    #[serde(default)]
    cpu_percent_recent: Option<f64>,
  },
  #[serde(rename = "idle_summary", alias = "idle_sample")]
  IdleSample {
    #[serde(default)]
    t_ms: Option<u64>,
    #[serde(default)]
    ts_ms: Option<u64>,
    #[serde(default, rename = "idle_frames_per_sec", alias = "idle_fps")]
    idle_fps: Option<f32>,
  },
  FrameUpload {
    #[serde(default)]
    t_ms: Option<u64>,
    #[serde(default)]
    ts_ms: Option<u64>,
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
  MemorySummary {
    #[serde(default)]
    t_ms: Option<u64>,
    #[serde(default)]
    ts_ms: Option<u64>,
    /// Resident set size in bytes (Linux-only in the browser; nullable elsewhere).
    #[serde(default)]
    rss_bytes: Option<u64>,
    /// Convenience conversion of `rss_bytes` to MiB (`rss_bytes / 1024^2`).
    #[serde(default)]
    rss_mb: Option<f64>,
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

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn parses_memory_summary_v2_with_nulls() {
    let json = r#"{
      "event":"memory_summary",
      "schema_version":2,
      "ts_ms":100,
      "window_id":"process",
      "rss_bytes":null,
      "rss_mb":null
    }"#;

    let event: BrowserPerfLogEvent = serde_json::from_str(json).expect("parse event");
    match event {
      BrowserPerfLogEvent::V2(BrowserPerfLogEventV2::MemorySummary {
        rss_bytes, rss_mb, ..
      }) => {
        assert!(rss_bytes.is_none());
        assert!(rss_mb.is_none());
      }
      other => panic!("unexpected event parsed: {other:?}"),
    }
  }

  #[test]
  fn parses_memory_summary_v2_with_values() {
    let json = r#"{
      "event":"memory_summary",
      "schema_version":2,
      "ts_ms":200,
      "window_id":"process",
      "rss_bytes":1048576,
      "rss_mb":1.0
    }"#;

    let event: BrowserPerfLogEvent = serde_json::from_str(json).expect("parse event");
    match event {
      BrowserPerfLogEvent::V2(BrowserPerfLogEventV2::MemorySummary {
        rss_bytes, rss_mb, ..
      }) => {
        assert_eq!(rss_bytes, Some(1048576));
        assert_eq!(rss_mb, Some(1.0));
      }
      other => panic!("unexpected event parsed: {other:?}"),
    }
  }

  #[test]
  fn parses_tab_switch_v2() {
    let json = r#"{
      "event":"tab_switch",
      "schema_version":2,
      "t_ms":300,
      "window_id":"WindowId(1)",
      "from_tab_id":1,
      "to_tab_id":2,
      "cached":true,
      "latency_ms":42
    }"#;

    let event: BrowserPerfLogEvent = serde_json::from_str(json).expect("parse event");
    match event {
      BrowserPerfLogEvent::V2(BrowserPerfLogEventV2::TabSwitch { latency_ms, .. }) => {
        assert_eq!(latency_ms, Some(42));
      }
      other => panic!("unexpected event parsed: {other:?}"),
    }
  }

  #[test]
  fn parses_idle_summary_v2_alias() {
    let json = r#"{
      "event":"idle_summary",
      "schema_version":2,
      "t_ms":400,
      "window_id":"process",
      "idle_frames_per_sec":12.5
    }"#;

    let event: BrowserPerfLogEvent = serde_json::from_str(json).expect("parse event");
    match event {
      BrowserPerfLogEvent::V2(BrowserPerfLogEventV2::IdleSample { idle_fps, .. }) => {
        assert_eq!(idle_fps, Some(12.5));
      }
      other => panic!("unexpected event parsed: {other:?}"),
    }
  }
}
