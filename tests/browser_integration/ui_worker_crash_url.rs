#![cfg(feature = "browser_ui")]

use std::collections::HashMap;
use std::sync::mpsc::RecvTimeoutError;
use std::time::{Duration, Instant};

use fastrender::api::{FastRenderConfig, FastRenderFactory, FastRenderPoolConfig};
use fastrender::debug::runtime::RuntimeToggles;
use fastrender::text::font_db::FontConfig;
use fastrender::ui::messages::{NavigationReason, TabId, WorkerToUi};
use fastrender::ui::spawn_ui_worker_with_factory;

use super::support::{
  allow_crash_urls_for_test, create_tab_msg, navigate_msg, recv_for_tab, DEFAULT_TIMEOUT,
};

const CRASH_URL: &str = "crash://panic";

fn factory_with_crash_urls_enabled(enabled: bool) -> FastRenderFactory {
  let mut raw = std::env::vars()
    .filter(|(k, _)| k.starts_with("FASTR_"))
    .collect::<HashMap<_, _>>();
  raw.insert(
    "FASTR_ENABLE_CRASH_URLS".to_string(),
    if enabled { "1" } else { "0" }.to_string(),
  );
  let toggles = RuntimeToggles::from_map(raw);

  let renderer_config = FastRenderConfig::default()
    .with_font_sources(FontConfig::bundled_only())
    .with_runtime_toggles(toggles);

  FastRenderFactory::with_config(
    FastRenderPoolConfig::new().with_renderer_config(renderer_config),
  )
  .expect("build factory")
}

#[test]
fn crash_url_is_ignored_unless_opted_in() {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
  let _lock = super::stage_listener_test_lock();
  let _allow_crash_urls = allow_crash_urls_for_test();

  let factory = factory_with_crash_urls_enabled(false);
  let (ui_tx, ui_rx, join) = spawn_ui_worker_with_factory("fastr-ui-worker-crash-url-disabled", factory)
    .expect("spawn ui worker")
    .split();

  let tab_id = TabId::new();
  ui_tx
    .send(create_tab_msg(tab_id, None))
    .expect("send CreateTab");
  ui_tx
    .send(navigate_msg(
      tab_id,
      CRASH_URL.to_string(),
      NavigationReason::TypedUrl,
    ))
    .expect("send Navigate");

  let Some(_msg) = recv_for_tab(&ui_rx, tab_id, DEFAULT_TIMEOUT, |msg| {
    matches!(msg, WorkerToUi::NavigationFailed { url, .. } if url.to_ascii_lowercase().starts_with(CRASH_URL))
  }) else {
    panic!("timed out waiting for NavigationFailed for {CRASH_URL}");
  };

  drop(ui_tx);
  join.join().expect("worker must not panic when crash URLs are disabled");
}

#[test]
fn crash_url_panics_worker_and_disconnects_channel_when_enabled() {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
  let _lock = super::stage_listener_test_lock();
  let _allow_crash_urls = allow_crash_urls_for_test();

  let factory = factory_with_crash_urls_enabled(true);
  let (ui_tx, ui_rx, join) = spawn_ui_worker_with_factory("fastr-ui-worker-crash-url-enabled", factory)
    .expect("spawn ui worker")
    .split();

  let tab_id = TabId::new();
  ui_tx
    .send(create_tab_msg(tab_id, None))
    .expect("send CreateTab");
  ui_tx
    .send(navigate_msg(
      tab_id,
      CRASH_URL.to_string(),
      NavigationReason::TypedUrl,
    ))
    .expect("send Navigate");

  let Some(_msg) = recv_for_tab(&ui_rx, tab_id, DEFAULT_TIMEOUT, |msg| {
    matches!(msg, WorkerToUi::NavigationStarted { url, .. } if url.to_ascii_lowercase().starts_with(CRASH_URL))
  }) else {
    panic!("timed out waiting for NavigationStarted for {CRASH_URL}");
  };

  // After the explicit crash the worker should drop its sender and the UI receiver should observe
  // disconnection (once any queued messages are drained).
  let deadline = Instant::now() + DEFAULT_TIMEOUT;
  let mut disconnected = false;
  while Instant::now() < deadline {
    match ui_rx.recv_timeout(Duration::from_millis(25)) {
      Ok(_msg) => {}
      Err(RecvTimeoutError::Timeout) => {}
      Err(RecvTimeoutError::Disconnected) => {
        disconnected = true;
        break;
      }
    }
  }
  assert!(
    disconnected,
    "timed out waiting for worker channel to disconnect after crash URL"
  );

  // Close the UiToWorker channel so the worker's router thread can observe shutdown even if it
  // didn't have a chance to forward a message after the crash.
  drop(ui_tx);

  assert!(
    join.join().is_err(),
    "expected worker thread to panic when crash URLs are enabled"
  );
}
