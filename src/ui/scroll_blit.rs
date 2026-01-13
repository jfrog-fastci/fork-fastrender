//! Scroll blit fast-path eligibility + fallback diagnostics.
//!
//! The browser UI can attempt a scroll "blit" fast-path (shift the previously rendered frame
//! instead of repainting the full viewport). This module provides:
//! - a structured `ScrollBlitFallbackReason` enum describing *why* the fast-path is unavailable
//! - a lightweight "plan" function that classifies the reason (used by the worker)
//! - test/debug hooks to read the last recorded fallback reason
//!
//! Note: the actual scroll-blit implementation may live elsewhere; this module is intentionally
//! scoped to *observability* so regressions don't silently disable the fast-path.

use crate::geometry::Point;
use crate::scroll::ScrollState;
use crate::tree::fragment_tree::{FragmentNode, FragmentTree};
use crate::{PreparedDocument, Size};

use crate::style::position::Position;
use crate::style::types::AnimationTimeline;

/// Reasons why the scroll blit fast-path could not be used.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ScrollBlitFallbackReason {
  /// The scroll delta (in *device pixels*) is not an integer, so we cannot shift the pixel buffer
  /// without resampling.
  NonIntegerDevicePixelDelta,
  /// The document contains `position: fixed` or `position: sticky` content which does not scroll as
  /// a simple translation of the whole frame.
  FixedOrStickyPresent,
  /// CSS scroll snap adjusted the effective scroll position, so the new frame is not a pure shift
  /// of the previous frame.
  ScrollSnapAdjustedEffectiveScroll,
  /// The document contains scroll-driven animations (scroll/view timelines), so scrolling can
  /// change pixels without a pure translation.
  ScrollDrivenAnimationsPresent,
}

impl ScrollBlitFallbackReason {
  const COUNT: usize = 4;

  fn as_index(self) -> usize {
    match self {
      ScrollBlitFallbackReason::NonIntegerDevicePixelDelta => 0,
      ScrollBlitFallbackReason::FixedOrStickyPresent => 1,
      ScrollBlitFallbackReason::ScrollSnapAdjustedEffectiveScroll => 2,
      ScrollBlitFallbackReason::ScrollDrivenAnimationsPresent => 3,
    }
  }

  fn from_index(value: usize) -> Option<Self> {
    match value {
      0 => Some(ScrollBlitFallbackReason::NonIntegerDevicePixelDelta),
      1 => Some(ScrollBlitFallbackReason::FixedOrStickyPresent),
      2 => Some(ScrollBlitFallbackReason::ScrollSnapAdjustedEffectiveScroll),
      3 => Some(ScrollBlitFallbackReason::ScrollDrivenAnimationsPresent),
      _ => None,
    }
  }
}

/// Output of a successful scroll-blit eligibility check.
///
/// This does **not** perform the blit; it only captures the information needed to do so.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ScrollBlitPlan {
  /// Scroll delta in device pixels.
  pub delta_device_px: (i32, i32),
}

fn approx_integer(v: f32) -> Option<i32> {
  if !v.is_finite() {
    return None;
  }
  // Keep this tolerance aligned with scroll snap's float epsilon (see `scroll::pick_snap_target`).
  let rounded = v.round();
  if (v - rounded).abs() <= 1e-3 {
    Some(rounded as i32)
  } else {
    None
  }
}

fn fragment_tree_has_fixed_or_sticky(tree: &FragmentTree) -> bool {
  let mut stack: Vec<&FragmentNode> = Vec::new();
  stack.push(&tree.root);
  for root in &tree.additional_fragments {
    stack.push(root);
  }
  while let Some(node) = stack.pop() {
    if let Some(style) = node.style.as_deref() {
      if matches!(style.position, Position::Fixed | Position::Sticky) {
        return true;
      }
    }
    for child in node.children.iter() {
      stack.push(child);
    }
  }
  false
}

