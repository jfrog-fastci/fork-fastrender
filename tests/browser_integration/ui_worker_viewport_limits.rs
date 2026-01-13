#![cfg(feature = "browser_ui")]

use super::support;
use fastrender::api::{FastRenderConfig, FastRenderFactory, FastRenderPoolConfig};
use fastrender::debug::runtime::RuntimeToggles;
use fastrender::text::font_db::FontConfig;
use fastrender::ui::browser_limits::BrowserLimits;
use fastrender::ui::messages::{NavigationReason, TabId, WorkerToUi};
use fastrender::ui::spawn_ui_worker_with_factory;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

// Keep this generous under CI contention; the first navigation can be slow.
const TIMEOUT: Duration = Duration::from_secs(20);

#[test]
fn absurd_viewport_changed_is_clamped_before_pixmap_allocation() {
  let _lock = super::stage_listener_test_lock();

  // Keep this test cheap and deterministic: clamp to a small pixmap limit so we don't allocate
  // hundreds of MiB on CI.
  let runtime_toggles = RuntimeToggles::from_map(HashMap::from([
    (
      "FASTR_BROWSER_MAX_PIXELS".to_string(),
      "1_000_000".to_string(),
    ),
    (
      "FASTR_BROWSER_MAX_DIM_PX".to_string(),
      "2048".to_string(),
    ),
  ]));
  let limits = fastrender::debug::runtime::with_thread_runtime_toggles(
    Arc::new(runtime_toggles.clone()),
    BrowserLimits::from_env,
  );
  assert_eq!(
    limits.max_pixels, 1_000_000,
    "expected FASTR_BROWSER_MAX_PIXELS override to apply"
  );
  assert_eq!(
    limits.max_dim_px, 2048,
    "expected FASTR_BROWSER_MAX_DIM_PX override to apply"
  );

  let renderer_config = FastRenderConfig::default()
    .with_font_sources(FontConfig::bundled_only())
    .with_runtime_toggles(runtime_toggles);

  let factory = FastRenderFactory::with_config(
    FastRenderPoolConfig::new().with_renderer_config(renderer_config),
  )
  .expect("factory");

  let handle = spawn_ui_worker_with_factory("fastr-ui-worker-viewport-limits", factory)
    .expect("spawn ui worker");
  let (ui_tx, ui_rx, join) = handle.split();

  let tab_id = TabId::new();
  ui_tx
    .send(support::create_tab_msg(tab_id, None))
    .expect("CreateTab");

  // These values would imply a multi-gigabyte pixmap without clamping.
  ui_tx
    .send(support::viewport_changed_msg(
      tab_id,
      (100_000, 100_000),
      4.0,
    ))
    .expect("ViewportChanged");

  ui_tx
    .send(support::navigate_msg(
      tab_id,
      "about:newtab".to_string(),
      NavigationReason::TypedUrl,
    ))
    .expect("Navigate");

  let mut saw_warning = false;

  let msg = support::recv_for_tab(&ui_rx, tab_id, TIMEOUT, |msg| match msg {
    WorkerToUi::Warning { .. } => {
      saw_warning = true;
      false
    }
    WorkerToUi::FrameReady { .. } => true,
    WorkerToUi::NavigationFailed { .. } => true,
    _ => false,
  })
  .unwrap_or_else(|| panic!("timed out waiting for FrameReady for tab {tab_id:?}"));

  let frame = match msg {
    WorkerToUi::FrameReady { frame, .. } => frame,
    WorkerToUi::NavigationFailed { url, error, .. } => {
      panic!("navigation failed for {url}: {error}")
    }
    other => panic!("unexpected WorkerToUi message: {other:?}"),
  };

  let (w_px, h_px) = (frame.pixmap.width(), frame.pixmap.height());
  assert!(
    w_px <= limits.max_dim_px && h_px <= limits.max_dim_px,
    "expected pixmap dims clamped to <= {}px, got {}x{} (viewport_css={:?}, dpr={})",
    limits.max_dim_px,
    w_px,
    h_px,
    frame.viewport_css,
    frame.dpr
  );
  let total = (w_px as u64) * (h_px as u64);
  assert!(
    total <= limits.max_pixels,
    "expected total pixels <= {}, got {} (pixmap={}x{})",
    limits.max_pixels,
    total,
    w_px,
    h_px
  );
  assert!(
    frame.viewport_css.0 < 100_000 || frame.viewport_css.1 < 100_000 || frame.dpr < 4.0,
    "expected worker to clamp absurd viewport/dpr, but frame meta was viewport_css={:?} dpr={}",
    frame.viewport_css,
    frame.dpr
  );
  assert!(
    saw_warning,
    "expected worker to emit a Warning when clamping viewport"
  );

  drop(ui_tx);
  join.join().expect("join ui worker");
}
