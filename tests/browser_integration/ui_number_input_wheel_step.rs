#![cfg(feature = "browser_ui")]

use super::support;
use fastrender::ui::messages::{
  PointerButton, PointerModifiers, RepaintReason, TabId, UiToWorker, WorkerToUi,
};
use fastrender::ui::BrowserTabController;
use fastrender::Result;

fn extract_frame(messages: Vec<WorkerToUi>) -> Option<fastrender::ui::messages::RenderedFrame> {
  messages.into_iter().rev().find_map(|msg| match msg {
    WorkerToUi::FrameReady { frame, .. } => Some(frame),
    _ => None,
  })
}

fn find_node_by_id<'a>(
  root: &'a fastrender::dom::DomNode,
  id: &str,
) -> Option<&'a fastrender::dom::DomNode> {
  let mut stack = vec![root];
  while let Some(node) = stack.pop() {
    if node.get_attribute_ref("id") == Some(id) {
      return Some(node);
    }
    for child in node.children.iter().rev() {
      stack.push(child);
    }
  }
  None
}

#[test]
fn browser_tab_controller_wheel_steps_focused_number_input_without_scrolling() -> Result<()> {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
  let _lock = super::stage_listener_test_lock();

  let tab_id = TabId(1);
  let viewport_css = (200, 140);
  let url = "https://example.com/index.html";

  let html = r#"<!doctype html>
    <html>
      <head>
        <meta charset="utf-8">
        <style>
          html, body { margin: 0; padding: 0; }
          #n { position: absolute; left: 0; top: 0; width: 120px; height: 32px; border: 0; padding: 0; }
          /* Ensure the page is scrollable so a wheel event would normally scroll the viewport. */
          #spacer { height: 2000px; }
        </style>
      </head>
      <body>
        <input id="n" type="number" value="1" min="0">
        <div id="spacer"></div>
      </body>
    </html>
  "#;

  let mut controller = BrowserTabController::from_html_with_renderer(
    support::deterministic_renderer(),
    tab_id,
    html,
    url,
    viewport_css,
    1.0,
  )?;

  // Initial paint to populate cached layout artifacts used for hit-testing.
  let initial = controller.handle_message(UiToWorker::RequestRepaint {
    tab_id,
    reason: RepaintReason::Explicit,
  })?;
  assert!(
    initial
      .iter()
      .any(|msg| matches!(msg, WorkerToUi::FrameReady { .. })),
    "expected initial repaint to produce FrameReady",
  );

  // Focus the number input by clicking it.
  let _ = controller.handle_message(UiToWorker::PointerDown {
    tab_id,
    pos_css: (10.0, 10.0),
    button: PointerButton::Primary,
    modifiers: PointerModifiers::NONE,
    click_count: 1,
  })?;
  let _ = controller.handle_message(UiToWorker::PointerUp {
    tab_id,
    pos_css: (10.0, 10.0),
    button: PointerButton::Primary,
    modifiers: PointerModifiers::NONE,
  })?;

  let scroll_before = controller.scroll_state().viewport;

  // Wheel down over the focused input: should step the value down (1 → 0) and *not* scroll.
  let out = controller.handle_message(UiToWorker::Scroll {
    tab_id,
    delta_css: (0.0, 50.0),
    pointer_css: Some((10.0, 10.0)),
  })?;

  // Stepping should trigger a repaint (value is observable via attribute selectors).
  assert!(
    extract_frame(out).is_some(),
    "expected wheel-stepping a number input to produce FrameReady"
  );

  let value = find_node_by_id(controller.document().dom(), "n")
    .and_then(|node| node.get_attribute_ref("value"))
    .unwrap_or("");
  assert_eq!(value, "0", "expected wheel to step input value");

  let scroll_after = controller.scroll_state().viewport;
  assert_eq!(
    scroll_after, scroll_before,
    "expected wheel stepping to not update viewport scroll"
  );

  // As a sanity check, the page should still be scrollable (root scroll bounds > 0).
  if let Some(prepared) = controller.document().prepared() {
    let viewport = fastrender::geometry::Size::new(viewport_css.0 as f32, viewport_css.1 as f32);
    let bounds =
      fastrender::scroll::build_scroll_chain(&prepared.fragment_tree().root, viewport, &[])
        .first()
        .map(|s| s.bounds);
    let max_y = bounds.map(|b| b.max_y).unwrap_or(0.0);
    assert!(
      max_y > 0.0,
      "expected test fixture to have non-zero root scroll range"
    );
  }

  Ok(())
}
