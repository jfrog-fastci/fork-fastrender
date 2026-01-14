#![cfg(feature = "browser_ui")]

use super::support;
use fastrender::interaction::KeyAction;
use fastrender::interaction::dom_index::DomIndex;
use fastrender::ui::messages::{RepaintReason, TabId};
use fastrender::ui::BrowserTabController;
use fastrender::Result;

fn dom_preorder_id(dom: &fastrender::dom::DomNode, element_id: &str) -> usize {
  let mut clone = dom.clone();
  let index = DomIndex::build(&mut clone);
  *index
    .id_by_element_id
    .get(element_id)
    .unwrap_or_else(|| panic!("expected element with id={element_id:?}"))
}

#[test]
fn page_up_down_key_actions_scroll_viewport_when_nothing_is_focused() -> Result<()> {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
  let _lock = super::stage_listener_test_lock();

  let tab_id = TabId(1);
  // Use a round height so viewport-height * 0.9 is easy to reason about (100 -> 90).
  let viewport_css = (200, 100);
  let url = "https://example.com/index.html";

  let html = r#"<!doctype html>
    <html>
      <head>
        <meta charset="utf-8">
        <style>
          html, body { margin: 0; padding: 0; }
          .spacer { height: 4000px; background: linear-gradient(#eee, #ccc); }
        </style>
      </head>
      <body>
        <div class="spacer">scroll</div>
      </body>
    </html>"#;

  let mut controller = BrowserTabController::from_html_with_renderer(
    support::deterministic_renderer(),
    tab_id,
    html,
    url,
    viewport_css,
    1.0,
  )?;
  let _ = controller.handle_message(support::request_repaint(tab_id, RepaintReason::Explicit))?;

  assert!(
    controller.interaction_state().focused.is_none(),
    "fixture should start with no focused element"
  );

  let y0 = controller.scroll_state().viewport.y;
  assert!(
    y0.abs() < 1e-3,
    "expected initial scroll y to start at 0, got {y0}"
  );

  let step_y = (viewport_css.1 as f32) * 0.9;

  let _ = controller.handle_message(support::key_action(tab_id, KeyAction::PageDown))?;
  let y1 = controller.scroll_state().viewport.y;
  assert!(
    (y1 - step_y).abs() < 1.0,
    "expected PageDown to scroll by ~{step_y} (0.9*viewport height), got {y1}"
  );

  let _ = controller.handle_message(support::key_action(tab_id, KeyAction::PageUp))?;
  let y2 = controller.scroll_state().viewport.y;
  assert!(
    y2 <= 1.0,
    "expected PageUp to scroll back toward the top (clamped), got {y2}"
  );

  // Clamp at the top when already at y=0.
  let _ = controller.handle_message(support::key_action(tab_id, KeyAction::PageUp))?;
  let y3 = controller.scroll_state().viewport.y;
  assert!(
    y3 <= 1.0,
    "expected PageUp at top to remain clamped near 0, got {y3}"
  );

  Ok(())
}

#[test]
fn video_controls_consume_scroll_shortcuts_in_tab_controller() -> Result<()> {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
  let _lock = super::stage_listener_test_lock();

  let tab_id = TabId(1);
  let viewport_css = (200, 100);

  let html = r#"<!doctype html>
    <html>
      <head>
        <meta charset="utf-8">
        <style>
          html, body { margin: 0; padding: 0; }
          #v { display: block; width: 200px; height: 40px; }
          #spacer { height: 5000px; }
        </style>
      </head>
      <body>
        <video id="v" controls tabindex="0"></video>
        <div id="spacer"></div>
      </body>
    </html>"#;

  let mut controller = BrowserTabController::from_html_with_renderer(
    support::deterministic_renderer(),
    tab_id,
    html,
    "https://example.com/index.html",
    viewport_css,
    1.0,
  )?;
  let _ = controller.handle_message(support::request_repaint(tab_id, RepaintReason::Explicit))?;

  // Focus the video element with Tab.
  let video_node_id = dom_preorder_id(controller.document().dom(), "v");
  let _ = controller.handle_message(support::key_action(tab_id, KeyAction::Tab))?;
  assert_eq!(
    controller.interaction_state().focused,
    Some(video_node_id),
    "expected Tab to focus <video id=v>"
  );

  // When video controls are focused, common page scroll shortcuts should be consumed rather than
  // interpreted as viewport scrolling.
  let baseline = controller.scroll_state().viewport;
  for key in [
    KeyAction::Space,
    KeyAction::ArrowDown,
    KeyAction::PageDown,
    KeyAction::End,
  ] {
    let _ = controller.handle_message(support::key_action(tab_id, key))?;
    assert!(
      (controller.scroll_state().viewport.x - baseline.x).abs() < 1e-3
        && (controller.scroll_state().viewport.y - baseline.y).abs() < 1e-3,
      "expected viewport to remain unchanged after {key:?}, baseline={baseline:?}, got {:?}",
      controller.scroll_state().viewport
    );
  }

  // Scroll down so ArrowUp/PageUp/Home would normally move the viewport, then ensure they are
  // consumed as well.
  let _ = controller.handle_message(support::scroll_msg(tab_id, (0.0, 200.0), None))?;
  let scrolled = controller.scroll_state().viewport;
  assert!(
    scrolled.y > baseline.y + 50.0,
    "expected baseline scroll to move viewport down, got baseline={baseline:?} now={scrolled:?}"
  );

  for key in [KeyAction::ArrowUp, KeyAction::PageUp, KeyAction::Home] {
    let _ = controller.handle_message(support::key_action(tab_id, key))?;
    assert!(
      (controller.scroll_state().viewport.x - scrolled.x).abs() < 1e-3
        && (controller.scroll_state().viewport.y - scrolled.y).abs() < 1e-3,
      "expected viewport to remain unchanged after {key:?}, baseline={scrolled:?}, got {:?}",
      controller.scroll_state().viewport
    );
  }

  Ok(())
}
