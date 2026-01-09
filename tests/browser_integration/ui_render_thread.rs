#![cfg(feature = "browser_ui")]

use fastrender::api::{FastRenderConfig, FastRenderFactory, FastRenderPoolConfig};
use fastrender::interaction::KeyAction;
use fastrender::render_control::StageHeartbeat;
use fastrender::text::font_db::FontConfig;
use fastrender::tree::box_tree::SelectItem;
use fastrender::ui::cancel::CancelGens;
use fastrender::ui::{
  spawn_browser_render_thread, spawn_browser_render_thread_for_test, NavigationReason, PointerButton,
  RenderedFrame, TabId, UiToWorker, WorkerToUi,
};
use super::support::{
  create_tab_msg_with_cancel, key_action, navigate_msg, pointer_down, pointer_up, scroll_msg,
  text_input, viewport_changed_msg, TempSite, DEFAULT_TIMEOUT,
};
use std::time::Duration;

fn factory_for_tests() -> FastRenderFactory {
  let renderer = FastRenderConfig::default().with_font_sources(FontConfig::bundled_only());
  FastRenderFactory::with_config(FastRenderPoolConfig::new().with_renderer_config(renderer)).unwrap()
}

fn rgba_at_css(frame: &RenderedFrame, x_css: u32, y_css: u32) -> [u8; 4] {
  let x_px = ((x_css as f32) * frame.dpr).round() as u32;
  let y_px = ((y_css as f32) * frame.dpr).round() as u32;
  super::support::rgba_at(&frame.pixmap, x_px, y_px)
}

#[test]
fn about_newtab_navigation_yields_frame_and_no_fetch_stages() {
  let _lock = super::stage_listener_test_lock();

  let factory = factory_for_tests();
  let (tx, rx, handle) = spawn_browser_render_thread(factory).unwrap();

  let tab_id = TabId(1);
  let cancel = CancelGens::new();
  tx.send(create_tab_msg_with_cancel(tab_id, None, cancel))
    .unwrap();
  tx.send(viewport_changed_msg(tab_id, (64, 64), 1.0))
    .unwrap();
  tx.send(navigate_msg(
    tab_id,
    "about:newtab".to_string(),
    NavigationReason::TypedUrl,
  ))
  .unwrap();

  let mut stages = Vec::new();
  let mut saw_frame = false;
  while let Ok(msg) = rx.recv_timeout(Duration::from_secs(10)) {
    match msg {
      WorkerToUi::Stage { stage, .. } => stages.push(stage),
      WorkerToUi::FrameReady { .. } => {
        saw_frame = true;
        break;
      }
      _ => {}
    }
  }

  drop(tx);
  handle.join().unwrap();

  assert!(saw_frame, "expected FrameReady message");
  assert!(
    !stages.iter().any(|stage| matches!(
      stage,
      StageHeartbeat::ReadCache | StageHeartbeat::FollowRedirects
    )),
    "about:newtab should not perform document fetch stages (got {stages:?})"
  );
}

#[test]
fn scroll_produces_scroll_update_and_frame() {
  let _lock = super::stage_listener_test_lock();

  let factory = factory_for_tests();
  let (tx, rx, handle) = spawn_browser_render_thread(factory).unwrap();

  let tab_id = TabId(1);
  let cancel = CancelGens::new();
  tx.send(create_tab_msg_with_cancel(tab_id, None, cancel))
    .unwrap();
  tx.send(viewport_changed_msg(tab_id, (64, 64), 1.0))
    .unwrap();
  tx.send(navigate_msg(
    tab_id,
    "about:test-scroll".to_string(),
    NavigationReason::TypedUrl,
  ))
  .unwrap();

  // Wait for the initial frame before issuing scroll commands so the document has cached layout.
  assert!(
    super::support::recv_until(&rx, DEFAULT_TIMEOUT * 2, |msg| {
      matches!(msg, WorkerToUi::FrameReady { .. })
    })
    .is_some(),
    "timed out waiting for initial FrameReady"
  );

  tx.send(scroll_msg(tab_id, (0.0, 200.0), None)).unwrap();

  let mut saw_scroll = false;
  let mut saw_frame = false;
  let deadline = std::time::Instant::now() + DEFAULT_TIMEOUT * 2;
  while std::time::Instant::now() < deadline {
    let remaining = deadline.saturating_duration_since(std::time::Instant::now());
    match rx.recv_timeout(remaining.min(Duration::from_millis(200))) {
      Ok(msg) => match msg {
        WorkerToUi::ScrollStateUpdated { scroll, .. } => {
          if scroll.viewport.y > 0.0 {
            saw_scroll = true;
          }
        }
        WorkerToUi::FrameReady { frame, .. } => {
          if frame.scroll_state.viewport.y > 0.0 {
            saw_frame = true;
          }
        }
        _ => {}
      },
      Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
      Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
    }
    if saw_scroll && saw_frame {
      break;
    }
  }

  drop(tx);
  handle.join().unwrap();

  assert!(saw_scroll, "expected non-zero ScrollStateUpdated");
  assert!(saw_frame, "expected FrameReady after scroll");
}

