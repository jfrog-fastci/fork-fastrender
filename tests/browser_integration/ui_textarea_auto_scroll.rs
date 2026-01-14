#![cfg(feature = "browser_ui")]

use super::support::{
  create_tab_msg, key_action, navigate_msg, pointer_down, pointer_up, viewport_changed_msg,
  DEFAULT_TIMEOUT,
};
use super::worker_harness::{WorkerHarness, WorkerToUiEvent};
use fastrender::interaction::KeyAction;
use fastrender::ui::messages::{NavigationReason, PointerButton, TabId};
use tempfile::tempdir;

#[test]
fn textarea_auto_scroll_keeps_caret_visible_while_moving() {
  let _lock = super::stage_listener_test_lock();
  let h = WorkerHarness::spawn();

  let tab_id = TabId::new();
  let viewport_css = (220, 140);
  let dpr = 1.0;

  let dir = tempdir().expect("temp dir");
  let html = r#"<!doctype html>
    <html>
      <head>
        <meta charset="utf-8">
        <style>
          html, body { margin: 0; padding: 0; }
          #ta {
            position: absolute;
            left: 0;
            top: 0;
            width: 200px;
            height: 40px;
            font-family: "Noto Sans Mono";
            font-size: 16px;
            line-height: 20px;
          }
        </style>
      </head>
      <body>
        <textarea id="ta">line1
line2
line3
line4
line5
line6
line7
line8
line9
line10</textarea>
      </body>
    </html>
  "#;
  std::fs::write(dir.path().join("index.html"), html).expect("write html");
  let url = url::Url::from_file_path(dir.path().join("index.html"))
    .expect("file URL")
    .to_string();

  h.send(create_tab_msg(tab_id, None));
  h.send(viewport_changed_msg(tab_id, viewport_css, dpr));
  h.send(navigate_msg(tab_id, url, NavigationReason::TypedUrl));

  h.wait_for_event(
    DEFAULT_TIMEOUT,
    |ev| matches!(ev, WorkerToUiEvent::NavigationCommitted { tab_id: t, .. } if *t == tab_id),
  );
  h.wait_for_frame(tab_id, DEFAULT_TIMEOUT);
  // Drain the initial post-navigation scroll notification.
  h.wait_for_event(
    DEFAULT_TIMEOUT,
    |ev| matches!(ev, WorkerToUiEvent::ScrollStateUpdated { tab_id: t, .. } if *t == tab_id),
  );

  // Focus the textarea and place the caret on the first line.
  let click = (10.0, 10.0);
  h.send(pointer_down(tab_id, click, PointerButton::Primary));
  h.send(pointer_up(tab_id, click, PointerButton::Primary));
  h.wait_for_frame(tab_id, DEFAULT_TIMEOUT);

  // Move the caret down far enough that the textarea must scroll.
  for _ in 0..10 {
    h.send(key_action(tab_id, KeyAction::ArrowDown));
  }

  let down_events = h.wait_for_event(DEFAULT_TIMEOUT, |ev| match ev {
    WorkerToUiEvent::ScrollStateUpdated { tab_id: t, scroll } if *t == tab_id => {
      scroll.elements.values().any(|offset| offset.y > 0.0)
    }
    _ => false,
  });
  let scrolled_y_down = match down_events.last().expect("ScrollStateUpdated event") {
    WorkerToUiEvent::ScrollStateUpdated { scroll, .. } => scroll
      .elements
      .values()
      .map(|offset| offset.y)
      .fold(0.0, f32::max),
    other => panic!("expected ScrollStateUpdated, got {other:?}"),
  };
  assert!(
    scrolled_y_down > 0.0,
    "expected textarea scroll_y > 0 after moving caret down, got {scrolled_y_down}"
  );

  // Move the caret back up to the top and ensure the textarea scroll offset returns to zero.
  for _ in 0..20 {
    h.send(key_action(tab_id, KeyAction::ArrowUp));
  }

  let up_events = h.wait_for_event(DEFAULT_TIMEOUT, |ev| match ev {
    WorkerToUiEvent::ScrollStateUpdated { tab_id: t, scroll } if *t == tab_id => {
      scroll.elements.is_empty()
    }
    _ => false,
  });
  let up_scroll = match up_events.last().expect("ScrollStateUpdated event") {
    WorkerToUiEvent::ScrollStateUpdated { scroll, .. } => scroll,
    other => panic!("expected ScrollStateUpdated, got {other:?}"),
  };
  assert!(
    up_scroll.elements.is_empty(),
    "expected textarea scroll state to return to zero after moving caret up, got {up_scroll:?}"
  );
}
