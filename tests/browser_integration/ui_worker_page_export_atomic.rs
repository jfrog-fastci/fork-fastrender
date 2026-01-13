#![cfg(feature = "browser_ui")]

use super::support;
use fastrender::ui::messages::{
  NavigationReason, PageExportKind, PageExportOutcome, TabId, UiToWorker, WorkerToUi,
};
use fastrender::ui::spawn_ui_worker;
use std::time::Duration;
use tempfile::tempdir;
use url::Url;

const TIMEOUT: Duration = support::DEFAULT_TIMEOUT;

#[test]
fn ui_worker_page_export_writes_atomically() {
  let _lock = super::stage_listener_test_lock();

  let site_dir = tempdir().expect("temp site dir");
  let export_dir = tempdir().expect("temp export dir");

  let html = r#"<!doctype html>
    <html>
      <head><meta charset="utf-8"></head>
      <body><p>Hello export</p></body>
    </html>
  "#;

  let page_path = site_dir.path().join("page.html");
  std::fs::write(&page_path, html).expect("write page");

  let page_url = Url::from_file_path(&page_path)
    .unwrap_or_else(|()| panic!("failed to build file:// url for {}", page_path.display()))
    .to_string();

  let handle = spawn_ui_worker("fastr-ui-worker-page-export-atomic").expect("spawn ui worker");
  let (ui_tx, ui_rx, join) = handle.split();
  let tab_id = TabId::new();

  ui_tx.send(support::create_tab_msg(tab_id, None)).unwrap();
  ui_tx
    .send(support::viewport_changed_msg(tab_id, (240, 120), 1.0))
    .unwrap();
  ui_tx
    .send(support::navigate_msg(
      tab_id,
      page_url.clone(),
      NavigationReason::TypedUrl,
    ))
    .unwrap();

  support::recv_for_tab(&ui_rx, tab_id, TIMEOUT, |msg| matches!(msg, WorkerToUi::FrameReady { .. }))
    .unwrap_or_else(|| panic!("timed out waiting for FrameReady after navigating to {page_url}"));
  let _ = support::drain_for(&ui_rx, Duration::from_millis(100));

  let final_path = export_dir.path().join("export.png");
  let part_path = export_dir.path().join("export.png.part");

  ui_tx
    .send(UiToWorker::PageExport {
      tab_id,
      kind: PageExportKind::SavePage,
      path: final_path.clone(),
    })
    .unwrap();

  let finished = support::recv_for_tab(&ui_rx, tab_id, TIMEOUT, |msg| {
    matches!(
      msg,
      WorkerToUi::PageExportFinished { path, .. } if *path == final_path
    )
  })
  .unwrap_or_else(|| panic!("timed out waiting for PageExportFinished for {}", final_path.display()));

  match finished {
    WorkerToUi::PageExportFinished { outcome, .. } => {
      assert_eq!(outcome, PageExportOutcome::Completed);
    }
    other => panic!("unexpected worker message: {other:?}"),
  }

  assert!(
    final_path.exists(),
    "expected export file at {}, but it does not exist",
    final_path.display()
  );
  assert!(
    !part_path.exists(),
    "expected no .part file after export, but {} exists",
    part_path.display()
  );

  let bytes = std::fs::read(&final_path).expect("read exported file");
  assert!(
    bytes.starts_with(b"\x89PNG\r\n\x1a\n"),
    "expected a PNG file at {}, but header was {:?}",
    final_path.display(),
    bytes.get(..8)
  );

  // Failure path: make the target path a directory so the finalize rename fails after writing the
  // `.part` file. The worker must clean up the part file.
  let fail_final = export_dir.path().join("blocked.png");
  std::fs::create_dir(&fail_final).expect("create blocking dir");
  let fail_part = export_dir.path().join("blocked.png.part");

  ui_tx
    .send(UiToWorker::PageExport {
      tab_id,
      kind: PageExportKind::SavePage,
      path: fail_final.clone(),
    })
    .unwrap();

  let failed = support::recv_for_tab(&ui_rx, tab_id, TIMEOUT, |msg| {
    matches!(
      msg,
      WorkerToUi::PageExportFinished { path, .. } if *path == fail_final
    )
  })
  .unwrap_or_else(|| panic!("timed out waiting for PageExportFinished for {}", fail_final.display()));

  match failed {
    WorkerToUi::PageExportFinished { outcome, .. } => match outcome {
      PageExportOutcome::Failed { error } => {
        assert!(
          !error.trim().is_empty(),
          "expected a non-empty error string for failed export"
        );
      }
      other => panic!("expected failed export outcome, got {other:?}"),
    },
    other => panic!("unexpected worker message: {other:?}"),
  }

  assert!(
    !fail_part.exists(),
    "expected no .part file after failed export, but {} exists",
    fail_part.display()
  );

  drop(ui_tx);
  join.join().expect("join ui worker thread");
}

