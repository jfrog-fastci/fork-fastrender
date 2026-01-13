#![cfg(feature = "browser_ui")]

use fastrender::api::{FastRenderConfig, FastRenderFactory, FastRenderPoolConfig};
use fastrender::debug::runtime::RuntimeToggles;
use fastrender::text::font_db::FontConfig;
use fastrender::ui::messages::{NavigationReason, TabId};
use fastrender::ui::spawn_ui_worker_with_factory;
use std::collections::HashMap;
use std::sync::mpsc::RecvTimeoutError;
use std::time::{Duration, Instant};

use super::support::{
  allow_crash_urls_for_test, create_tab_msg, format_messages, navigate_msg, DEFAULT_TIMEOUT,
};

fn crash_enabled_factory() -> FastRenderFactory {
  let mut raw = std::env::vars()
    .filter(|(k, _)| k.starts_with("FASTR_"))
    .collect::<HashMap<_, _>>();
  raw.insert("FASTR_ENABLE_CRASH_URLS".to_string(), "1".to_string());
  let toggles = RuntimeToggles::from_map(raw);

  let renderer_config = FastRenderConfig::default()
    .with_font_sources(FontConfig::bundled_only())
    .with_runtime_toggles(toggles);

  FastRenderFactory::with_config(
    FastRenderPoolConfig::new().with_renderer_config(renderer_config),
  )
  .expect("build crash-enabled factory")
}

fn join_with_timeout(
  join: std::thread::JoinHandle<()>,
  timeout: Duration,
) -> std::thread::Result<()> {
  let (done_tx, done_rx) = std::sync::mpsc::channel::<std::thread::Result<()>>();
  std::thread::spawn(move || {
    let _ = done_tx.send(join.join());
  });
  match done_rx.recv_timeout(timeout) {
    Ok(res) => res,
    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
      panic!("timed out joining crashed worker thread");
    }
    Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
      panic!("join waiter disconnected");
    }
  }
}

#[test]
fn crash_scheme_navigation_panics_worker_thread() {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
  let _lock = super::stage_listener_test_lock();
  let _allow_crash_urls = allow_crash_urls_for_test();

  let handle = spawn_ui_worker_with_factory(
    "fastr-ui-worker-crash-hook",
    crash_enabled_factory(),
  )
  .expect("spawn ui worker");
  let (ui_tx, ui_rx, join) = handle.split();

  let tab_id = TabId::new();
  ui_tx
    .send(create_tab_msg(tab_id, None))
    .expect("send CreateTab");
  ui_tx
    .send(navigate_msg(
      tab_id,
      "crash://panic".to_string(),
      NavigationReason::TypedUrl,
    ))
    .expect("send Navigate");

  // Close the UI→worker channel so the router thread inside `spawn_ui_worker` can exit even if the
  // main worker thread panics before joining it.
  drop(ui_tx);

  // Wait for the worker→UI channel to disconnect so we don't hang on `join` due to a stuck worker.
  let deadline = Instant::now() + DEFAULT_TIMEOUT;
  let mut msgs = Vec::new();
  loop {
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
      panic!(
        "timed out waiting for worker channel disconnect\nmessages:\n{}",
        format_messages(&msgs)
      );
    }
    match ui_rx.recv_timeout(remaining.min(Duration::from_millis(25))) {
      Ok(msg) => msgs.push(msg),
      Err(RecvTimeoutError::Timeout) => continue,
      Err(RecvTimeoutError::Disconnected) => break,
    }
  }

  let join_res = join_with_timeout(join, DEFAULT_TIMEOUT);
  assert!(
    join_res.is_err(),
    "expected worker thread to panic for crash:// navigation, but join returned {join_res:?}"
  );
}
