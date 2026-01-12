use fastrender::api::{FastRender, RenderOptions};
use fastrender::error::{Error, RenderError, RenderStage};
use fastrender::render_control::{GlobalStageListenerGuard, StageHeartbeat};
use fastrender::LayoutParallelism;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;

fn heavy_inline_html(count: usize) -> String {
  let mut html = String::from("<div>");
  for i in 0..count {
    html.push_str(&format!("<span class=\"c{i}\">item</span>"));
  }
  html.push_str("</div>");
  html
}

#[test]
fn layout_loops_respect_timeout() {
  let layout_checks = Arc::new(AtomicUsize::new(0));
  let saw_layout_heartbeat = Arc::new(AtomicBool::new(false));
  let saw_layout_heartbeat_listener = Arc::clone(&saw_layout_heartbeat);
  let _stage_guard = GlobalStageListenerGuard::new(Arc::new(move |stage| {
    if stage == StageHeartbeat::Layout {
      saw_layout_heartbeat_listener.store(true, Ordering::Relaxed);
    }
  }));

  // Cancel on the second deadline check observed during layout. This exercises layout's periodic
  // deadline checks while avoiding fragile wall-clock thresholds (DOM parse speed varies).
  let layout_checks_cancel = Arc::clone(&layout_checks);
  let cancel_callback: Arc<fastrender::CancelCallback> = Arc::new(move || {
    if fastrender::render_control::active_stage() != Some(RenderStage::Layout) {
      return false;
    }
    let seen = layout_checks_cancel.fetch_add(1, Ordering::Relaxed) + 1;
    seen >= 2
  });

  let mut renderer = FastRender::new().unwrap();
  let html = heavy_inline_html(4000);
  let options = RenderOptions::new()
    .with_viewport(64, 64)
    // Disable layout parallelism so deadline checks are deterministic (and we don't have to care
    // which Rayon worker first trips cancellation).
    .with_layout_parallelism(LayoutParallelism::disabled())
    .with_cancel_callback(Some(cancel_callback));

  let err = renderer
    .render_html_with_options(&html, options)
    .expect_err("layout should time out cooperatively");

  assert!(
    layout_checks.load(Ordering::Relaxed) >= 2,
    "expected at least 2 layout deadline checks before cancellation"
  );
  assert!(
    saw_layout_heartbeat.load(Ordering::Relaxed),
    "expected to observe layout stage heartbeat"
  );

  match err {
    Error::Render(RenderError::Timeout { stage, .. }) => assert_eq!(stage, RenderStage::Layout),
    other => panic!("unexpected error: {other:?}"),
  }
}

#[test]
fn layout_timeout_records_diagnostics() {
  let layout_checks = Arc::new(AtomicUsize::new(0));
  let saw_layout_heartbeat = Arc::new(AtomicBool::new(false));
  let saw_layout_heartbeat_listener = Arc::clone(&saw_layout_heartbeat);
  let _stage_guard = GlobalStageListenerGuard::new(Arc::new(move |stage| {
    if stage == StageHeartbeat::Layout {
      saw_layout_heartbeat_listener.store(true, Ordering::Relaxed);
    }
  }));

  let layout_checks_cancel = Arc::clone(&layout_checks);
  let cancel_callback: Arc<fastrender::CancelCallback> = Arc::new(move || {
    if fastrender::render_control::active_stage() != Some(RenderStage::Layout) {
      return false;
    }
    let seen = layout_checks_cancel.fetch_add(1, Ordering::Relaxed) + 1;
    seen >= 2
  });

  let mut renderer = FastRender::new().unwrap();
  let html = heavy_inline_html(4000);
  let options = RenderOptions::new()
    .with_viewport(32, 24)
    .with_layout_parallelism(LayoutParallelism::disabled())
    .with_cancel_callback(Some(cancel_callback))
    .allow_partial(true);

  let result = renderer
    .render_html_with_stylesheets(&html, "https://example.com", options)
    .expect("layout timeout should produce diagnostics");

  assert!(
    layout_checks.load(Ordering::Relaxed) >= 2,
    "expected at least 2 layout deadline checks before cancellation"
  );
  assert!(
    saw_layout_heartbeat.load(Ordering::Relaxed),
    "expected to observe layout stage heartbeat"
  );

  assert_eq!(result.diagnostics.timeout_stage, Some(RenderStage::Layout));
  assert_eq!(result.pixmap.width(), 32);
  assert_eq!(result.pixmap.height(), 24);
}
