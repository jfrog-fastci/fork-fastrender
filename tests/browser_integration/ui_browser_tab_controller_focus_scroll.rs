#![cfg(feature = "browser_ui")]

use fastrender::interaction::KeyAction;
use fastrender::ui::messages::{PointerButton, RepaintReason, RenderedFrame, TabId, WorkerToUi};
use fastrender::ui::BrowserTabController;
use fastrender::Result;

use super::support::{key_action, pointer_down, pointer_up, request_repaint, deterministic_renderer};

fn extract_frame(messages: Vec<WorkerToUi>) -> Option<RenderedFrame> {
  messages.into_iter().rev().find_map(|msg| match msg {
    WorkerToUi::FrameReady { frame, .. } => Some(frame),
    _ => None,
  })
}

fn has_scroll_update(messages: &[WorkerToUi]) -> bool {
  messages
    .iter()
    .any(|msg| matches!(msg, WorkerToUi::ScrollStateUpdated { .. }))
}

#[test]
fn browser_tab_controller_tab_focus_scrolls_viewport_to_reveal_focused_element() -> Result<()> {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
  let _lock = super::stage_listener_test_lock();

  let tab_id = TabId(1);
  let viewport_css = (200, 200);

  let html = r#"<!doctype html>
    <html>
      <head>
        <style>
          html, body { margin: 0; padding: 0; }
          body { height: 2000px; background: rgb(0,0,0); position: relative; }
          #target {
            position: absolute;
            left: 10px;
            top: 1500px;
            width: 120px;
            height: 30px;
            margin: 0;
            padding: 0;
            border: 0;
            background: rgb(255,0,0);
          }
        </style>
      </head>
      <body>
        <input id="target" value="hello" />
      </body>
    </html>
  "#;

  let mut controller = BrowserTabController::from_html_with_renderer(
    deterministic_renderer(),
    tab_id,
    html,
    "https://example.com/",
    viewport_css,
    1.0,
  )?;

  let initial = controller.handle_message(request_repaint(tab_id, RepaintReason::Explicit))?;
  assert!(
    extract_frame(initial).is_some(),
    "expected initial FrameReady"
  );
  assert!(
    controller.scroll_state().viewport.y.abs() < 1e-3,
    "expected initial scroll y to be at top (got {})",
    controller.scroll_state().viewport.y
  );

  let out = controller.handle_message(key_action(tab_id, KeyAction::Tab))?;
  assert!(
    has_scroll_update(&out),
    "expected Tab focus scroll to emit ScrollStateUpdated"
  );

  let frame = extract_frame(out).expect("expected FrameReady after Tab");
  let scroll_y = frame.scroll_state.viewport.y;
  assert!(
    scroll_y.is_finite() && scroll_y > 0.0,
    "expected scroll y > 0 after Tab focus scroll, got {scroll_y}"
  );

  let viewport_top = scroll_y;
  let viewport_bottom = scroll_y + viewport_css.1 as f32;
  let input_top = 1500.0;
  let input_bottom = input_top + 30.0;
  assert!(
    viewport_top <= input_top && viewport_bottom >= input_bottom,
    "expected focused input [{input_top}, {input_bottom}] to be visible in viewport [{viewport_top}, {viewport_bottom}]",
  );

  Ok(())
}

