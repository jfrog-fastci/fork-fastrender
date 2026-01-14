//! Shared schema for `FASTR_PERF_LOG` JSON Lines (JSONL) performance logs.
//!
//! The windowed browser (`src/bin/browser.rs`) emits newline-delimited JSON objects. Offline tools
//! (e.g. `browser_perf_log_summary`) parse the same stream. Keeping the schema here avoids
//! producer/consumer drift.
//!
//! Notes:
//! - This module intentionally avoids any `browser_ui` types (winit/wgpu/egui).
//! - The current emitter schema is versioned via a `schema_version` field on each event.

use serde::{Deserialize, Serialize};
use std::io;
use std::io::Write;
use std::time::Instant;

/// Current `FASTR_PERF_LOG` schema version emitted by the windowed browser.
pub const PERF_LOG_SCHEMA_VERSION: u32 = 2;

/// Alias used by the historical in-binary schema (`src/bin/browser.rs`).
pub const SCHEMA_VERSION: u32 = PERF_LOG_SCHEMA_VERSION;

/// Schema versions accepted by [`parse_jsonl_line`].
pub const SUPPORTED_SCHEMA_VERSIONS: &[u32] = &[1, 2];

fn deserialize_schema_version<'de, D>(deserializer: D) -> Result<u32, D::Error>
where
  D: serde::Deserializer<'de>,
{
  let value = u32::deserialize(deserializer)?;
  if !SUPPORTED_SCHEMA_VERSIONS.contains(&value) {
    return Err(serde::de::Error::custom(format!(
      "unsupported perf log schema_version {value} (supported: {SUPPORTED_SCHEMA_VERSIONS:?})"
    )));
  }
  Ok(value)
}

fn default_count_one() -> u32 {
  1
}

fn default_empty_str() -> &'static str {
  ""
}

/// Coarse input-kind classification.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
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

impl InputKind {
  #[must_use]
  pub fn as_str(self) -> &'static str {
    match self {
      Self::Keyboard => "keyboard",
      Self::MouseWheel => "mouse_wheel",
      Self::PointerMove => "pointer_move",
      Self::Button => "button",
      Self::Mouse => "mouse",
      Self::Unknown => "unknown",
    }
  }
}

impl Default for InputKind {
  fn default() -> Self {
    Self::Unknown
  }
}

/// Breakdown (ms) for one UI frame. Missing fields default to `0.0` to keep parsing resilient across
/// schema versions.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct UiFrameBreakdownMs {
  pub worker_msgs_ms: f64,
  pub upload_ms: f64,
  pub egui_ms: f64,
  pub tessellate_ms: f64,
  pub wgpu_ms: f64,
  pub present_ms: f64,
  pub total_ms: f64,
}

