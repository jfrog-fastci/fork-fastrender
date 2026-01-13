#![cfg(feature = "browser_ui")]

use fastrender::render_control::{record_stage, StageHeartbeat};
use fastrender::ui::messages::{
  NavigationReason, PointerButton, PointerModifiers, TabId, UiToWorker, WorkerToUi, WorkerToUiMsg,
};
use fastrender::ui::spawn_ui_worker;
use fastrender::ui::RenderWorker;
use fastrender::{PreparedPaintOptions, RenderOptions};
use tempfile::tempdir;

use super::support::{
  create_tab_msg, drain_for, format_messages, navigate_msg, scroll_msg, viewport_changed_msg,
  DEFAULT_TIMEOUT,
};

fn assert_stage_order(stages: &[StageHeartbeat], expected: &[StageHeartbeat]) {
  let mut next = 0usize;
  for stage in stages {
    if next < expected.len() && *stage == expected[next] {
      next += 1;
    }
  }
  assert_eq!(
    next,
    expected.len(),
    "expected stage sequence {:?} in {:?}",
    expected,
    stages
  );
}

#[test]
fn stage_heartbeats_forwarded_to_ui_with_tab_id() {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
  let _lock = super::stage_listener_test_lock();
  let dir = tempdir().expect("temp dir");
  std::fs::write(
    dir.path().join("style.css"),
    "body { background: rgb(1, 2, 3); }",
  )
  .expect("write css");

  let base_url = url::Url::from_file_path(dir.path().join("index.html"))
    .unwrap()
    .to_string();
  let html = r#"
    <!doctype html>
    <html>
      <head>
        <link rel="stylesheet" href="style.css">
      </head>
      <body>Hello</body>
    </html>
  "#;

  let mut renderer = super::support::deterministic_renderer();
  renderer.set_base_url(base_url);
  let (tx, rx) = std::sync::mpsc::channel::<WorkerToUiMsg>();
  let rx = fastrender::ui::WorkerToUiInbox::new(rx);
  let mut worker = RenderWorker::new(renderer, tx);

  let options = RenderOptions::new().with_viewport(200, 120);

  let tab1 = TabId(1);
  let doc1 = worker
    .prepare_html(tab1, html, options.clone())
    .expect("prepare tab1");
  let _ = worker
    .paint_prepared(tab1, &doc1, PreparedPaintOptions::new())
    .expect("paint tab1");

  let tab2 = TabId(2);
  let doc2 = worker
    .prepare_html(tab2, html, options)
    .expect("prepare tab2");
  let _ = worker
    .paint_prepared(tab2, &doc2, PreparedPaintOptions::new())
    .expect("paint tab2");

  let messages: Vec<WorkerToUi> = rx.try_iter().collect();
  let stages_for = |tab_id: TabId| {
    messages
      .iter()
      .filter_map(|msg| match msg {
        WorkerToUi::Stage {
          tab_id: msg_tab,
          stage,
        } if *msg_tab == tab_id => Some(*stage),
        _ => None,
      })
      .collect::<Vec<_>>()
  };

  let expected = [
    StageHeartbeat::DomParse,
    StageHeartbeat::CssInline,
    StageHeartbeat::CssParse,
    StageHeartbeat::Cascade,
    StageHeartbeat::BoxTree,
    StageHeartbeat::Layout,
    StageHeartbeat::PaintBuild,
    StageHeartbeat::PaintRasterize,
  ];

  let tab1_stages = stages_for(tab1);
  assert!(
    !tab1_stages.is_empty(),
    "expected stage heartbeats for tab1, got none"
  );
  assert_stage_order(&tab1_stages, &expected);

  let tab2_stages = stages_for(tab2);
  assert!(
    !tab2_stages.is_empty(),
    "expected stage heartbeats for tab2, got none"
  );
  assert_stage_order(&tab2_stages, &expected);

  // Stage forwarding must be scoped to the job: once the job completes, the stage listener should
  // be removed.
  record_stage(StageHeartbeat::DomParse);
  assert!(
    rx.try_recv().is_err(),
    "expected stage listener to be cleared after jobs"
  );
}

