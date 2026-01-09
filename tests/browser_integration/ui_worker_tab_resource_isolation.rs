#![cfg(feature = "browser_ui")]

use fastrender::ui::cancel::CancelGens;
use fastrender::ui::messages::{NavigationReason, RepaintReason, TabId, UiToWorker, WorkerToUi};
use fastrender::ui::worker_loop::spawn_ui_worker;
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};
use tempfile::tempdir;

use super::support::{create_tab_msg_with_cancel, navigate_msg, viewport_changed_msg};

fn pixel(pixmap: &tiny_skia::Pixmap, x: u32, y: u32) -> (u8, u8, u8, u8) {
  let px = pixmap.pixel(x, y).unwrap();
  (px.red(), px.green(), px.blue(), px.alpha())
}

fn wait_for_frame(
  rx: &Receiver<WorkerToUi>,
  tab_id: TabId,
  timeout: Duration,
) -> fastrender::ui::messages::RenderedFrame {
  let deadline = Instant::now() + timeout;
  loop {
    let remaining = deadline.saturating_duration_since(Instant::now());
    match rx.recv_timeout(remaining) {
      Ok(WorkerToUi::FrameReady {
        tab_id: got,
        frame,
      }) if got == tab_id => return frame,
      Ok(_) => continue,
      Err(RecvTimeoutError::Timeout) => panic!("timed out waiting for FrameReady for {tab_id:?}"),
      Err(RecvTimeoutError::Disconnected) => panic!("worker disconnected while waiting for frame"),
    }
  }
}

#[test]
fn tabs_do_not_leak_base_url_when_resolving_relative_css() {
  let dir = tempdir().expect("temp dir");

  let tab1_dir = dir.path().join("tab1");
  let tab2_dir = dir.path().join("tab2");
  std::fs::create_dir_all(&tab1_dir).expect("create tab1 dir");
  std::fs::create_dir_all(&tab2_dir).expect("create tab2 dir");

  let html = r#"<!doctype html>
    <html>
      <head>
        <link rel="stylesheet" href="style.css">
      </head>
      <body>
        <div id="fill"></div>
      </body>
    </html>
  "#;

  std::fs::write(tab1_dir.join("index.html"), html).expect("write tab1 html");
  std::fs::write(
    tab1_dir.join("style.css"),
    "html,body{margin:0;padding:0} #fill{width:2000px;height:2000px;background: rgb(255,0,0);}",
  )
  .expect("write tab1 css");

  std::fs::write(tab2_dir.join("index.html"), html).expect("write tab2 html");
  std::fs::write(
    tab2_dir.join("style.css"),
    "html,body{margin:0;padding:0} #fill{width:2000px;height:2000px;background: rgb(0,255,0);}",
  )
  .expect("write tab2 css");

  let tab1_url = format!("file://{}", tab1_dir.join("index.html").display());
  let tab2_url = format!("file://{}", tab2_dir.join("index.html").display());

  let handle = spawn_ui_worker("fastr-ui-worker-tab-resource-isolation").expect("spawn ui worker");
  let (ui_tx, ui_rx, join) = handle.split();

  let tab1 = TabId::new();
  ui_tx
    .send(create_tab_msg_with_cancel(tab1, None, CancelGens::new()))
    .expect("create tab1");
  ui_tx
    .send(viewport_changed_msg(tab1, (64, 64), 1.0))
    .expect("set viewport tab1");
  ui_tx
    .send(navigate_msg(tab1, tab1_url, NavigationReason::TypedUrl))
    .expect("navigate tab1");
  let frame1 = wait_for_frame(&ui_rx, tab1, Duration::from_secs(5));
  assert_eq!(pixel(&frame1.pixmap, 1, 1), (255, 0, 0, 255));

  let tab2 = TabId::new();
  ui_tx
    .send(create_tab_msg_with_cancel(tab2, None, CancelGens::new()))
    .expect("create tab2");
  ui_tx
    .send(viewport_changed_msg(tab2, (64, 64), 1.0))
    .expect("set viewport tab2");
  ui_tx
    .send(navigate_msg(tab2, tab2_url, NavigationReason::TypedUrl))
    .expect("navigate tab2");
  let frame2 = wait_for_frame(&ui_rx, tab2, Duration::from_secs(5));
  assert_eq!(pixel(&frame2.pixmap, 1, 1), (0, 255, 0, 255));

  // Drain any queued messages so the next FrameReady for tab1 is attributable to the repaint request.
  while ui_rx.try_recv().is_ok() {}

  ui_tx
    .send(UiToWorker::RequestRepaint {
      tab_id: tab1,
      reason: RepaintReason::Explicit,
    })
    .expect("repaint tab1");
  let frame1_repaint = wait_for_frame(&ui_rx, tab1, Duration::from_secs(5));
  assert_eq!(pixel(&frame1_repaint.pixmap, 1, 1), (255, 0, 0, 255));

  drop(ui_tx);
  join.join().expect("join worker thread");
}