/// One `FASTR_PERF_LOG` JSONL event record.
///
/// The schema is intentionally permissive for forward/backward compatibility:
/// - Unknown extra fields are ignored.
/// - Many fields are optional/defaulted so older logs remain parseable.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum PerfEvent<'a> {
  Frame {
    #[serde(deserialize_with = "deserialize_schema_version")]
    schema_version: u32,
    #[serde(alias = "ts_ms")]
    t_ms: u64,
    #[serde(default = "default_empty_str")]
    window_id: &'a str,
    #[serde(default)]
    active_tab_id: Option<u64>,
    ui_frame_ms: f64,
    #[serde(default)]
    fps: Option<f64>,
    #[serde(default)]
    window_focused: bool,
    #[serde(default)]
    window_occluded: bool,
    #[serde(default)]
    window_minimized: bool,
    #[serde(flatten)]
    breakdown: UiFrameBreakdownMs,
  },
  Input {
    #[serde(deserialize_with = "deserialize_schema_version")]
    schema_version: u32,
    #[serde(alias = "ts_ms")]
    t_ms: u64,
    #[serde(default = "default_empty_str")]
    window_id: &'a str,
    #[serde(default)]
    active_tab_id: Option<u64>,
    #[serde(default, alias = "kind")]
    input_kind: InputKind,
    #[serde(default)]
    input_ts_ms: u64,
    input_to_present_ms: f64,
    #[serde(default = "default_count_one")]
    count: u32,
  },
  TabSwitch {
    #[serde(deserialize_with = "deserialize_schema_version")]
    schema_version: u32,
    #[serde(alias = "ts_ms")]
    t_ms: u64,
    #[serde(default = "default_empty_str")]
    window_id: &'a str,
    #[serde(default)]
    from_tab_id: u64,
    #[serde(default)]
    to_tab_id: u64,
    #[serde(default)]
    cached: bool,
    #[serde(default)]
    latency_ms: u64,
  },
  Resize {
    #[serde(deserialize_with = "deserialize_schema_version")]
    schema_version: u32,
    #[serde(alias = "ts_ms")]
    t_ms: u64,
    #[serde(default = "default_empty_str")]
    window_id: &'a str,
    #[serde(default)]
    resize_ts_ms: u64,
    resize_to_present_ms: f64,
    #[serde(default)]
    new_width_px: u32,
    #[serde(default)]
    new_height_px: u32,
  },
  Navigation {
    #[serde(deserialize_with = "deserialize_schema_version")]
    schema_version: u32,
    #[serde(alias = "ts_ms")]
    t_ms: u64,
    #[serde(default = "default_empty_str")]
    window_id: &'a str,
    #[serde(default)]
    tab_id: u64,
    #[serde(default)]
    navigation_seqno: u64,
    #[serde(default = "default_empty_str")]
    url: &'a str,
  },
  Stage {
    #[serde(deserialize_with = "deserialize_schema_version")]
    schema_version: u32,
    #[serde(alias = "ts_ms")]
    t_ms: u64,
    #[serde(default = "default_empty_str")]
    window_id: &'a str,
    #[serde(default)]
    tab_id: u64,
    #[serde(default = "default_empty_str")]
    stage: &'a str,
    #[serde(default = "default_empty_str")]
    hotspot: &'a str,
  },
  Ttfp {
    #[serde(deserialize_with = "deserialize_schema_version")]
    schema_version: u32,
    #[serde(alias = "ts_ms")]
    t_ms: u64,
    #[serde(default = "default_empty_str")]
    window_id: &'a str,
    #[serde(default)]
    tab_id: u64,
    #[serde(default)]
    navigation_seqno: u64,
    ttfp_ms: f64,
  },
  CpuSummary {
    #[serde(deserialize_with = "deserialize_schema_version")]
    schema_version: u32,
    #[serde(alias = "t_ms")]
    ts_ms: u64,
    #[serde(default = "default_empty_str")]
    window_id: &'a str,
    #[serde(default)]
    cpu_time_ms_total: u64,
    cpu_percent_recent: f64,
  },
  #[serde(rename = "idle_summary", alias = "idle_sample")]
  IdleSample {
    #[serde(deserialize_with = "deserialize_schema_version")]
    schema_version: u32,
    #[serde(alias = "ts_ms")]
    t_ms: u64,
    #[serde(default = "default_empty_str")]
    window_id: &'a str,
    #[serde(default)]
    rolling_window_ms: u64,
    #[serde(rename = "idle_frames_per_sec", alias = "idle_fps", default)]
    idle_fps: f32,
    #[serde(default)]
    idle_frames_total: u64,
    #[serde(default)]
    idle_frames_window: u64,
  },
  FrameUpload {
    #[serde(deserialize_with = "deserialize_schema_version")]
    schema_version: u32,
    #[serde(alias = "ts_ms")]
    t_ms: u64,
    #[serde(default = "default_empty_str")]
    window_id: &'a str,
    #[serde(default)]
    active_tab_id: Option<u64>,
    #[serde(default)]
    uploaded_tab_id: Option<u64>,
    #[serde(default)]
    uploads: u32,
    #[serde(default)]
    uploaded_bytes: u64,
    #[serde(default)]
    upload_last_ms: f64,
    #[serde(default)]
    upload_total_ms: f64,
    #[serde(default)]
    textures_created: u32,
    #[serde(default)]
    textures_updated: u32,
    #[serde(default)]
    push_calls: u64,
    #[serde(default)]
    overwritten_frames: u64,
    #[serde(default)]
    drained_frames: u64,
    #[serde(default)]
    pending_tabs: u64,
    #[serde(default)]
    max_pending_tabs: u64,
    #[serde(default)]
    pending_bytes: u64,
    #[serde(default)]
    received_total: u64,
    #[serde(default)]
    dropped_total: u64,
    #[serde(default)]
    drained_total: u64,
  },
  MemorySummary {
    #[serde(deserialize_with = "deserialize_schema_version")]
    schema_version: u32,
    #[serde(alias = "t_ms")]
    ts_ms: u64,
    #[serde(default = "default_empty_str")]
    window_id: &'a str,
    #[serde(default)]
    rss_bytes: Option<u64>,
    #[serde(default)]
    rss_mb: Option<f64>,
  },
  #[serde(other)]
  Unknown,
}

