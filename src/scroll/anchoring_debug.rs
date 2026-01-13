use crate::debug::runtime;
use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ScrollAnchoringContainerId {
  Viewport,
  Element { box_id: usize },
}

impl fmt::Display for ScrollAnchoringContainerId {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    match *self {
      Self::Viewport => f.write_str("viewport"),
      Self::Element { box_id } => write!(f, "element#{box_id}"),
    }
  }
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct ScrollAnchoringLogEvent {
  pub container: ScrollAnchoringContainerId,
  pub anchor_box_id: Option<usize>,
  pub y0: Option<f32>,
  pub y1: Option<f32>,
  pub adjustment: Option<f32>,
  pub scroll_y_before: Option<f32>,
  pub scroll_y_unclamped: Option<f32>,
  pub scroll_y_clamped: Option<f32>,
  pub suppressed_reason: Option<&'static str>,
}

#[inline]
fn enabled() -> bool {
  runtime::runtime_toggles().truthy("FASTR_LOG_SCROLL_ANCHORING")
}

struct OptUsize(Option<usize>);
impl fmt::Display for OptUsize {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    match self.0 {
      Some(v) => write!(f, "{v}"),
      None => f.write_str("-"),
    }
  }
}

struct OptF32(Option<f32>);
impl fmt::Display for OptF32 {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    match self.0 {
      Some(v) => write!(f, "{v:.3}"),
      None => f.write_str("-"),
    }
  }
}

struct Suppressed(Option<&'static str>);
impl fmt::Display for Suppressed {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    match self.0 {
      Some(reason) => write!(f, "true:{reason}"),
      None => f.write_str("false"),
    }
  }
}

#[inline]
pub(crate) fn log_if_enabled(f: impl FnOnce() -> ScrollAnchoringLogEvent) {
  if !enabled() {
    return;
  }
  log(f());
}

pub(crate) fn log(event: ScrollAnchoringLogEvent) {
  // Keep this formatting allocation-free. All values are formatted directly into the stderr buffer.
  eprintln!(
    "[scroll-anchoring] container={} anchor={} y0={} y1={} delta={} scroll_y0={} scroll_y1={} scroll_y_clamped={} suppressed={}",
    event.container,
    OptUsize(event.anchor_box_id),
    OptF32(event.y0),
    OptF32(event.y1),
    OptF32(event.adjustment),
    OptF32(event.scroll_y_before),
    OptF32(event.scroll_y_unclamped),
    OptF32(event.scroll_y_clamped),
    Suppressed(event.suppressed_reason),
  );
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::debug::runtime::{with_thread_runtime_toggles, RuntimeToggles};
  use std::collections::HashMap;
  use std::sync::Arc;

  #[test]
  fn scroll_anchoring_debug_log_does_not_panic_when_enabled() {
    let toggles = Arc::new(RuntimeToggles::from_map(HashMap::from([(
      "FASTR_LOG_SCROLL_ANCHORING".to_string(),
      "1".to_string(),
    )])));

    with_thread_runtime_toggles(toggles, || {
      log_if_enabled(|| ScrollAnchoringLogEvent {
        container: ScrollAnchoringContainerId::Viewport,
        anchor_box_id: Some(42),
        y0: Some(100.0),
        y1: Some(120.5),
        adjustment: Some(20.5),
        scroll_y_before: Some(200.0),
        scroll_y_unclamped: Some(220.5),
        scroll_y_clamped: Some(210.0),
        suppressed_reason: None,
      });

      log_if_enabled(|| ScrollAnchoringLogEvent {
        container: ScrollAnchoringContainerId::Element { box_id: 7 },
        anchor_box_id: None,
        y0: None,
        y1: None,
        adjustment: None,
        scroll_y_before: Some(0.0),
        scroll_y_unclamped: None,
        scroll_y_clamped: None,
        suppressed_reason: Some("no eligible anchor"),
      });
    });
  }
}