#[test]
fn stage_heartbeats_forwarded_from_ui_worker_for_navigation_and_repaints() {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
  let _lock = super::stage_listener_test_lock();
  let dir = tempdir().expect("temp dir");

  std::fs::write(
    dir.path().join("style.css"),
    "body { background: rgb(1, 2, 3); }",
  )
  .expect("write css");

  let html = r#"<!doctype html>
    <html>
      <head>
        <link rel="stylesheet" href="style.css">
        <style>
          #hover-target { width: 120px; height: 40px; background: rgb(1, 2, 3); }
          #hover-target:hover { background: rgb(4, 5, 6); }
        </style>
      </head>
      <body>
        <div id="hover-target">Hover</div>
        <div style="height: 2000px;"></div>
      </body>
    </html>
  "#;
  std::fs::write(dir.path().join("index.html"), html).expect("write html");
  let url = url::Url::from_file_path(dir.path().join("index.html"))
    .unwrap()
    .to_string();

  let (ui_tx, ui_rx, join) = spawn_ui_worker("fastr-ui-worker-loop-stage-test")
    .expect("spawn ui worker")
    .split();
  let tab_id = TabId::new();

  ui_tx.send(create_tab_msg(tab_id, None)).expect("CreateTab");
  ui_tx
    .send(viewport_changed_msg(tab_id, (200, 120), 1.0))
    .expect("ViewportChanged");
  ui_tx
    .send(navigate_msg(tab_id, url, NavigationReason::TypedUrl))
    .expect("Navigate");

  // Rendering can take a few seconds under contention (CI runs integration tests in parallel and
  // the browser UI worker does real HTML/CSS/layout/paint work). Use a more generous timeout than
  // the default to reduce flakiness.
  let deadline = std::time::Instant::now() + DEFAULT_TIMEOUT * 2;
  let mut messages = Vec::new();
  let mut saw_frame = false;
  let mut saw_loading_done = false;

  while std::time::Instant::now() < deadline && !(saw_frame && saw_loading_done) {
    let remaining = deadline.saturating_duration_since(std::time::Instant::now());
    match ui_rx.recv_timeout(remaining.min(std::time::Duration::from_millis(200))) {
      Ok(msg) => {
        if matches!(msg, WorkerToUi::FrameReady { tab_id: got, .. } if got == tab_id) {
          saw_frame = true;
        }
        if matches!(
          msg,
          WorkerToUi::LoadingState {
            tab_id: got,
            loading: false,
          } if got == tab_id
        ) {
          saw_loading_done = true;
        }
        messages.push(msg);
      }
      Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
      Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
    }
  }

  // Allow any trailing stage heartbeats enqueued by background threads to arrive.
  while let Ok(msg) = ui_rx.recv_timeout(std::time::Duration::from_millis(50)) {
    messages.push(msg);
  }

  assert!(
    saw_frame,
    "expected FrameReady message for worker loop navigation; got:\n{}",
    format_messages(&messages)
  );
  assert!(
    saw_loading_done,
    "expected LoadingState(false) message after navigation; got:\n{}",
    format_messages(&messages)
  );

  let stages: Vec<StageHeartbeat> = messages
    .iter()
    .filter_map(|msg| match msg {
      WorkerToUi::Stage { tab_id: got, stage } if *got == tab_id => Some(*stage),
      _ => None,
    })
    .collect();
  assert!(
    !stages.is_empty(),
    "expected stage heartbeats for worker loop tab, got none"
  );

  // Stage forwarding must be scoped to each render job. Pointer moves trigger a repaint, and the
  // worker should forward stage heartbeats for that paint without including navigation-specific
  // fetch stages (e.g. ReadCache/FollowRedirects).
  while ui_rx.try_recv().is_ok() {}
  ui_tx
    .send(UiToWorker::PointerMove {
      tab_id,
      pos_css: (10.0, 10.0),
      button: PointerButton::None,
      modifiers: PointerModifiers::NONE,
    })
    .expect("PointerMove");

  let deadline = std::time::Instant::now() + DEFAULT_TIMEOUT;
  let mut saw_frame_after_input = false;
  let mut stages_after_input = Vec::new();
  while std::time::Instant::now() < deadline && !saw_frame_after_input {
    let remaining = deadline.saturating_duration_since(std::time::Instant::now());
    match ui_rx.recv_timeout(remaining.min(std::time::Duration::from_millis(200))) {
      Ok(msg) => match msg {
        WorkerToUi::Stage { tab_id: got, stage } if got == tab_id => {
          stages_after_input.push(stage);
        }
        WorkerToUi::FrameReady { tab_id: got, .. } if got == tab_id => {
          saw_frame_after_input = true;
        }
        _ => {}
      },
      Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
      Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
    }
  }
  assert!(
    saw_frame_after_input,
    "expected FrameReady after PointerMove"
  );
  assert!(
    !stages_after_input.is_empty(),
    "expected stage heartbeats during PointerMove repaint"
  );
  assert!(
    stages_after_input.iter().any(|stage| matches!(
      stage,
      StageHeartbeat::PaintBuild | StageHeartbeat::PaintRasterize
    )),
    "expected paint stage heartbeats during PointerMove repaint: {stages_after_input:?}"
  );
  assert!(
    !stages_after_input.iter().any(|stage| matches!(
      stage,
      StageHeartbeat::ReadCache | StageHeartbeat::FollowRedirects
    )),
    "unexpected fetch stage heartbeats during PointerMove repaint: {stages_after_input:?}"
  );

  // Ensure stage forwarding was scoped to the PointerMove repaint.
  let _ = drain_for(&ui_rx, std::time::Duration::from_millis(100));
  record_stage(StageHeartbeat::DomParse);
  assert!(
    ui_rx.try_recv().is_err(),
    "expected stage listener to be cleared after PointerMove repaint"
  );

  drop(ui_tx);
  join.join().expect("join ui worker thread");
}