impl PerfEvent<'_> {
  #[must_use]
  pub fn schema_version(&self) -> Option<u32> {
    match self {
      Self::Frame { schema_version, .. }
      | Self::Input { schema_version, .. }
      | Self::TabSwitch { schema_version, .. }
      | Self::Resize { schema_version, .. }
      | Self::Navigation { schema_version, .. }
      | Self::Stage { schema_version, .. }
      | Self::Ttfp { schema_version, .. }
      | Self::CpuSummary { schema_version, .. }
      | Self::IdleSample { schema_version, .. }
      | Self::FrameUpload { schema_version, .. }
      | Self::MemorySummary { schema_version, .. } => Some(*schema_version),
      Self::Unknown => None,
    }
  }

  #[must_use]
  pub fn timestamp_ms(&self) -> Option<u64> {
    match self {
      Self::Frame { t_ms, .. }
      | Self::Input { t_ms, .. }
      | Self::TabSwitch { t_ms, .. }
      | Self::Resize { t_ms, .. }
      | Self::Navigation { t_ms, .. }
      | Self::Stage { t_ms, .. }
      | Self::Ttfp { t_ms, .. }
      | Self::IdleSample { t_ms, .. }
      | Self::FrameUpload { t_ms, .. } => Some(*t_ms),
      Self::CpuSummary { ts_ms, .. } | Self::MemorySummary { ts_ms, .. } => Some(*ts_ms),
      Self::Unknown => None,
    }
  }
}

/// Write one JSONL record (newline terminated).
pub fn write_jsonl<W: Write, T: Serialize>(writer: &mut W, value: &T) -> io::Result<()> {
  serde_json::to_writer(&mut *writer, value)
    .map_err(|err| io::Error::new(io::ErrorKind::Other, err))?;
  writer.write_all(b"\n")?;
  Ok(())
}

/// Convenience wrapper for writing a typed perf-log event as JSONL.
pub fn write_event_jsonl<W: Write>(writer: &mut W, event: &PerfEvent<'_>) -> io::Result<()> {
  write_jsonl(writer, event)
}

/// Parse a single JSONL line into a typed perf-log event.
pub fn parse_jsonl_line<'a>(line: &'a str) -> Result<PerfEvent<'a>, serde_json::Error> {
  serde_json::from_str(line.trim_end())
}

/// Simple best-effort writer for perf-log streams.
#[derive(Debug)]
pub struct JsonlPerfWriter<W: Write> {
  pub start: Instant,
  writer: W,
  disabled: bool,
}

impl<W: Write> JsonlPerfWriter<W> {
  #[must_use]
  pub fn new(start: Instant, writer: W) -> Self {
    Self {
      start,
      writer,
      disabled: false,
    }
  }

  #[must_use]
  pub fn ms_since_start(&self, t: Instant) -> u64 {
    t.saturating_duration_since(self.start).as_millis() as u64
  }

  pub fn emit_value<T: Serialize>(&mut self, value: &T) {
    if self.disabled {
      return;
    }
    if write_jsonl(&mut self.writer, value).is_err() {
      // Avoid crashing or spamming logs if stdout is closed (e.g. broken pipe).
      self.disabled = true;
    }
  }

  pub fn emit(&mut self, event: &PerfEvent<'_>) {
    self.emit_value(event);
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn round_trip_event_through_jsonl() {
    let event = PerfEvent::Frame {
      schema_version: SCHEMA_VERSION,
      t_ms: 42,
      window_id: "WindowId(1)",
      active_tab_id: Some(123),
      ui_frame_ms: 9.5,
      fps: Some(60.0),
      window_focused: true,
      window_occluded: false,
      window_minimized: false,
      breakdown: UiFrameBreakdownMs::default(),
    };

    let mut buf = Vec::new();
    write_event_jsonl(&mut buf, &event).expect("write_event_jsonl");
    let text = String::from_utf8(buf).expect("utf8");
    let parsed = parse_jsonl_line(text.lines().next().expect("line")).expect("parse_jsonl_line");
    assert_eq!(parsed, event);
  }

  #[test]
  fn schema_version_mismatch_is_a_parse_error() {
    let bad = r#"{"schema_version":999,"event":"frame","t_ms":0,"ui_frame_ms":1.0}"#;
    assert!(parse_jsonl_line(bad).is_err());
  }

  #[test]
  fn missing_schema_version_is_a_parse_error() {
    let bad = r#"{"event":"frame","t_ms":0,"ui_frame_ms":1.0}"#;
    assert!(parse_jsonl_line(bad).is_err());
  }
}
