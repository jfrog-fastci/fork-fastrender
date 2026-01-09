#![cfg(feature = "browser_ui")]

use fastrender::render_control::{record_stage, StageHeartbeat};
use fastrender::ui::cancel::CancelGens;
use fastrender::ui::messages::{NavigationReason, TabId, UiToWorker, WorkerToUi};
use fastrender::ui::worker::RenderWorker;
use fastrender::ui::worker::spawn_ui_worker as spawn_history_ui_worker;
use fastrender::ui::worker_loop::spawn_ui_worker as spawn_ui_worker_loop;
use fastrender::{FastRender, PreparedPaintOptions, RenderOptions};
use tempfile::tempdir;

use super::support::{create_tab_msg_with_cancel, navigate_msg, viewport_changed_msg, DEFAULT_TIMEOUT};

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
  let _lock = super::stage_listener_test_lock();
  let dir = tempdir().expect("temp dir");
  std::fs::write(
    dir.path().join("style.css"),
    "body { background: rgb(1, 2, 3); }",
  )
  .expect("write css");

  let base_url = format!("file://{}/index.html", dir.path().display());
  let html = r#"
    <!doctype html>
    <html>
      <head>
        <link rel="stylesheet" href="style.css">
      </head>
      <body>Hello</body>
    </html>
  "#;

  let renderer = FastRender::builder().base_url(base_url).build().unwrap();
  let (tx, rx) = std::sync::mpsc::channel::<WorkerToUi>();
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
        WorkerToUi::Stage { tab_id: msg_tab, stage } if *msg_tab == tab_id => Some(*stage),
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

  // Stage forwarding must be scoped to the job: once the job completes, the global stage listener
  // should be removed.
  record_stage(StageHeartbeat::DomParse);
  assert!(
    rx.try_recv().is_err(),
    "expected stage listener to be cleared after jobs"
  );
}

#[test]
fn stage_heartbeats_forwarded_from_worker_loop_and_listener_cleared() {
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
      <body>Hello</body>
    </html>
  "#;
  std::fs::write(dir.path().join("index.html"), html).expect("write html");
  let url = format!("file://{}/index.html", dir.path().display());

  let (ui_tx, ui_rx, join) = spawn_ui_worker_loop("fastr-ui-worker-loop-stage-test")
    .expect("spawn ui worker")
    .split();
  let tab_id = TabId::new();

  ui_tx
    .send(create_tab_msg_with_cancel(tab_id, None, CancelGens::new()))
    .expect("CreateTab");
  ui_tx
    .send(viewport_changed_msg(tab_id, (200, 120), 1.0))
    .expect("ViewportChanged");
  ui_tx
    .send(navigate_msg(tab_id, url, NavigationReason::TypedUrl))
    .expect("Navigate");

  let deadline = std::time::Instant::now() + DEFAULT_TIMEOUT;
  let mut messages = Vec::new();
  let mut saw_frame = false;

  while std::time::Instant::now() < deadline {
    let remaining = deadline.saturating_duration_since(std::time::Instant::now());
    match ui_rx.recv_timeout(remaining.min(std::time::Duration::from_millis(200))) {
      Ok(msg) => {
        if matches!(msg, WorkerToUi::FrameReady { tab_id: got, .. } if got == tab_id) {
          saw_frame = true;
        }
        messages.push(msg);
        if saw_frame {
          break;
        }
      }
      Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
      Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
    }
  }

  // Allow any trailing stage heartbeats enqueued by background threads to arrive.
  while let Ok(msg) = ui_rx.recv_timeout(std::time::Duration::from_millis(50)) {
    messages.push(msg);
  }

  assert!(saw_frame, "expected FrameReady message for worker loop navigation");

  let stages: Vec<StageHeartbeat> = messages
    .iter()
    .filter_map(|msg| match msg {
      WorkerToUi::Stage {
        tab_id: got,
        stage,
      } if *got == tab_id => Some(*stage),
      _ => None,
    })
    .collect();
  assert!(
    !stages.is_empty(),
    "expected stage heartbeats for worker loop tab, got none"
  );

  // Stage forwarding must be scoped to the job: once the navigation completes, the global stage
  // listener should be removed.
  while ui_rx.try_recv().is_ok() {}
  record_stage(StageHeartbeat::DomParse);
  assert!(
    ui_rx.try_recv().is_err(),
    "expected stage listener to be cleared after worker loop navigation"
  );

  drop(ui_tx);
  join.join().expect("join ui worker thread");
}

#[test]
fn stage_heartbeats_forwarded_from_history_worker_loop_and_listener_cleared() {
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
      <body>Hello</body>
    </html>
  "#;
  std::fs::write(dir.path().join("index.html"), html).expect("write html");
  let url = format!("file://{}/index.html", dir.path().display());

  let (ui_tx, ui_rx, join) = spawn_history_ui_worker("fastr-ui-worker-stage-test")
    .expect("spawn ui worker")
    .into_parts();
  let tab_id = TabId::new();

  ui_tx
    .send(UiToWorker::CreateTab {
      tab_id,
      initial_url: None,
      cancel: CancelGens::new(),
    })
    .expect("CreateTab");
  ui_tx
    .send(UiToWorker::ViewportChanged {
      tab_id,
      viewport_css: (200, 120),
      dpr: 1.0,
    })
    .expect("ViewportChanged");
  ui_tx
    .send(UiToWorker::Navigate {
      tab_id,
      url,
      reason: NavigationReason::TypedUrl,
    })
    .expect("Navigate");

  let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
  let mut messages = Vec::new();
  let mut saw_frame = false;

  while std::time::Instant::now() < deadline {
    let remaining = deadline.saturating_duration_since(std::time::Instant::now());
    match ui_rx.recv_timeout(remaining.min(std::time::Duration::from_millis(200))) {
      Ok(msg) => {
        if matches!(msg, WorkerToUi::FrameReady { tab_id: got, .. } if got == tab_id) {
          saw_frame = true;
        }
        messages.push(msg);
        if saw_frame {
          break;
        }
      }
      Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
      Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
    }
  }

  // Allow any trailing stage heartbeats enqueued by background threads to arrive.
  while let Ok(msg) = ui_rx.recv_timeout(std::time::Duration::from_millis(50)) {
    messages.push(msg);
  }

  assert!(saw_frame, "expected FrameReady message for navigation");

  let stages: Vec<StageHeartbeat> = messages
    .iter()
    .filter_map(|msg| match msg {
      WorkerToUi::Stage {
        tab_id: got,
        stage,
      } if *got == tab_id => Some(*stage),
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

  // Stage forwarding must be scoped to the job: once the navigation completes, the global stage
  // listener should be removed.
  while ui_rx.try_recv().is_ok() {}
  record_stage(StageHeartbeat::DomParse);
  assert!(
    ui_rx.try_recv().is_err(),
    "expected stage listener to be cleared after history worker navigation"
  );

  drop(ui_tx);
  join.join().expect("join ui worker thread");
}