fn fragment_tree_has_scroll_driven_animations(tree: &FragmentTree) -> bool {
  let mut stack: Vec<&FragmentNode> = Vec::new();
  stack.push(&tree.root);
  for root in &tree.additional_fragments {
    stack.push(root);
  }
  while let Some(node) = stack.pop() {
    if let Some(style) = node.style.as_deref() {
      if style.animation_timelines.iter().any(|timeline| {
        matches!(
          timeline,
          AnimationTimeline::Scroll(_) | AnimationTimeline::View(_) | AnimationTimeline::Named(_)
        )
      }) {
        return true;
      }
    }
    for child in node.children.iter() {
      stack.push(child);
    }
  }
  false
}

fn effective_scroll_state_for_paint_like_scroll_blit(
  mut tree: FragmentTree,
  mut scroll_state: ScrollState,
  scrollport_viewport: Size,
) -> ScrollState {
  // Mirror the subset of `api::paint_fragment_tree_with_state` that can adjust the effective scroll
  // offset before painting:
  // - scroll snap
  // - sanitization + clamping to scroll bounds
  let snap_result = crate::scroll::apply_scroll_snap(&mut tree, &scroll_state);
  scroll_state = snap_result.state;

  scroll_state.viewport = Point::new(
    if scroll_state.viewport.x.is_finite() {
      scroll_state.viewport.x
    } else {
      0.0
    },
    if scroll_state.viewport.y.is_finite() {
      scroll_state.viewport.y
    } else {
      0.0
    },
  );

  if let Some(bounds) = crate::scroll::build_scroll_chain(&tree.root, scrollport_viewport, &[])
    .last()
    .map(|state| state.bounds)
  {
    scroll_state.viewport = bounds.clamp(scroll_state.viewport);
  }

  scroll_state
}

/// Computes a scroll-blit plan, or returns a structured reason why the fast-path is unavailable.
pub(crate) fn scroll_blit_plan(
  prepared: &PreparedDocument,
  prev_scroll: &ScrollState,
  next_scroll: &ScrollState,
) -> std::result::Result<ScrollBlitPlan, ScrollBlitFallbackReason> {
  let dpr = prepared.device_pixel_ratio();
  let delta_css = Point::new(
    next_scroll.viewport.x - prev_scroll.viewport.x,
    next_scroll.viewport.y - prev_scroll.viewport.y,
  );
  let delta_device = Point::new(delta_css.x * dpr, delta_css.y * dpr);

  let dx = approx_integer(delta_device.x).ok_or(ScrollBlitFallbackReason::NonIntegerDevicePixelDelta)?;
  let dy = approx_integer(delta_device.y).ok_or(ScrollBlitFallbackReason::NonIntegerDevicePixelDelta)?;

  if fragment_tree_has_fixed_or_sticky(prepared.fragment_tree()) {
    return Err(ScrollBlitFallbackReason::FixedOrStickyPresent);
  }

  // Scroll snap can adjust the effective scroll position, which invalidates a simple blit. Compute
  // the paint-time effective scroll for the *new* state and check if it differs.
  let scrollport_viewport = prepared.layout_viewport();
  let effective =
    effective_scroll_state_for_paint_like_scroll_blit(prepared.fragment_tree().clone(), next_scroll.clone(), scrollport_viewport);
  let requested = next_scroll.viewport;
  if (effective.viewport.x - requested.x).abs() > 1e-3 || (effective.viewport.y - requested.y).abs() > 1e-3 {
    return Err(ScrollBlitFallbackReason::ScrollSnapAdjustedEffectiveScroll);
  }

  if fragment_tree_has_scroll_driven_animations(prepared.fragment_tree()) {
    return Err(ScrollBlitFallbackReason::ScrollDrivenAnimationsPresent);
  }

  Ok(ScrollBlitPlan {
    delta_device_px: (dx, dy),
  })
}

// -----------------------------------------------------------------------------
// Test/debug hooks
// -----------------------------------------------------------------------------

