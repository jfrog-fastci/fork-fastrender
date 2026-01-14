#![cfg(feature = "browser_ui")]

use std::time::Duration;

use fastrender::scroll::ScrollState;
use fastrender::ui::messages::{NavigationReason, RenderedFrame, RepaintReason, TabId, WorkerToUi};
use fastrender::ui::spawn_ui_worker;
use fastrender::Point;

use super::support;

const TIMEOUT: Duration = support::DEFAULT_TIMEOUT;

fn next_navigation_committed(rx: &impl support::RecvTimeout<WorkerToUi>, tab_id: TabId) -> String {
  let msg = support::recv_for_tab(rx, tab_id, TIMEOUT, |msg| {
    matches!(
      msg,
      WorkerToUi::NavigationCommitted { .. } | WorkerToUi::NavigationFailed { .. }
    )
  })
  .unwrap_or_else(|| panic!("timed out waiting for NavigationCommitted for tab {tab_id:?}"));
  match msg {
    WorkerToUi::NavigationCommitted { url, .. } => url,
    WorkerToUi::NavigationFailed { url, error, .. } => {
      panic!("navigation failed for {url}: {error}");
    }
    other => panic!("unexpected WorkerToUi message: {other:?}"),
  }
}

fn next_frame_ready(rx: &impl support::RecvTimeout<WorkerToUi>, tab_id: TabId) -> RenderedFrame {
  let msg = support::recv_for_tab(rx, tab_id, TIMEOUT, |msg| {
    matches!(msg, WorkerToUi::FrameReady { .. })
  })
  .unwrap_or_else(|| panic!("timed out waiting for FrameReady for tab {tab_id:?}"));
  match msg {
    WorkerToUi::FrameReady { frame, .. } => frame,
    other => panic!("unexpected WorkerToUi message: {other:?}"),
  }
}

fn next_scroll_state_from_frame_ready(
  rx: &impl support::RecvTimeout<WorkerToUi>,
  tab_id: TabId,
) -> ScrollState {
  // `WorkerToUi::ScrollStateUpdated` may be emitted *before* the corresponding `FrameReady` (early
  // scroll acknowledgement), or not at all (e.g. explicit repaint after a no-op scroll). The scroll
  // state embedded in `RenderedFrame` is authoritative for what was actually painted.
  next_frame_ready(rx, tab_id).scroll_state
}

