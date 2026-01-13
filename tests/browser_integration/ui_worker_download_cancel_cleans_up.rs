#![cfg(feature = "browser_ui")]

use super::support;
use fastrender::ui::messages::{
  DownloadOutcome, NavigationReason, PointerButton, PointerModifiers, TabId, UiToWorker, WorkerToUi,
};
use fastrender::ui::spawn_ui_worker;
use std::path::Path;
use std::time::Duration;
use tempfile::tempdir;
use url::Url;

const TIMEOUT: Duration = support::DEFAULT_TIMEOUT;

fn assert_path_in_download_dir(path: &Path, download_dir: &Path) {
  let download_dir = std::fs::canonicalize(download_dir).unwrap_or_else(|err| {
    panic!(
      "failed to canonicalize download dir {}: {err}",
      download_dir.display()
    )
  });
  let path = if path.exists() {
    std::fs::canonicalize(path).unwrap_or_else(|err| {
      panic!(
        "failed to canonicalize download path {}: {err}",
        path.display()
      )
    })
  } else {
    // During cancellation tests we may assert the final path before the download is finalized
    // (since the worker writes to a sibling `*.part` file). In that case the final file may not
    // exist, so canonicalize the existing parent directory instead.
    let parent = path.parent().unwrap_or_else(|| {
      panic!(
        "download path {} has no parent; expected it to live under {}",
        path.display(),
        download_dir.display()
      )
    });
    std::fs::canonicalize(parent).unwrap_or_else(|err| {
      panic!(
        "failed to canonicalize download parent dir {} (for {}): {err}",
        parent.display(),
        path.display()
      )
    })
  };
  assert!(
    path.starts_with(&download_dir),
    "expected download path {} to be inside download dir {}",
    path.display(),
    download_dir.display()
  );
}

#[test]
fn ui_worker_download_cancel_cleans_up() {
  let _lock = super::stage_listener_test_lock();

  let site_dir = tempdir().expect("temp site dir");
  let download_dir = tempdir().expect("temp download dir");

  // Create a deterministic "large" payload so the test can observe progress before completion.
  let payload_path = site_dir.path().join("payload.bin");
  let payload = vec![0xABu8; 3 * 1024 * 1024];
  std::fs::write(&payload_path, &payload).expect("write payload");

  let html = r#"<!doctype html>
    <html>
      <head>
        <meta charset="utf-8">
        <style>
          html, body { margin: 0; padding: 0; }
          a { position: absolute; left: 0; top: 0; width: 200px; height: 40px; background: rgb(255, 0, 0); }
        </style>
      </head>
      <body>
        <a id="dl" download="payload.bin" href="payload.bin">download</a>
      </body>
    </html>
  "#;

  let page_path = site_dir.path().join("page.html");
  std::fs::write(&page_path, html).expect("write page");

  let page_url = Url::from_file_path(&page_path)
    .unwrap_or_else(|()| panic!("failed to build file:// url for {}", page_path.display()))
    .to_string();

  let handle = spawn_ui_worker("fastr-ui-worker-download-cancel").expect("spawn ui worker");
  let (ui_tx, ui_rx, join) = handle.split();
  let tab_id = TabId::new();

  ui_tx
    .send(UiToWorker::SetDownloadDirectory {
      path: download_dir.path().to_path_buf(),
    })
    .unwrap();
  ui_tx.send(support::create_tab_msg(tab_id, None)).unwrap();
  ui_tx
    .send(support::viewport_changed_msg(tab_id, (240, 80), 1.0))
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

  // Click the download link.
  ui_tx
    .send(UiToWorker::PointerDown {
      tab_id,
      pos_css: (10.0, 10.0),
      button: PointerButton::Primary,
      modifiers: PointerModifiers::NONE,
      click_count: 1,
    })
    .unwrap();
  ui_tx
    .send(UiToWorker::PointerUp {
      tab_id,
      pos_css: (10.0, 10.0),
      button: PointerButton::Primary,
      modifiers: PointerModifiers::NONE,
    })
    .unwrap();

  let started = support::recv_for_tab(&ui_rx, tab_id, TIMEOUT, |msg| {
    matches!(msg, WorkerToUi::DownloadStarted { .. })
  })
  .unwrap_or_else(|| panic!("timed out waiting for DownloadStarted after clicking download link"));

  let (download_id, path) = match started {
    WorkerToUi::DownloadStarted {
      download_id, path, ..
    } => (download_id, path),
    other => panic!("unexpected worker message: {other:?}"),
  };

  assert_path_in_download_dir(&path, download_dir.path());

  // Wait for progress so we know the download thread actually began writing to disk (cancellation
  // before the first write would make cleanup assertions vacuous).
  support::recv_for_tab(&ui_rx, tab_id, TIMEOUT, |msg| {
    matches!(
      msg,
      WorkerToUi::DownloadProgress {
        download_id: got,
        received_bytes,
        ..
      } if *got == download_id && *received_bytes > 0
    )
  })
  .unwrap_or_else(|| {
    panic!("timed out waiting for DownloadProgress after DownloadStarted for download {download_id:?}")
  });

  ui_tx
    .send(UiToWorker::CancelDownload { tab_id, download_id })
    .unwrap();

  let finished = support::recv_for_tab(&ui_rx, tab_id, TIMEOUT, |msg| {
    matches!(
      msg,
      WorkerToUi::DownloadFinished {
        download_id: got,
        ..
      } if *got == download_id
    )
  })
  .unwrap_or_else(|| panic!("timed out waiting for DownloadFinished for download {download_id:?}"));

  match finished {
    WorkerToUi::DownloadFinished { outcome, .. } => {
      assert_eq!(outcome, DownloadOutcome::Cancelled);
    }
    other => panic!("unexpected worker message: {other:?}"),
  }

  let final_path = &path;
  let part_path = fastrender::ui::downloads::part_path_for_final(final_path);
  assert!(
    !final_path.exists(),
    "expected no final file after cancel, but {} exists",
    final_path.display()
  );
  assert!(
    !part_path.exists(),
    "expected no .part file after cancel, but {} exists",
    part_path.display()
  );

  drop(ui_tx);
  join.join().expect("join ui worker thread");
}