#[cfg(any(test, feature = "browser_ui"))]
use std::sync::atomic::{AtomicUsize, Ordering};

/// Last recorded scroll-blit fallback reason.
///
/// Stored as `reason_index + 1` so that `0` can represent "none recorded".
#[cfg(any(test, feature = "browser_ui"))]
static LAST_SCROLL_BLIT_FALLBACK_REASON: AtomicUsize = AtomicUsize::new(0);

/// Per-reason fallback counters.
#[cfg(any(test, feature = "browser_ui"))]
static SCROLL_BLIT_FALLBACK_COUNTS: [AtomicUsize; ScrollBlitFallbackReason::COUNT] = [
  AtomicUsize::new(0),
  AtomicUsize::new(0),
  AtomicUsize::new(0),
  AtomicUsize::new(0),
];

#[cfg(any(test, feature = "browser_ui"))]
pub(crate) fn record_scroll_blit_fallback_reason(reason: ScrollBlitFallbackReason) {
  LAST_SCROLL_BLIT_FALLBACK_REASON.store(reason.as_index() + 1, Ordering::Relaxed);
  SCROLL_BLIT_FALLBACK_COUNTS[reason.as_index()].fetch_add(1, Ordering::Relaxed);
}

#[cfg(any(test, feature = "browser_ui"))]
pub(crate) fn last_scroll_blit_fallback_reason_for_test() -> Option<ScrollBlitFallbackReason> {
  let stored = LAST_SCROLL_BLIT_FALLBACK_REASON.load(Ordering::Relaxed);
  stored
    .checked_sub(1)
    .and_then(ScrollBlitFallbackReason::from_index)
}

#[cfg(any(test, feature = "browser_ui"))]
pub(crate) fn reset_scroll_blit_fallback_reason_for_test() {
  LAST_SCROLL_BLIT_FALLBACK_REASON.store(0, Ordering::Relaxed);
  for counter in &SCROLL_BLIT_FALLBACK_COUNTS {
    counter.store(0, Ordering::Relaxed);
  }
}

#[cfg(any(test, feature = "browser_ui"))]
pub(crate) fn scroll_blit_fallback_count_for_test(reason: ScrollBlitFallbackReason) -> usize {
  SCROLL_BLIT_FALLBACK_COUNTS[reason.as_index()].load(Ordering::Relaxed)
}

// -----------------------------------------------------------------------------
// Unit tests
// -----------------------------------------------------------------------------

#[cfg(test)]
mod tests {
  use super::*;
  use crate::text::font_db::FontConfig;
  use crate::FastRender;
  use crate::RenderOptions;
  use std::sync::{Mutex, OnceLock};

  static TEST_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