#[test]
fn navigation_cancellation_drops_stale_frame() {
  let _lock = super::stage_listener_test_lock();

  let factory = factory_for_tests();
  // Slow down render stages on this worker thread to make cancellation deterministic without
  // mutating the process-global render-delay environment variable.
  let (tx, rx, handle) = spawn_browser_render_thread_for_test(factory, Some(1)).unwrap();

  let tab_id = TabId(1);
  let cancel = CancelGens::new();
  tx.send(create_tab_msg_with_cancel(tab_id, None, cancel.clone()))
    .unwrap();
  tx.send(viewport_changed_msg(tab_id, (128, 128), 1.0))
    .unwrap();

  let first_url = "about:test-heavy".to_string();
  let second_url = "about:blank".to_string();

  tx.send(navigate_msg(tab_id, first_url.clone(), NavigationReason::TypedUrl))
    .unwrap();

  let mut started_first = false;
  let mut sent_second = false;
  let mut last_committed: Option<String> = None;
  let mut saw_second_frame = false;
  let mut saw_first_frame = false;

  while let Ok(msg) = rx.recv_timeout(DEFAULT_TIMEOUT) {
    match msg {
      WorkerToUi::NavigationStarted { url, .. } if url == first_url => {
        started_first = true;
      }
      WorkerToUi::Stage { .. } if started_first && !sent_second => {
        // Simulate UI-driven cancellation while the worker is blocked in the first navigation.
        cancel.bump_nav();
        tx.send(navigate_msg(tab_id, second_url.clone(), NavigationReason::TypedUrl))
          .unwrap();
        sent_second = true;
      }
      WorkerToUi::NavigationCommitted { url, .. } => {
        last_committed = Some(url);
      }
      WorkerToUi::FrameReady { .. } => {
        if last_committed.as_deref() == Some(second_url.as_str()) {
          saw_second_frame = true;
          break;
        }
        if last_committed.as_deref() == Some(first_url.as_str()) {
          saw_first_frame = true;
          break;
        }
      }
      _ => {}
    }
  }

  drop(tx);
  handle.join().unwrap();

  assert!(started_first, "expected to observe NavigationStarted for the first URL");
  assert!(
    sent_second,
    "expected to observe a stage heartbeat during the first navigation"
  );
  assert!(saw_second_frame, "expected FrameReady for the second navigation");
  assert!(
    !saw_first_frame,
    "expected no FrameReady for the cancelled first navigation"
  );
}

#[test]
fn enter_submits_focused_text_input_form() {
  let _lock = super::stage_listener_test_lock();

  let factory = factory_for_tests();
  let (tx, rx, handle) = spawn_browser_render_thread(factory).unwrap();

  let tab_id = TabId(1);
  let cancel = CancelGens::new();
  tx.send(create_tab_msg_with_cancel(tab_id, None, cancel))
    .unwrap();
  tx.send(viewport_changed_msg(tab_id, (200, 120), 1.0))
    .unwrap();
  tx.send(navigate_msg(
    tab_id,
    "about:test-form".to_string(),
    NavigationReason::TypedUrl,
  ))
  .unwrap();

  // Wait for the initial frame so the document has cached layout for hit-testing.
  assert!(
    super::support::recv_until(&rx, DEFAULT_TIMEOUT * 2, |msg| {
      matches!(msg, WorkerToUi::FrameReady { .. })
    })
    .is_some(),
    "timed out waiting for initial FrameReady"
  );

  let pos_css = (5.0, 5.0);
  tx.send(pointer_down(tab_id, pos_css, PointerButton::Primary))
    .unwrap();
  tx.send(pointer_up(tab_id, pos_css, PointerButton::Primary))
    .unwrap();
  tx.send(text_input(tab_id, "a")).unwrap();
  tx.send(key_action(tab_id, KeyAction::Enter)).unwrap();

  let expected_url = "about:test-form?q=a&go=1";
  let mut saw_commit = false;
  while let Ok(msg) = rx.recv_timeout(Duration::from_secs(10)) {
    if let WorkerToUi::NavigationCommitted { url, .. } = msg {
      if url == expected_url {
        saw_commit = true;
        break;
      }
    }
  }

  drop(tx);
  handle.join().unwrap();

  assert!(
    saw_commit,
    "expected NavigationCommitted for {expected_url} (keyboard submit)"
  );
}

