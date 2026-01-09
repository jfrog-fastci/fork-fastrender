#![cfg(feature = "browser_ui")]

use fastrender::api::{FastRenderConfig, FastRenderFactory, FastRenderPoolConfig};
use fastrender::interaction::KeyAction;
use fastrender::render_control::StageHeartbeat;
use fastrender::text::font_db::FontConfig;
use fastrender::ui::cancel::CancelGens;
use fastrender::ui::{
  spawn_browser_render_thread, NavigationReason, PointerButton, TabId, UiToWorker, WorkerToUi,
};
use super::support::{
  create_tab_msg_with_cancel, navigate_msg, scroll_msg, viewport_changed_msg, DEFAULT_TIMEOUT,
};
use std::time::Duration;

fn factory_for_tests() -> FastRenderFactory {
  let renderer = FastRenderConfig::default().with_font_sources(FontConfig::bundled_only());
  FastRenderFactory::with_config(FastRenderPoolConfig::new().with_renderer_config(renderer)).unwrap()
}

struct TestRenderDelayGuard;

impl TestRenderDelayGuard {
  fn set(ms: Option<u64>) -> Self {
    fastrender::render_control::set_test_render_delay_ms(ms);
    Self
  }
}

impl Drop for TestRenderDelayGuard {
  fn drop(&mut self) {
    fastrender::render_control::set_test_render_delay_ms(None);
  }
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

  // Wait for the initial frame.
  while let Ok(msg) = rx.recv_timeout(Duration::from_secs(10)) {
    if matches!(msg, WorkerToUi::FrameReady { .. }) {
      break;
    }
  }

  tx.send(scroll_msg(tab_id, (0.0, 200.0), None)).unwrap();

  let mut saw_scroll = false;
  let mut saw_frame = false;
  while let Ok(msg) = rx.recv_timeout(Duration::from_secs(10)) {
    match msg {
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
  let _delay = TestRenderDelayGuard::set(Some(1));

  let factory = factory_for_tests();
  let (tx, rx, handle) = spawn_browser_render_thread(factory).unwrap();

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
        // Once the cancellation signal is sent, disable the synthetic slowdown so we can complete
        // the follow-up navigation quickly.
        fastrender::render_control::set_test_render_delay_ms(None);
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
  while let Ok(msg) = rx.recv_timeout(Duration::from_secs(10)) {
    if matches!(msg, WorkerToUi::FrameReady { .. }) {
      break;
    }
  }

  let pos_css = (5.0, 5.0);
  tx.send(UiToWorker::PointerDown {
    tab_id,
    pos_css,
    button: PointerButton::Primary,
  })
  .unwrap();
  tx.send(UiToWorker::PointerUp {
    tab_id,
    pos_css,
    button: PointerButton::Primary,
  })
  .unwrap();
  tx.send(UiToWorker::TextInput {
    tab_id,
    text: "a".to_string(),
  })
  .unwrap();
  tx.send(UiToWorker::KeyAction {
    tab_id,
    key: KeyAction::Enter,
  })
  .unwrap();

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
