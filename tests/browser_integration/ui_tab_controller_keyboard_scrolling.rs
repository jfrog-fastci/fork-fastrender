#![cfg(feature = "browser_ui")]

use super::support;
use fastrender::interaction::KeyAction;
use fastrender::ui::messages::{RepaintReason, TabId};
use fastrender::ui::BrowserTabController;
use fastrender::Result;

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

