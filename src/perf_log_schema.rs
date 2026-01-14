//! Shared schema for `FASTR_PERF_LOG` JSONL performance logs.
//!
//! This module is intended to be the single source of truth for the event schema so that the
//! windowed browser emitter and any offline analysis tools stay in sync.
//!
//! The schema is designed to be *append-only*:
//! - Adding a new event variant is additive.
//! - Adding a new optional field to an existing event is additive.
//! - Removing/renaming a field or changing its meaning must bump [`PERF_LOG_VERSION`].
//!
//! All timestamps are relative to `run_start` to avoid wall-clock instability.

use crate::render_control::StageHeartbeat;
use serde::{Deserialize, Serialize};

/// Current `FASTR_PERF_LOG` schema version.
///
/// Bump this when making an incompatible change (field rename/removal or meaning change).
pub const PERF_LOG_VERSION: u32 = 1;

/// A monotonic timestamp, measured in microseconds relative to the `run_start` event.
#[derive(
  Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, Default,
)]
#[serde(transparent)]
pub struct PerfTimestampMicros(pub u64);

impl PerfTimestampMicros {
  pub const ZERO: Self = Self(0);

  #[must_use]
  pub fn from_duration(duration: std::time::Duration) -> Self {
    let micros = duration.as_micros();
    Self(micros.min(u128::from(u64::MAX)) as u64)
  }
}

/// A duration, measured in microseconds.
#[derive(
  Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, Default,
)]
#[serde(transparent)]
pub struct PerfDurationMicros(pub u64);

impl PerfDurationMicros {
  #[must_use]
  pub fn from_duration(duration: std::time::Duration) -> Self {
    let micros = duration.as_micros();
    Self(micros.min(u128::from(u64::MAX)) as u64)
  }
}

/// Tab identifier used in perf logs.
///
/// This mirrors [`crate::ui::TabId`] but is defined here so the perf log schema remains independent
/// from the UI↔worker messaging protocol.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct PerfTabId(pub u64);

#[cfg(feature = "vmjs")]
impl From<crate::ui::TabId> for PerfTabId {
  fn from(tab_id: crate::ui::TabId) -> Self {
    Self(tab_id.0)
  }
}

/// Common metadata attached to most events.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct PerfEventMeta {
  /// Monotonic timestamp in microseconds since `run_start`.
  pub t_us: PerfTimestampMicros,

  /// Tab this event belongs to.
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub tab_id: Option<PerfTabId>,
}

/// Viewport size information (typically in physical pixels).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct PerfViewportSize {
  pub width_px: u32,
  pub height_px: u32,

  /// Device pixel ratio (DPR), when known.
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub dpr: Option<f32>,
}