  fn test_guard() -> std::sync::MutexGuard<'static, ()> {
    TEST_LOCK
      .get_or_init(|| Mutex::new(()))
      .lock()
      .unwrap_or_else(|poisoned| poisoned.into_inner())
  }

  fn renderer_for_tests() -> FastRender {
    FastRender::builder()
      .font_sources(FontConfig::bundled_only())
      .build()
      .expect("renderer")
  }

  fn prepare_for_html(html: &str, dpr: f32) -> PreparedDocument {
    let mut renderer = renderer_for_tests();
    renderer
      .prepare_html(
        html,
        RenderOptions::new()
          .with_viewport(100, 100)
          .with_device_pixel_ratio(dpr),
      )
      .expect("prepare html")
  }

  #[test]
  fn scroll_blit_fallback_reason_fractional_device_pixel_delta() {
    let _guard = test_guard();
    reset_scroll_blit_fallback_reason_for_test();
    let prepared = prepare_for_html("<div style=\"height: 200px\"></div>", 1.0);

    let prev = ScrollState::with_viewport(Point::new(0.0, 0.0));
    let next = ScrollState::with_viewport(Point::new(0.0, 0.5));
    let err = scroll_blit_plan(&prepared, &prev, &next).unwrap_err();
    assert_eq!(err, ScrollBlitFallbackReason::NonIntegerDevicePixelDelta);
    record_scroll_blit_fallback_reason(err);
    assert_eq!(
      last_scroll_blit_fallback_reason_for_test(),
      Some(ScrollBlitFallbackReason::NonIntegerDevicePixelDelta)
    );
  }

  #[test]
  fn scroll_blit_fallback_reason_fixed_or_sticky_present() {
    let _guard = test_guard();
    reset_scroll_blit_fallback_reason_for_test();
    let html = r#"
      <style>
        html, body { margin: 0; }
        #fixed { position: fixed; top: 0; left: 0; width: 10px; height: 10px; background: red; }
      </style>
      <div id="fixed"></div>
      <div style="height: 500px"></div>
    "#;
    let prepared = prepare_for_html(html, 1.0);

    let prev = ScrollState::with_viewport(Point::new(0.0, 0.0));
    let next = ScrollState::with_viewport(Point::new(0.0, 10.0));
    let err = scroll_blit_plan(&prepared, &prev, &next).unwrap_err();
    assert_eq!(err, ScrollBlitFallbackReason::FixedOrStickyPresent);
    record_scroll_blit_fallback_reason(err);
    assert_eq!(
      last_scroll_blit_fallback_reason_for_test(),
      Some(ScrollBlitFallbackReason::FixedOrStickyPresent)
    );
  }

  #[test]
  fn scroll_blit_fallback_reason_scroll_snap_adjusted() {
    let _guard = test_guard();
    reset_scroll_blit_fallback_reason_for_test();
    let html = r#"
      <style>
        html, body { margin: 0; }
        html { scroll-snap-type: y mandatory; }
        .snap { height: 100px; scroll-snap-align: start; }
      </style>
      <div class="snap"></div>
      <div class="snap"></div>
      <div class="snap"></div>
    "#;
    let prepared = prepare_for_html(html, 1.0);

    let prev = ScrollState::with_viewport(Point::new(0.0, 0.0));
    // 70px should snap to 100px under mandatory snapping.
    let next = ScrollState::with_viewport(Point::new(0.0, 70.0));
    let err = scroll_blit_plan(&prepared, &prev, &next).unwrap_err();
    assert_eq!(err, ScrollBlitFallbackReason::ScrollSnapAdjustedEffectiveScroll);
    record_scroll_blit_fallback_reason(err);
    assert_eq!(
      last_scroll_blit_fallback_reason_for_test(),
      Some(ScrollBlitFallbackReason::ScrollSnapAdjustedEffectiveScroll)
    );
  }

  #[test]
  fn scroll_blit_fallback_reason_scroll_driven_animation_present() {
    let _guard = test_guard();
    reset_scroll_blit_fallback_reason_for_test();
    let html = r#"
      <style>
        html, body { margin: 0; }
        #box {
          width: 10px;
          height: 10px;
          background: red;
          animation-name: fade;
          animation-duration: 1s;
          animation-timing-function: linear;
          animation-timeline: scroll(root);
        }
        @keyframes fade {
          from { opacity: 0; }
          to { opacity: 1; }
        }
      </style>
      <div id="box"></div>
      <div style="height: 500px"></div>
    "#;
    let prepared = prepare_for_html(html, 1.0);

    let prev = ScrollState::with_viewport(Point::new(0.0, 0.0));
    let next = ScrollState::with_viewport(Point::new(0.0, 10.0));
    let err = scroll_blit_plan(&prepared, &prev, &next).unwrap_err();
    assert_eq!(err, ScrollBlitFallbackReason::ScrollDrivenAnimationsPresent);
    record_scroll_blit_fallback_reason(err);
    assert_eq!(
      last_scroll_blit_fallback_reason_for_test(),
      Some(ScrollBlitFallbackReason::ScrollDrivenAnimationsPresent)
    );
  }
}
