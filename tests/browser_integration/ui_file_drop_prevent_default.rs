#![cfg(feature = "browser_ui")]

use super::support;
use fastrender::ui::messages::{RepaintReason, TabId, UiToWorker, WorkerToUi};
use fastrender::ui::spawn_ui_worker_with_factory;
use fastrender::Result;
use std::time::Duration;

const TIMEOUT: Duration = support::DEFAULT_TIMEOUT;

#[test]
fn ui_worker_drop_files_prevent_default_suppresses_file_input_selection() -> Result<()> {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
  let _lock = super::stage_listener_test_lock();

  let dir = tempfile::tempdir()?;
  let file_path = dir.path().join("hello.txt");
  std::fs::write(&file_path, b"hello world")?;

  let site = support::TempSite::new();
  let index_url = site.write(
    "index.html",
    r#"<!doctype html>
<html>
  <head>
    <meta charset="utf-8">
    <style>
      html, body { margin: 0; padding: 0; }
      #f { position: absolute; left: 0; top: 0; width: 140px; height: 40px; }
      #marker { position: absolute; left: 0; top: 60px; width: 140px; height: 40px; background: rgb(0, 255, 0); }
      #f[data-fastr-file-value] ~ #marker { background: rgb(255, 0, 0); }
    </style>
  </head>
  <body>
    <input id="f" type="file">
    <div id="marker"></div>
    <script>
      const f = document.getElementById("f");
      f.addEventListener("drop", (e) => {
        e.preventDefault();
      });
    </script>
  </body>
</html>
"#,
  );

  let handle = spawn_ui_worker_with_factory(
    "fastr-ui-worker-drop-files-prevent-default",
    support::deterministic_factory(),
  )?;
  let (ui_tx, ui_rx, join) = handle.split();

  let tab_id = TabId::new();
  ui_tx
    .send(support::create_tab_msg(tab_id, Some(index_url.clone())))
    .expect("create tab");
  ui_tx
    .send(support::viewport_changed_msg(tab_id, (200, 120), 1.0))
    .expect("viewport");

  // Wait for the first paint so hit-testing has layout artifacts.
  support::recv_for_tab(&ui_rx, tab_id, TIMEOUT, |msg| {
    matches!(msg, WorkerToUi::FrameReady { .. })
  })
  .unwrap_or_else(|| panic!("timed out waiting for initial FrameReady for {index_url}"));

  // Drop a file onto the input. The page's `drop` handler calls `preventDefault()`, so the worker
  // must not update the underlying file input selection state.
  ui_tx
    .send(UiToWorker::DropFiles {
      tab_id,
      pos_css: (10.0, 10.0),
      paths: vec![file_path.clone()],
    })
    .expect("drop files");

  // Force a repaint so we can observe whether the input's internal selection attribute changed.
  ui_tx
    .send(support::request_repaint(tab_id, RepaintReason::Explicit))
    .expect("request repaint");

  let msg = support::recv_for_tab(&ui_rx, tab_id, TIMEOUT, |msg| {
    matches!(msg, WorkerToUi::FrameReady { .. })
  })
  .unwrap_or_else(|| panic!("timed out waiting for FrameReady after dropping files"));

  let frame = match msg {
    WorkerToUi::FrameReady { frame, .. } => frame,
    other => panic!("unexpected message while waiting for FrameReady: {other:?}"),
  };

  // Marker background is green when the file input has no `data-fastr-file-value`, and red when
  // the default drop handler selected a file.
  let pixel = support::rgba_at(&frame.pixmap, 10, 70);
  assert_eq!(
    pixel,
    [0, 255, 0, 255],
    "expected marker to remain green when drop is prevented; got {pixel:?}"
  );

  drop(ui_tx);
  join.join().expect("worker join");
  Ok(())
}