/// Input payload for [`PerfLogEvent::Input`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PerfInputKind {
  Key {
    /// Hardware/physical key code (e.g. `"KeyA"`, `"ArrowLeft"`).
    code: String,
    /// `"down"`, `"up"`, or `"repeat"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    state: Option<PerfKeyState>,
  },
  Pointer {
    x_px: f32,
    y_px: f32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    button: Option<PerfPointerButton>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    state: Option<PerfPointerState>,
  },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PerfKeyState {
  Down,
  Up,
  Repeat,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PerfPointerState {
  Down,
  Up,
  Move,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PerfPointerButton {
  Primary,
  Secondary,
  Middle,
  Back,
  Forward,
  Other(u16),
}

/// A single JSONL record emitted by `FASTR_PERF_LOG`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[non_exhaustive]
pub enum PerfLogEvent {
  RunStart {
    #[serde(flatten)]
    meta: PerfEventMeta,
    /// Perf log schema version (see [`PERF_LOG_VERSION`]).
    version: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    run_name: Option<String>,
  },
  RunEnd {
    #[serde(flatten)]
    meta: PerfEventMeta,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    ok: Option<bool>,
  },
  /// A rendered/presented frame.
  Frame {
    #[serde(flatten)]
    meta: PerfEventMeta,
    frame_id: u64,
    /// Time since the previous frame (when known).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    dt_us: Option<PerfDurationMicros>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    viewport: Option<PerfViewportSize>,
  },
  /// Render-stage heartbeat forwarding.
  Stage {
    #[serde(flatten)]
    meta: PerfEventMeta,
    stage: StageHeartbeat,
  },
  NavigationStart {
    #[serde(flatten)]
    meta: PerfEventMeta,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    nav_id: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    url: Option<String>,
  },
  /// First paint after a navigation.
  FirstPaint {
    #[serde(flatten)]
    meta: PerfEventMeta,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    nav_id: Option<u64>,
  },
  Input {
    #[serde(flatten)]
    meta: PerfEventMeta,
    #[serde(flatten)]
    input: PerfInputKind,
  },
  Scroll {
    #[serde(flatten)]
    meta: PerfEventMeta,
    delta_x_px: f32,
    delta_y_px: f32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    offset_x_px: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    offset_y_px: Option<f32>,
  },
  Resize {
    #[serde(flatten)]
    meta: PerfEventMeta,
    viewport: PerfViewportSize,
  },
  TabSwitch {
    #[serde(flatten)]
    meta: PerfEventMeta,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    from_tab_id: Option<PerfTabId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    to_tab_id: Option<PerfTabId>,
  },
  /// Periodic resource usage sample.
  Sample {
    #[serde(flatten)]
    meta: PerfEventMeta,
    /// Process CPU usage percentage (0-100 per core, depending on sampling source).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    cpu_pct: Option<f32>,
    /// Resident set size in bytes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    rss_bytes: Option<u64>,
  },
}

impl PerfLogEvent {
  /// Convenience constructor that ensures the schema version is filled from [`PERF_LOG_VERSION`].
  #[must_use]
  pub fn run_start(meta: PerfEventMeta, run_name: Option<String>) -> Self {
    Self::RunStart {
      meta,
      version: PERF_LOG_VERSION,
      run_name,
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  fn round_trip(event: &PerfLogEvent) {
    let encoded = serde_json::to_string(event).expect("serialize");
    let decoded: PerfLogEvent = serde_json::from_str(&encoded).expect("deserialize");
    assert_eq!(&decoded, event);
  }

  #[test]
  fn perf_log_event_round_trip_all_variants() {
    let tab = PerfTabId(42);

    let events = vec![
      PerfLogEvent::run_start(
        PerfEventMeta {
          t_us: PerfTimestampMicros::ZERO,
          tab_id: None,
        },
        Some("smoke".to_string()),
      ),
      PerfLogEvent::RunEnd {
        meta: PerfEventMeta {
          t_us: PerfTimestampMicros(1_234),
          tab_id: None,
        },
        ok: Some(true),
      },
      PerfLogEvent::Frame {
        meta: PerfEventMeta {
          t_us: PerfTimestampMicros(10_000),
          tab_id: Some(tab),
        },
        frame_id: 7,
        dt_us: Some(PerfDurationMicros(16_666)),
        viewport: Some(PerfViewportSize {
          width_px: 800,
          height_px: 600,
          dpr: Some(2.0),
        }),
      },
      PerfLogEvent::Stage {
        meta: PerfEventMeta {
          t_us: PerfTimestampMicros(11_000),
          tab_id: Some(tab),
        },
        stage: StageHeartbeat::Layout,
      },
      PerfLogEvent::NavigationStart {
        meta: PerfEventMeta {
          t_us: PerfTimestampMicros(12_000),
          tab_id: Some(tab),
        },
        nav_id: Some(1),
        url: Some("https://example.test/".to_string()),
      },
      PerfLogEvent::FirstPaint {
        meta: PerfEventMeta {
          t_us: PerfTimestampMicros(34_000),
          tab_id: Some(tab),
        },
        nav_id: Some(1),
      },
      PerfLogEvent::Input {
        meta: PerfEventMeta {
          t_us: PerfTimestampMicros(40_000),
          tab_id: Some(tab),
        },
        input: PerfInputKind::Key {
          code: "KeyA".to_string(),
          state: Some(PerfKeyState::Down),
        },
      },
      PerfLogEvent::Scroll {
        meta: PerfEventMeta {
          t_us: PerfTimestampMicros(41_000),
          tab_id: Some(tab),
        },
        delta_x_px: 0.0,
        delta_y_px: 120.0,
        offset_x_px: Some(0.0),
        offset_y_px: Some(240.0),
      },
      PerfLogEvent::Resize {
        meta: PerfEventMeta {
          t_us: PerfTimestampMicros(42_000),
          tab_id: Some(tab),
        },
        viewport: PerfViewportSize {
          width_px: 1024,
          height_px: 768,
          dpr: None,
        },
      },
      PerfLogEvent::TabSwitch {
        meta: PerfEventMeta {
          t_us: PerfTimestampMicros(43_000),
          tab_id: None,
        },
        from_tab_id: Some(PerfTabId(1)),
        to_tab_id: Some(tab),
      },
      PerfLogEvent::Sample {
        meta: PerfEventMeta {
          t_us: PerfTimestampMicros(44_000),
          tab_id: None,
        },
        cpu_pct: Some(12.5),
        rss_bytes: Some(123_456_789),
      },
    ];

    for event in &events {
      round_trip(event);
    }
  }

  #[test]
  fn run_start_emits_perf_log_version() {
    let event = PerfLogEvent::run_start(
      PerfEventMeta {
        t_us: PerfTimestampMicros::ZERO,
        tab_id: None,
      },
      None,
    );
    let value = serde_json::to_value(&event).expect("serialize to value");
    assert_eq!(value["type"], "run_start");
    assert_eq!(value["version"], PERF_LOG_VERSION);
  }
}
