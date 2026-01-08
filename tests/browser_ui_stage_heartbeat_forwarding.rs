#![cfg(feature = "browser_ui")]

use fastrender::render_control::{record_stage, StageHeartbeat};
use fastrender::ui::messages::{TabId, WorkerToUi};
use fastrender::ui::worker::RenderWorker;
use fastrender::{FastRender, PreparedPaintOptions, RenderOptions};
use tempfile::tempdir;

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
