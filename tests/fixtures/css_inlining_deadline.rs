use crate::common::{global_test_lock, StageListenerGuard};
use std::path::PathBuf;
use std::time::Duration;

use fastrender::api::{FastRender, RenderOptions};
use fastrender::error::RenderStage;
use fastrender::render_control::StageHeartbeat;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use url::Url;

#[test]
fn css_inlining_respects_deadline() {
  let _lock = global_test_lock();
  let css_checks = Arc::new(AtomicUsize::new(0));
  let cancel_fired = Arc::new(AtomicBool::new(false));
  let saw_css_inline_heartbeat = Arc::new(AtomicBool::new(false));

  // Use stage heartbeats to ensure we reach the CSS inlining phase, while using the deadline's
  // stage hint (`render_control::active_stage()`) to deterministically cancel only during CSS work
  // (avoiding fragile wall-clock thresholds).
  let saw_css_inline_heartbeat_listener = Arc::clone(&saw_css_inline_heartbeat);
  let _stage_listener_guard = StageListenerGuard::new(Arc::new(move |stage| {
    if stage == StageHeartbeat::CssInline {
      saw_css_inline_heartbeat_listener.store(true, Ordering::Relaxed);
    }
  }));

  let css_checks_cancel = Arc::clone(&css_checks);
  let cancel_fired = Arc::clone(&cancel_fired);
  let cancel_callback: Arc<fastrender::CancelCallback> = Arc::new(move || {
    // Cancellation should only trigger during CSS fetch/inline/parse work, not earlier DOM parsing
    // or later cascade/layout/paint stages.
    if fastrender::render_control::active_stage() != Some(RenderStage::Css) {
      return false;
    }
    if cancel_fired.swap(true, Ordering::Relaxed) {
      return false;
    }
    css_checks_cancel.fetch_add(1, Ordering::Relaxed);
    true
  });

  let mut renderer = FastRender::new().unwrap();
  let fixtures = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
  let outer_path = fixtures.join("css_timeout_outer.css");
  let outer_url = Url::from_file_path(&outer_path).unwrap().to_string();
  let html = format!(
    r#"<html><head><link rel="stylesheet" href="{outer_url}"></head><body>slow css</body></html>"#
  );
  let options = RenderOptions::default()
    .with_timeout(Some(Duration::from_secs(1)))
    .with_cancel_callback(Some(cancel_callback));

  let result = renderer
    .render_html_with_stylesheets(&html, "file:///css-timeout.html", options)
    .expect("render should continue after stylesheet timeout");

  assert_eq!(
    css_checks.load(Ordering::Relaxed),
    1,
    "expected exactly one CSS cancellation-triggering deadline check"
  );
  assert!(
    saw_css_inline_heartbeat.load(Ordering::Relaxed),
    "expected to observe css_inline stage heartbeat"
  );

  assert!(result.diagnostics.failure_stage.is_none());
  assert!(
    result
      .diagnostics
      .fetch_errors
      .iter()
      .any(|err| err.message.contains("timed out during css")),
    "expected stylesheet timeout error, got: {:?}",
    result.diagnostics.fetch_errors
  );
}