#[test]
fn select_dropdown_choose_updates_dom_and_repaints() {
  let _lock = super::stage_listener_test_lock();

  let site = TempSite::new();
  let url = site.write(
    "index.html",
    r#"<!doctype html>
<html>
  <head>
    <style>
      html, body { margin: 0; padding: 0; }
      #sel { position: absolute; left: 0; top: 0; width: 120px; height: 24px; }
      #box { position: absolute; left: 0; top: 40px; width: 64px; height: 64px; background: rgb(255,0,0); }
      select:has(option[selected][value="b"]) + #box { background: rgb(0,255,0); }
    </style>
  </head>
  <body>
    <select id="sel">
      <option value="a" selected>Red</option>
      <option value="b">Green</option>
    </select>
    <div id="box"></div>
  </body>
</html>
"#,
  );

  let factory = factory_for_tests();
  let (tx, rx, handle) = spawn_browser_render_thread(factory).unwrap();

  let tab_id = TabId(1);
  let cancel = CancelGens::new();
  tx.send(create_tab_msg_with_cancel(tab_id, None, cancel))
    .unwrap();
  tx.send(viewport_changed_msg(tab_id, (160, 160), 1.0))
    .unwrap();
  tx.send(navigate_msg(tab_id, url, NavigationReason::TypedUrl))
    .unwrap();

  let frame = match super::support::recv_for_tab(&rx, tab_id, DEFAULT_TIMEOUT, |msg| {
    matches!(msg, WorkerToUi::FrameReady { .. })
  }) {
    Some(WorkerToUi::FrameReady { frame, .. }) => frame,
    Some(other) => panic!("expected FrameReady, got {other:?}"),
    None => panic!("timed out waiting for FrameReady"),
  };
  assert_eq!(rgba_at_css(&frame, 10, 50), [255, 0, 0, 255]);

  while rx.try_recv().is_ok() {}

  // Click the <select> to open the dropdown.
  tx.send(UiToWorker::PointerDown {
    tab_id,
    pos_css: (10.0, 10.0),
    button: PointerButton::Primary,
  })
  .unwrap();
  tx.send(UiToWorker::PointerUp {
    tab_id,
    pos_css: (10.0, 10.0),
    button: PointerButton::Primary,
  })
  .unwrap();

  let msg = super::support::recv_for_tab(&rx, tab_id, DEFAULT_TIMEOUT, |msg| {
    matches!(msg, WorkerToUi::SelectDropdownOpened { .. })
  })
  .expect("expected SelectDropdownOpened message");

  let WorkerToUi::SelectDropdownOpened {
    select_node_id,
    control,
    ..
  } = msg
  else {
    unreachable!("filtered above");
  };

  let option_node_id = control
    .items
    .iter()
    .find_map(|item| match item {
      SelectItem::Option {
        node_id, value, ..
      } if value == "b" => Some(*node_id),
      _ => None,
    })
    .expect("expected option value=b in select control");

  while rx.try_recv().is_ok() {}

  tx.send(UiToWorker::SelectDropdownChoose {
    tab_id,
    select_node_id,
    option_node_id,
  })
  .unwrap();

  let frame = match super::support::recv_for_tab(&rx, tab_id, DEFAULT_TIMEOUT, |msg| {
    matches!(msg, WorkerToUi::FrameReady { .. })
  }) {
    Some(WorkerToUi::FrameReady { frame, .. }) => frame,
    Some(other) => panic!("expected FrameReady, got {other:?}"),
    None => panic!("timed out waiting for FrameReady after SelectDropdownChoose"),
  };
  assert_eq!(rgba_at_css(&frame, 10, 50), [0, 255, 0, 255]);

  drop(tx);
  handle.join().unwrap();
}
