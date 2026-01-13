#![cfg(feature = "browser_ui")]

use super::support::{
  allow_crash_urls_for_test, create_tab_msg, navigate_msg, viewport_changed_msg, DEFAULT_TIMEOUT,
};
use super::worker_harness::{format_events, WorkerHarness, WorkerToUiEvent};
use fastrender::api::{FastRenderConfig, FastRenderFactory, FastRenderPoolConfig};
use fastrender::debug::runtime::RuntimeToggles;
use fastrender::ui::messages::{NavigationReason, TabId};
use fastrender::text::font_db::FontConfig;
use std::collections::HashMap;

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

#[test]
fn worker_harness_wait_for_disconnect_observes_worker_panic() {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
  let _allow_crash_urls = allow_crash_urls_for_test();

  let h = WorkerHarness::spawn_with_factory(crash_enabled_factory());
  let tab_id = TabId::new();

  // Ensure the worker thread is up and producing frames before triggering the crash.
  h.send(create_tab_msg(tab_id, Some("about:newtab".to_string())));
  h.send(viewport_changed_msg(tab_id, (200, 120), 1.0));
  let _ = h.wait_for_frame(tab_id, DEFAULT_TIMEOUT);
  let pre_crash_log = h.drain_for(std::time::Duration::from_millis(100));

  // Trigger a deterministic worker panic.
  h.send(navigate_msg(
    tab_id,
    "crash://panic".to_string(),
    NavigationReason::TypedUrl,
  ));

  let events = h.assert_disconnect_within(DEFAULT_TIMEOUT);
  assert!(
    events.iter().any(|ev| matches!(
      ev,
      WorkerToUiEvent::NavigationStarted { url, .. } if url.to_ascii_lowercase().starts_with("crash://panic")
    )),
    "expected NavigationStarted for crash URL.\npre-crash drain:\n{pre_crash_log}\nrecent events:\n{}",
    format_events(&events)
  );
}
