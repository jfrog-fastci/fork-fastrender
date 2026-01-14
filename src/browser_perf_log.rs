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
  /// Emitted once at startup so a perf log is self-describing.
  RunStart {
    #[serde(default)]
    t_ms: Option<u64>,
    #[serde(default)]
    ts_ms: Option<u64>,
    #[serde(default)]
    pid: Option<u32>,
    #[serde(default)]
    start_unix_ms: Option<u64>,
    /// Best-effort RSS snapshot at startup (Linux-only in the browser; nullable elsewhere).
    #[serde(default)]
    rss_bytes: Option<u64>,
    /// Nested build metadata (shape is defined by `fastrender::perf_log::BuildInfo`).
    #[serde(default)]
    build: Option<serde_json::Value>,
    /// Nested config snapshot (shape is defined by `fastrender::perf_log::RunConfig`).
    #[serde(default)]
    config: Option<serde_json::Value>,
  },
  /// Emitted once on graceful shutdown (best-effort).
  RunEnd {
    #[serde(default)]
    t_ms: Option<u64>,
    #[serde(default)]
    ts_ms: Option<u64>,
    #[serde(default)]
    frames_presented: Option<u64>,
    #[serde(default)]
    idle_frames: Option<u64>,
    #[serde(default)]
    input_events: Option<u64>,
    #[serde(default)]
    dropped_frames: Option<u64>,
    #[serde(default)]
    elapsed_ms: Option<u64>,
    #[serde(default)]
    cpu_time_ms: Option<u64>,
    #[serde(default)]
    rss_bytes: Option<u64>,
  },
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
    from_tab_id: Option<u64>,
    #[serde(default)]
    to_tab_id: Option<u64>,
    #[serde(default)]
    t_ms_start: Option<u64>,
    #[serde(default)]
    had_cached_texture: Option<bool>,
    #[serde(default)]
    switch_to_present_ms: Option<f64>,
    #[serde(default)]
    cached: Option<bool>,
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
  WorkerWakeSummary {
    #[serde(default)]
    t_ms: Option<u64>,
    #[serde(default)]
    ts_ms: Option<u64>,
    #[serde(default)]
    worker_msgs_forwarded_per_sec: Option<f32>,
    #[serde(default)]
    worker_msgs_processed_per_sec: Option<f32>,
    #[serde(default)]
    worker_wakes_handled_per_sec: Option<f32>,
    #[serde(default)]
    worker_wake_events_sent_per_sec: Option<f32>,
    #[serde(default)]
    worker_wake_events_coalesced_per_sec: Option<f32>,
    #[serde(default)]
    worker_followup_wakes_per_sec: Option<f32>,
    #[serde(default)]
    worker_empty_wakes_per_sec: Option<f32>,
    #[serde(default)]
    worker_pending_msgs_estimate: Option<u64>,
    #[serde(default)]
    worker_msgs_per_nonempty_wake: Option<f32>,
    #[serde(default)]
    worker_last_drain: Option<u64>,
    #[serde(default)]
    worker_max_drain: Option<u64>,
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
      "t_ms_start":250,
      "window_id":"WindowId(1)",
      "from_tab_id":1,
      "to_tab_id":2,
      "had_cached_texture":true,
      "switch_to_present_ms":42.5,
      "cached":true,
      "latency_ms":42
    }"#;

    let event: BrowserPerfLogEvent = serde_json::from_str(json).expect("parse event");
    match event {
      BrowserPerfLogEvent::V2(BrowserPerfLogEventV2::TabSwitch {
        from_tab_id,
        to_tab_id,
        t_ms_start,
        had_cached_texture,
        switch_to_present_ms,
        cached,
        latency_ms,
        ..
      }) => {
        assert_eq!(from_tab_id, Some(1));
        assert_eq!(to_tab_id, Some(2));
        assert_eq!(t_ms_start, Some(250));
        assert_eq!(had_cached_texture, Some(true));
        assert_eq!(switch_to_present_ms, Some(42.5));
        assert_eq!(cached, Some(true));
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

  #[test]
  fn parses_run_start_v2() {
    let json = r#"{
      "event":"run_start",
      "schema_version":2,
      "t_ms":0,
      "pid":123,
      "rss_bytes":1048576,
      "start_unix_ms":1700000000000,
      "build":{"crate_version":"0.1.0","debug":true,"target":"x86_64-linux"},
      "config":{"hud_enabled":true,"perf_log_enabled":true}
    }"#;

    let event: BrowserPerfLogEvent = serde_json::from_str(json).expect("parse event");
    match event {
      BrowserPerfLogEvent::V2(BrowserPerfLogEventV2::RunStart { pid, rss_bytes, .. }) => {
        assert_eq!(pid, Some(123));
        assert_eq!(rss_bytes, Some(1048576));
      }
      other => panic!("unexpected event parsed: {other:?}"),
    }
  }

  #[test]
  fn parses_run_end_v2() {
    let json = r#"{
      "event":"run_end",
      "schema_version":2,
      "t_ms":1000,
      "frames_presented":10,
      "idle_frames":3,
      "input_events":5,
      "dropped_frames":2,
      "elapsed_ms":1000
    }"#;

    let event: BrowserPerfLogEvent = serde_json::from_str(json).expect("parse event");
    match event {
      BrowserPerfLogEvent::V2(BrowserPerfLogEventV2::RunEnd {
        frames_presented,
        dropped_frames,
        ..
      }) => {
        assert_eq!(frames_presented, Some(10));
        assert_eq!(dropped_frames, Some(2));
      }
      other => panic!("unexpected event parsed: {other:?}"),
    }
  }

  #[test]
  fn parses_worker_wake_summary_v2() {
    let json = r#"{
      "event":"worker_wake_summary",
      "schema_version":2,
      "t_ms":500,
      "window_id":"WindowId(9)",
      "worker_msgs_forwarded_per_sec":10.0,
      "worker_msgs_processed_per_sec":9.5,
      "worker_wakes_handled_per_sec":2.0,
      "worker_wake_events_sent_per_sec":1.0,
      "worker_wake_events_coalesced_per_sec":99.0,
      "worker_followup_wakes_per_sec":0.25,
      "worker_empty_wakes_per_sec":0.5,
      "worker_pending_msgs_estimate":12,
      "worker_msgs_per_nonempty_wake":4.0,
      "worker_last_drain":8,
      "worker_max_drain":16
    }"#;

    let event: BrowserPerfLogEvent = serde_json::from_str(json).expect("parse event");
    match event {
      BrowserPerfLogEvent::V2(BrowserPerfLogEventV2::WorkerWakeSummary {
        worker_wake_events_coalesced_per_sec,
        worker_pending_msgs_estimate,
        worker_max_drain,
        ..
      }) => {
        assert_eq!(worker_wake_events_coalesced_per_sec, Some(99.0));
        assert_eq!(worker_pending_msgs_estimate, Some(12));
        assert_eq!(worker_max_drain, Some(16));
      }
      other => panic!("unexpected event parsed: {other:?}"),
    }
  }
}
