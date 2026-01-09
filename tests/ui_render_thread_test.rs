#![cfg(feature = "browser_ui")]

use fastrender::api::{FastRenderConfig, FastRenderFactory, FastRenderPoolConfig};
use fastrender::render_control::StageHeartbeat;
use fastrender::text::font_db::FontConfig;
use fastrender::ui::cancel::CancelGens;
use fastrender::ui::{spawn_browser_render_thread, NavigationReason, TabId, UiToWorker, WorkerToUi};
use std::ffi::OsString;
use std::sync::{Mutex, MutexGuard, OnceLock};
use std::time::Duration;

fn test_lock() -> MutexGuard<'static, ()> {
  static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
  LOCK.get_or_init(|| Mutex::new(())).lock().unwrap()
}

fn factory_for_tests() -> FastRenderFactory {
  let renderer = FastRenderConfig::default().with_font_sources(FontConfig::bundled_only());
  FastRenderFactory::with_config(FastRenderPoolConfig::new().with_renderer_config(renderer)).unwrap()
}

struct EnvVarGuard {
  key: &'static str,
  previous: Option<OsString>,
}

impl EnvVarGuard {
  fn set(key: &'static str, value: &str) -> Self {
    let previous = std::env::var_os(key);
    std::env::set_var(key, value);
    Self { key, previous }
  }
}

impl Drop for EnvVarGuard {
  fn drop(&mut self) {
    match self.previous.take() {
      Some(value) => std::env::set_var(self.key, value),
      None => std::env::remove_var(self.key),
    }
  }
}

#[test]
fn about_newtab_navigation_yields_frame_and_no_fetch_stages() {
  let _lock = test_lock();

  let factory = factory_for_tests();
  let (tx, rx, handle) = spawn_browser_render_thread(factory).unwrap();

  let tab_id = TabId(1);
  let cancel = CancelGens::new();
  tx.send(UiToWorker::CreateTab {
    tab_id,
    initial_url: None,
    cancel,
  })
  .unwrap();
  tx.send(UiToWorker::ViewportChanged {
    tab_id,
    viewport_css: (64, 64),
    dpr: 1.0,
  })
  .unwrap();
  tx.send(UiToWorker::Navigate {
    tab_id,
    url: "about:newtab".to_string(),
    reason: NavigationReason::TypedUrl,
  })
  .unwrap();

  let mut stages = Vec::new();
  let mut saw_frame = false;
  while let Ok(msg) = rx.recv_timeout(Duration::from_secs(2)) {
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
  let _lock = test_lock();

  let factory = factory_for_tests();
  let (tx, rx, handle) = spawn_browser_render_thread(factory).unwrap();

  let tab_id = TabId(1);
  let cancel = CancelGens::new();
  tx.send(UiToWorker::CreateTab {
    tab_id,
    initial_url: None,
    cancel,
  })
  .unwrap();
  tx.send(UiToWorker::ViewportChanged {
    tab_id,
    viewport_css: (64, 64),
    dpr: 1.0,
  })
  .unwrap();
  tx.send(UiToWorker::Navigate {
    tab_id,
    url: "about:test-scroll".to_string(),
    reason: NavigationReason::TypedUrl,
  })
  .unwrap();

  // Wait for the initial frame.
  while let Ok(msg) = rx.recv_timeout(Duration::from_secs(2)) {
    if matches!(msg, WorkerToUi::FrameReady { .. }) {
      break;
    }
  }

  tx.send(UiToWorker::Scroll {
    tab_id,
    delta_css: (0.0, 200.0),
    pointer_css: None,
  })
  .unwrap();

  let mut saw_scroll = false;
  let mut saw_frame = false;
  while let Ok(msg) = rx.recv_timeout(Duration::from_secs(2)) {
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
  let _lock = test_lock();
  let _env = EnvVarGuard::set("FASTR_TEST_RENDER_DELAY_MS", "1");

  let factory = factory_for_tests();
  let (tx, rx, handle) = spawn_browser_render_thread(factory).unwrap();

  let tab_id = TabId(1);
  let cancel = CancelGens::new();
  tx.send(UiToWorker::CreateTab {
    tab_id,
    initial_url: None,
    cancel: cancel.clone(),
  })
  .unwrap();
  tx.send(UiToWorker::ViewportChanged {
    tab_id,
    viewport_css: (128, 128),
    dpr: 1.0,
  })
  .unwrap();

  let first_url = "about:test-heavy".to_string();
  let second_url = "about:blank".to_string();

  tx.send(UiToWorker::Navigate {
    tab_id,
    url: first_url.clone(),
    reason: NavigationReason::TypedUrl,
  })
  .unwrap();

  let mut started_first = false;
  let mut sent_second = false;
  let mut last_committed: Option<String> = None;
  let mut saw_second_frame = false;
  let mut saw_first_frame = false;

  while let Ok(msg) = rx.recv_timeout(Duration::from_secs(10)) {
    match msg {
      WorkerToUi::NavigationStarted { url, .. } if url == first_url => {
        started_first = true;
      }
      WorkerToUi::Stage { .. } if started_first && !sent_second => {
        // Simulate UI-driven cancellation while the worker is blocked in the first navigation.
        cancel.bump_nav();
        tx.send(UiToWorker::Navigate {
          tab_id,
          url: second_url.clone(),
          reason: NavigationReason::TypedUrl,
        })
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