#[test]
fn stage_heartbeats_forwarded_from_history_ui_worker_for_navigation_and_repaints() {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
  let _lock = super::stage_listener_test_lock();
  let dir = tempdir().expect("temp dir");

  std::fs::write(
    dir.path().join("style.css"),
    "body { background: rgb(1, 2, 3); }",
  )
  .expect("write css");

  let html = r#"<!doctype html>
    <html>
      <head>
        <link rel="stylesheet" href="style.css">
      </head>
      <body>
        Hello
        <div style="height: 2000px;"></div>
      </body>
    </html>
  "#;
  std::fs::write(dir.path().join("index.html"), html).expect("write html");
  let url = url::Url::from_file_path(dir.path().join("index.html"))
    .unwrap()
    .to_string();

  let (ui_tx, ui_rx, join) = spawn_ui_worker("fastr-ui-worker-stage-test")
    .expect("spawn ui worker")
    .split();
  let tab_id = TabId::new();

  ui_tx.send(create_tab_msg(tab_id, None)).expect("CreateTab");
  ui_tx
    .send(viewport_changed_msg(tab_id, (200, 120), 1.0))
    .expect("ViewportChanged");
  ui_tx
    .send(navigate_msg(tab_id, url, NavigationReason::TypedUrl))
    .expect("Navigate");

  let deadline = std::time::Instant::now() + DEFAULT_TIMEOUT * 2;
  let mut messages = Vec::new();
  let mut saw_frame = false;
  let mut saw_loading_done = false;

  while std::time::Instant::now() < deadline && !(saw_frame && saw_loading_done) {
    let remaining = deadline.saturating_duration_since(std::time::Instant::now());
    match ui_rx.recv_timeout(remaining.min(std::time::Duration::from_millis(200))) {
      Ok(msg) => {
        if matches!(msg, WorkerToUi::FrameReady { tab_id: got, .. } if got == tab_id) {
          saw_frame = true;
        }
        if matches!(
          msg,
          WorkerToUi::LoadingState {
            tab_id: got,
            loading: false,
          } if got == tab_id
        ) {
          saw_loading_done = true;
        }
        messages.push(msg);
      }
      Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
      Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
    }
  }

  // Allow any trailing stage heartbeats enqueued by background threads to arrive.
  while let Ok(msg) = ui_rx.recv_timeout(std::time::Duration::from_millis(50)) {
    messages.push(msg);
  }

  assert!(
    saw_frame,
    "expected FrameReady message for navigation; got:\n{}",
    format_messages(&messages)
  );
  assert!(
    saw_loading_done,
    "expected LoadingState(false) message after navigation; got:\n{}",
    format_messages(&messages)
  );

  let stages: Vec<StageHeartbeat> = messages
    .iter()
    .filter_map(|msg| match msg {
      WorkerToUi::Stage { tab_id: got, stage } if *got == tab_id => Some(*stage),
      _ => None,
    })
    .collect();
  assert!(
    !stages.is_empty(),
    "expected stage heartbeats for history worker tab, got none"
  );

  let expected = [
    StageHeartbeat::DomParse,
    StageHeartbeat::CssInline,
    StageHeartbeat::CssParse,
    StageHeartbeat::Cascade,
    StageHeartbeat::BoxTree,
    StageHeartbeat::Layout,
    StageHeartbeat::PaintBuild,
    StageHeartbeat::PaintRasterize,
  ];
  assert_stage_order(&stages, &expected);

  // Stage forwarding must be scoped to each render job. Scrolling triggers a repaint, and the
  // worker should forward stage heartbeats for that paint without including navigation-specific
  // fetch stages (e.g. ReadCache/FollowRedirects).
  while ui_rx.try_recv().is_ok() {}
  ui_tx
    .send(scroll_msg(tab_id, (0.0, 80.0), None))
    .expect("Scroll");

  let deadline = std::time::Instant::now() + DEFAULT_TIMEOUT;
  let mut saw_scroll_frame = false;
  let mut stages_after_scroll = Vec::new();
  while std::time::Instant::now() < deadline && !saw_scroll_frame {
    let remaining = deadline.saturating_duration_since(std::time::Instant::now());
    match ui_rx.recv_timeout(remaining.min(std::time::Duration::from_millis(200))) {
      Ok(msg) => match msg {
        WorkerToUi::Stage { tab_id: got, stage } if got == tab_id => {
          stages_after_scroll.push(stage);
        }
        WorkerToUi::FrameReady { tab_id: got, frame } if got == tab_id => {
          if frame.scroll_state.viewport.y > 0.0 {
            saw_scroll_frame = true;
          }
        }
        _ => {}
      },
      Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
      Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
    }
  }
  assert!(saw_scroll_frame, "expected FrameReady after scroll");
  assert!(
    !stages_after_scroll.is_empty(),
    "expected stage heartbeats during scroll repaint"
  );
  assert!(
    stages_after_scroll.iter().any(|stage| matches!(
      stage,
      StageHeartbeat::PaintBuild | StageHeartbeat::PaintRasterize
    )),
    "expected paint stage heartbeats during scroll repaint: {stages_after_scroll:?}"
  );
  assert!(
    !stages_after_scroll.iter().any(|stage| matches!(
      stage,
      StageHeartbeat::ReadCache | StageHeartbeat::FollowRedirects
    )),
    "unexpected fetch stage heartbeats during scroll repaint: {stages_after_scroll:?}"
  );

  // Ensure stage forwarding was scoped to the Scroll repaint.
  let _ = drain_for(&ui_rx, std::time::Duration::from_millis(100));
  record_stage(StageHeartbeat::DomParse);
  assert!(
    ui_rx.try_recv().is_err(),
    "expected stage listener to be cleared after scroll repaint"
  );

  drop(ui_tx);
  join.join().expect("join ui worker thread");
}