#[test]
fn overscroll_behavior_contain_prevents_scroll_chaining_to_viewport() {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
  let _lock = super::stage_listener_test_lock();

  let site = support::TempSite::new();
  let url = site.write(
    "index.html",
    r#"<!doctype html>
<html>
  <head>
    <meta charset="utf-8">
    <style>
      html, body { margin: 0; padding: 0; }
      .scroller {
        width: 160px;
        height: 60px;
        overflow-y: scroll;
        border: 1px solid black;
      }
      .scroller > .content {
        height: 400px;
        background: linear-gradient(#eee, #ccc);
      }
      #contain {
        overscroll-behavior-y: contain;
      }
      .spacer { height: 2000px; }
    </style>
  </head>
  <body>
    <div id="contain" class="scroller"><div class="content"></div></div>
    <div id="auto" class="scroller"><div class="content"></div></div>
    <div class="spacer"></div>
  </body>
</html>
"#,
  );

  let handle =
    spawn_ui_worker("fastr-ui-worker-overscroll-behavior").expect("spawn ui worker overscroll");
  let (ui_tx, ui_rx, join) = handle.split();
  let tab_id = TabId::new();

  ui_tx
    .send(support::create_tab_msg(tab_id, None))
    .expect("CreateTab");
  ui_tx
    .send(support::viewport_changed_msg(tab_id, (240, 200), 1.0))
    .expect("ViewportChanged");
  ui_tx
    .send(support::navigate_msg(
      tab_id,
      url.clone(),
      NavigationReason::TypedUrl,
    ))
    .expect("Navigate");

  let committed_url = next_navigation_committed(&ui_rx, tab_id);
  assert_eq!(
    committed_url, url,
    "expected committed URL to match navigation URL"
  );

  // The worker does not guarantee an initial `ScrollStateUpdated` during navigation. Use the first
  // painted frame as our baseline scroll state.
  let baseline_viewport_y = next_scroll_state_from_frame_ready(&ui_rx, tab_id).viewport.y;
  assert!(
    baseline_viewport_y.abs() < 1e-3,
    "expected initial viewport scroll y≈0, got {baseline_viewport_y}"
  );

  // ---------------------------------------------------------------------------
  // Scroller with `overscroll-behavior: contain` should NOT scroll-chain to the viewport.
  // ---------------------------------------------------------------------------

  // Pointer inside the first scroller (at the top of the page).
  let contain_pointer = (10.0, 10.0);
  ui_tx
    .send(support::scroll_msg(
      tab_id,
      (0.0, 10_000.0),
      Some(contain_pointer),
    ))
    .expect("Scroll contain scroller to max");
  let contain_scrolled = next_scroll_state_from_frame_ready(&ui_rx, tab_id);

  assert!(
    (contain_scrolled.viewport.y - baseline_viewport_y).abs() < 1.0,
    "expected overscroll-behavior: contain to prevent chaining; viewport y was {} then {}",
    baseline_viewport_y,
    contain_scrolled.viewport.y
  );
  assert!(
    !contain_scrolled.elements.is_empty(),
    "expected element scroll offsets after scrolling #contain, got {:?}",
    contain_scrolled.elements
  );

  let (contain_box_id, contain_offset) = contain_scrolled
    .elements
    .iter()
    .max_by(|a, b| a.1.y.total_cmp(&b.1.y))
    .map(|(&id, &pt)| (id, pt))
    .expect("expected a scroll offset entry for #contain");
  let max_scroll_y = contain_offset.y;
  assert!(
    max_scroll_y > 0.0,
    "expected #contain to scroll vertically, got offset {contain_offset:?}"
  );

  // Overscroll while already at max: containment should prevent chaining to the viewport.
  //
  // When containment works, overscrolling a maxed-out element is a no-op (no element scroll + no
  // viewport scroll), so the worker may not produce a new `FrameReady` on the `UiToWorker::Scroll`
  // alone. Request an explicit repaint so the test can observe the resulting state via the
  // authoritative `RenderedFrame.scroll_state` (a `ScrollStateUpdated` may arrive early or be
  // absent entirely).
  ui_tx
    .send(support::scroll_msg(
      tab_id,
      (0.0, 10_000.0),
      Some(contain_pointer),
    ))
    .expect("Scroll contain scroller beyond max");
  ui_tx
    .send(support::request_repaint(tab_id, RepaintReason::Explicit))
    .expect("RequestRepaint after overscroll contain");
  let contain_scrolled_again = next_scroll_state_from_frame_ready(&ui_rx, tab_id);
  let contain_offset_again = contain_scrolled_again
    .elements
    .get(&contain_box_id)
    .copied()
    .unwrap_or(Point::ZERO);
  assert!(
    (contain_scrolled_again.viewport.y - baseline_viewport_y).abs() < 1.0,
    "expected viewport scroll to remain unchanged when overscrolling contained scroller; was {} then {}",
    baseline_viewport_y,
    contain_scrolled_again.viewport.y
  );
  assert!(
    (contain_offset_again.y - contain_offset.y).abs() < 1.0,
    "expected #contain scroll offset to remain stable when overscrolling; was {:?} then {:?}",
    contain_offset,
    contain_offset_again
  );

  // ---------------------------------------------------------------------------
  // Default scroller (`overscroll-behavior: auto`) SHOULD scroll-chain to the viewport at max.
  // ---------------------------------------------------------------------------

  // Pointer inside the second scroller. The first scroller is 60px tall (plus borders), so y=80 is
  // safely inside the second.
  let auto_pointer = (10.0, 80.0);

  // Scroll the auto scroller to (approximately) its max without overshooting: use the max scroll
  // distance observed from the contain scroller (identical geometry/content).
  ui_tx
    .send(support::scroll_msg(
      tab_id,
      (0.0, max_scroll_y),
      Some(auto_pointer),
    ))
    .expect("Scroll auto scroller to max");
  let auto_scrolled = next_scroll_state_from_frame_ready(&ui_rx, tab_id);
  let viewport_after_auto_to_max = auto_scrolled.viewport.y;
  assert!(
    (viewport_after_auto_to_max - baseline_viewport_y).abs() < 1.0,
    "expected viewport scroll to remain ~unchanged while auto scroller still has room; was {} then {}",
    baseline_viewport_y,
    viewport_after_auto_to_max
  );

  let (auto_box_id, auto_offset) = auto_scrolled
    .elements
    .iter()
    .filter_map(|(&id, &offset)| {
      let prev = contain_scrolled_again
        .elements
        .get(&id)
        .copied()
        .unwrap_or(Point::ZERO);
      (offset.y > prev.y + 1.0).then_some((id, offset))
    })
    .max_by(|a, b| a.1.y.total_cmp(&b.1.y))
    .expect("expected #auto element scroll offset to increase");
  assert_ne!(
    auto_box_id, contain_box_id,
    "expected #auto and #contain to be distinct scroll containers"
  );
  assert!(
    (auto_offset.y - max_scroll_y).abs() < 2.0,
    "expected #auto to reach the same max scroll y as #contain ({}), got {:?}",
    max_scroll_y,
    auto_offset
  );

  // Overscroll the auto scroller: leftover delta should propagate to the viewport.
  ui_tx
    .send(support::scroll_msg(
      tab_id,
      (0.0, 200.0),
      Some(auto_pointer),
    ))
    .expect("Scroll auto scroller beyond max (should chain)");
  let auto_overscrolled = next_scroll_state_from_frame_ready(&ui_rx, tab_id);
  assert!(
    auto_overscrolled.viewport.y > viewport_after_auto_to_max + 10.0,
    "expected scroll chaining to increase viewport scroll when overscrolling auto scroller; viewport y was {} then {}",
    viewport_after_auto_to_max,
    auto_overscrolled.viewport.y
  );
  assert!(
    (auto_overscrolled
      .elements
      .get(&auto_box_id)
      .copied()
      .unwrap_or(Point::ZERO)
      .y
      - auto_offset.y)
      .abs()
      < 2.0,
    "expected #auto to remain at its max scroll offset while chaining to the viewport"
  );

  drop(ui_tx);
  join.join().expect("join ui worker");
}