#[test]
fn browser_tab_controller_tab_focus_scrolls_nested_scroller_to_reveal_focused_element() -> Result<()> {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
  let _lock = super::stage_listener_test_lock();

  let tab_id = TabId(1);
  let viewport_css = (220, 220);

  let html = r#"<!doctype html>
    <html>
      <head>
        <style>
          html, body { margin: 0; padding: 0; }
          #scroller {
            width: 200px;
            height: 100px;
            overflow-y: scroll;
            border: 0;
            background: rgb(0,0,0);
          }
          #content { position: relative; height: 1000px; }
          #target {
            position: absolute;
            left: 10px;
            top: 800px;
            width: 120px;
            height: 30px;
            margin: 0;
            padding: 0;
            border: 0;
            background: rgb(255,0,0);
          }
        </style>
      </head>
      <body>
        <div id="scroller">
          <div id="content">
            <input id="target" value="hello" />
          </div>
        </div>
      </body>
    </html>
  "#;

  let mut controller = BrowserTabController::from_html_with_renderer(
    deterministic_renderer(),
    tab_id,
    html,
    "https://example.com/",
    viewport_css,
    1.0,
  )?;

  let initial = controller.handle_message(request_repaint(tab_id, RepaintReason::Explicit))?;
  assert!(extract_frame(initial).is_some(), "expected initial FrameReady");
  assert!(
    controller.scroll_state().viewport.y.abs() < 1e-3,
    "expected initial viewport scroll to be at top (got {})",
    controller.scroll_state().viewport.y
  );
  assert!(
    controller.scroll_state().elements.is_empty(),
    "expected initial element scroll offsets to be empty"
  );

  let out = controller.handle_message(key_action(tab_id, KeyAction::Tab))?;
  assert!(
    has_scroll_update(&out),
    "expected Tab focus scroll to emit ScrollStateUpdated"
  );

  let frame = extract_frame(out).expect("expected FrameReady after Tab");
  assert!(
    frame.scroll_state.viewport.y.abs() < 1e-3,
    "expected focus scroll to adjust nested scroller, not viewport (got viewport_y={})",
    frame.scroll_state.viewport.y
  );
  assert_eq!(
    frame.scroll_state.elements.len(),
    1,
    "expected exactly one element scroller to be updated"
  );
  let scroll_y = frame
    .scroll_state
    .elements
    .values()
    .next()
    .copied()
    .expect("element scroll offset")
    .y;
  assert!(
    scroll_y.is_finite() && scroll_y > 0.0,
    "expected element scroll y > 0, got {scroll_y}"
  );

  let viewport_top = scroll_y;
  let viewport_bottom = scroll_y + 100.0;
  let input_top = 800.0;
  let input_bottom = input_top + 30.0;
  assert!(
    viewport_top <= input_top && viewport_bottom >= input_bottom,
    "expected focused input [{input_top}, {input_bottom}] to be visible in nested scrollport [{viewport_top}, {viewport_bottom}]",
  );

  Ok(())
}

#[test]
fn browser_tab_controller_click_focus_scrolls_nested_scroller_to_reveal_focused_element() -> Result<()> {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
  let _lock = super::stage_listener_test_lock();

  let tab_id = TabId(1);
  let viewport_css = (220, 220);

  let html = r#"<!doctype html>
    <html>
      <head>
        <style>
          html, body { margin: 0; padding: 0; }
          #scroller {
            width: 200px;
            height: 100px;
            overflow-y: scroll;
            border: 0;
            background: rgb(0,0,0);
          }
          #content { position: relative; height: 1000px; }
          #target {
            position: absolute;
            left: 10px;
            top: 90px;
            width: 120px;
            height: 30px;
            margin: 0;
            padding: 0;
            border: 0;
            background: rgb(255,0,0);
          }
        </style>
      </head>
      <body>
        <div id="scroller">
          <div id="content">
            <input id="target" value="hello" />
          </div>
        </div>
      </body>
    </html>
  "#;

  let mut controller = BrowserTabController::from_html_with_renderer(
    deterministic_renderer(),
    tab_id,
    html,
    "https://example.com/",
    viewport_css,
    1.0,
  )?;

  let initial = controller.handle_message(request_repaint(tab_id, RepaintReason::Explicit))?;
  assert!(extract_frame(initial).is_some(), "expected initial FrameReady");
  assert!(
    controller.scroll_state().elements.is_empty(),
    "expected initial element scroll offsets to be empty"
  );

  let _ = controller.handle_message(pointer_down(tab_id, (20.0, 95.0), PointerButton::Primary))?;
  let out = controller.handle_message(pointer_up(tab_id, (20.0, 95.0), PointerButton::Primary))?;

  assert!(
    has_scroll_update(&out),
    "expected click focus scroll to emit ScrollStateUpdated"
  );

  let frame = extract_frame(out).expect("expected FrameReady after click focus");
  assert_eq!(frame.scroll_state.elements.len(), 1);
  let scroll_y = frame
    .scroll_state
    .elements
    .values()
    .next()
    .copied()
    .expect("element scroll offset")
    .y;
  assert!(
    scroll_y.is_finite() && scroll_y > 0.0,
    "expected element scroll y > 0, got {scroll_y}"
  );

  let viewport_top = scroll_y;
  let viewport_bottom = scroll_y + 100.0;
  let input_top = 90.0;
  let input_bottom = input_top + 30.0;
  assert!(
    viewport_top <= input_top && viewport_bottom >= input_bottom,
    "expected focused input [{input_top}, {input_bottom}] to be visible in nested scrollport [{viewport_top}, {viewport_bottom}]",
  );

  Ok(())
}

