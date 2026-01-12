use fastrender::render_control::StageHeartbeat;
use fastrender::ui::messages::WorkerToUi;
use fastrender::ui::{BrowserAppState, BrowserTabState, TabId};

#[test]
fn stage_loading_progress_is_monotonic() {
  assert!(
    StageHeartbeat::ReadCache.loading_progress() > 0.0,
    "expected ReadCache to map to a progress value > 0.0 so 0.0 can represent \"no stage yet\""
  );
  assert_eq!(
    StageHeartbeat::Done.loading_progress(),
    1.0,
    "expected Done to map to exactly 1.0"
  );

  let mut prev = 0.0_f32;

  for stage in StageHeartbeat::all() {
    let progress = stage.loading_progress();
    assert!(
      progress.is_finite(),
      "expected StageHeartbeat::{stage:?}.loading_progress() to be finite, got {progress}"
    );
    assert!(
      (0.0..=1.0).contains(&progress),
      "expected progress in [0,1], got {progress} for StageHeartbeat::{stage:?}"
    );
    assert!(
      progress > prev,
      "expected strictly increasing progress: StageHeartbeat::{stage:?} ({progress}) <= previous ({prev})"
    );
    prev = progress;
  }

  assert!(
    (prev - 1.0).abs() <= f32::EPSILON,
    "expected final stage to map to 1.0, got {prev}"
  );
}

#[test]
fn chrome_loading_progress_resets_across_navigations() {
  let tab_id = TabId(1);
  let mut app = BrowserAppState::new();
  app.push_tab(BrowserTabState::new(tab_id, "about:newtab".to_string()), true);

  // Navigation 1: start → observe stage progress.
  app.apply_worker_msg(WorkerToUi::NavigationStarted {
    tab_id,
    url: "https://example.com/".to_string(),
  });
  app.apply_worker_msg(WorkerToUi::Stage {
    tab_id,
    stage: StageHeartbeat::Layout,
  });
  let progress_before = app
    .tab(tab_id)
    .expect("tab exists")
    .chrome_loading_progress()
    .expect("tab should be loading");
  assert!(
    progress_before > 0.0,
    "expected non-zero progress after a stage heartbeat, got {progress_before}"
  );

  // Navigation 2: should clear stage/progress.
  app.apply_worker_msg(WorkerToUi::NavigationStarted {
    tab_id,
    url: "https://example.org/".to_string(),
  });
  let progress_after = app
    .tab(tab_id)
    .expect("tab exists")
    .chrome_loading_progress()
    .expect("tab should still be loading after NavigationStarted");
  assert!(
    (progress_after - 0.0).abs() <= f32::EPSILON,
    "expected progress to reset to 0.0 on navigation start, got {progress_after}"
  );

  // Navigation commit should stop showing progress entirely.
  app.apply_worker_msg(WorkerToUi::NavigationCommitted {
    tab_id,
    url: "https://example.org/".to_string(),
    title: None,
    can_go_back: false,
    can_go_forward: false,
  });
  assert_eq!(
    app
      .tab(tab_id)
      .expect("tab exists")
      .chrome_loading_progress(),
    None,
    "expected progress to be hidden once loading=false"
  );
}

#[test]
fn chrome_loading_progress_is_monotonic_across_out_of_order_stage_events() {
  let tab_id = TabId(1);
  let mut app = BrowserAppState::new();
  app.push_tab(BrowserTabState::new(tab_id, "about:newtab".to_string()), true);

  app.apply_worker_msg(WorkerToUi::NavigationStarted {
    tab_id,
    url: "https://example.com/".to_string(),
  });

  let mut prev = app
    .tab(tab_id)
    .expect("tab exists")
    .chrome_loading_progress()
    .expect("tab should be loading");
  assert!(
    (prev - 0.0).abs() <= f32::EPSILON,
    "expected initial progress to be 0.0 after NavigationStarted, got {prev}"
  );

  for stage in [
    StageHeartbeat::Layout,
    // Regressing stage heartbeat must not reduce chrome progress.
    StageHeartbeat::ReadCache,
    StageHeartbeat::PaintRasterize,
    StageHeartbeat::DomParse,
    StageHeartbeat::Done,
  ] {
    app.apply_worker_msg(WorkerToUi::Stage { tab_id, stage });
    let next = app
      .tab(tab_id)
      .expect("tab exists")
      .chrome_loading_progress()
      .expect("tab should be loading");
    assert!(
      next + f32::EPSILON >= prev,
      "expected chrome loading progress to be monotonic (prev={prev}, next={next}, stage={stage:?})"
    );
    prev = next;
  }
}