#[test]
fn browser_tab_controller_keyboard_scroll_fallback_scrolls_viewport_and_respects_link_focus() -> Result<()> {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
  let _lock = super::stage_listener_test_lock();

  let tab_id = TabId(1);
  // Use a round height so viewport-height * 0.9 is easy to reason about (100 -> 90).
  let viewport_css = (200, 100);

  let html = r#"<!doctype html>
    <html>
      <head>
        <style>
          html, body { margin: 0; padding: 0; }
          body { height: 5000px; background: rgb(0,0,0); }
          #link { display: block; height: 20px; background: rgb(220,220,0); }
          #target { height: 20px; background: rgb(0,0,255); }
        </style>
      </head>
      <body>
        <a id="link" href="#target">top link</a>
        <div style="height: 4800px"></div>
        <div id="target">target</div>
      </body>
    </html>
  "#;

  let mut controller = BrowserTabController::from_html_with_renderer(
    deterministic_renderer(),
    tab_id,
    html,
    "https://example.com/",
    viewport_css,
    1.0,
  )?;

  let initial = controller.handle_message(request_repaint(tab_id, RepaintReason::Explicit))?;
  assert!(extract_frame(initial).is_some(), "expected initial FrameReady");
  assert!(
    controller.scroll_state().viewport.y.abs() < 1e-3,
    "expected initial scroll y to start at 0, got {}",
    controller.scroll_state().viewport.y
  );

  // End should scroll near the bottom when nothing is focused.
  let out = controller.handle_message(key_action(tab_id, KeyAction::End))?;
  assert!(has_scroll_update(&out));
  assert!(
    controller.scroll_state().viewport.y > 3_000.0,
    "expected End to scroll near bottom (got {})",
    controller.scroll_state().viewport.y
  );

  // Home should scroll back to the top.
  let out = controller.handle_message(key_action(tab_id, KeyAction::Home))?;
  assert!(has_scroll_update(&out));
  assert!(
    controller.scroll_state().viewport.y <= 1.0,
    "expected Home to scroll back to the top (got {})",
    controller.scroll_state().viewport.y
  );

  let step_y = (viewport_css.1 as f32) * 0.9;

  // Space should scroll down by ~0.9 * viewport height.
  let out = controller.handle_message(key_action(tab_id, KeyAction::Space))?;
  assert!(has_scroll_update(&out));
  assert!(
    (controller.scroll_state().viewport.y - step_y).abs() < 1.0,
    "expected Space to scroll by ~{step_y}, got {}",
    controller.scroll_state().viewport.y
  );

  // ArrowDown should scroll by 40px.
  let out = controller.handle_message(key_action(tab_id, KeyAction::ArrowDown))?;
  assert!(has_scroll_update(&out));
  assert!(
    (controller.scroll_state().viewport.y - (step_y + 40.0)).abs() < 1.0,
    "expected ArrowDown to scroll by ~40, got {}",
    controller.scroll_state().viewport.y
  );

  // ArrowUp should scroll back by 40px.
  let out = controller.handle_message(key_action(tab_id, KeyAction::ArrowUp))?;
  assert!(has_scroll_update(&out));
  assert!(
    (controller.scroll_state().viewport.y - step_y).abs() < 1.0,
    "expected ArrowUp to scroll back to ~{step_y}, got {}",
    controller.scroll_state().viewport.y
  );

  // Shift+Space should scroll back up by ~0.9 * viewport height.
  let out = controller.handle_message(key_action(tab_id, KeyAction::ShiftSpace))?;
  assert!(has_scroll_update(&out));
  assert!(
    controller.scroll_state().viewport.y <= 1.0,
    "expected Shift+Space to scroll back to the top (got {})",
    controller.scroll_state().viewport.y
  );

  // Focus the link using Tab.
  let _ = controller.handle_message(key_action(tab_id, KeyAction::Tab))?;

  // Space should still scroll when a link is focused.
  let out = controller.handle_message(key_action(tab_id, KeyAction::Space))?;
  assert!(has_scroll_update(&out));
  assert!(
    (controller.scroll_state().viewport.y - step_y).abs() < 1.0,
    "expected Space to scroll by ~{step_y} when a link is focused, got {}",
    controller.scroll_state().viewport.y
  );

  let out = controller.handle_message(key_action(tab_id, KeyAction::ShiftSpace))?;
  assert!(has_scroll_update(&out));
  assert!(
    controller.scroll_state().viewport.y <= 1.0,
    "expected Shift+Space to scroll back to the top when a link is focused (got {})",
    controller.scroll_state().viewport.y
  );

  Ok(())
}
