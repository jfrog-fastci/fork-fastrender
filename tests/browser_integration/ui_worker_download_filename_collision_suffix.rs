#![cfg(feature = "browser_ui")]

use super::support;
use fastrender::ui::messages::{
  NavigationReason, PointerButton, PointerModifiers, TabId, UiToWorker, WorkerToUi,
};
use fastrender::ui::spawn_ui_worker;
use std::path::PathBuf;
use std::time::Duration;
use tempfile::tempdir;
use url::Url;

const TIMEOUT: Duration = support::DEFAULT_TIMEOUT;

fn click_download_link(ui_tx: &std::sync::mpsc::Sender<UiToWorker>, tab_id: TabId) {
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
}

fn wait_for_download_success(
  ui_rx: &std::sync::mpsc::Receiver<WorkerToUi>,
  tab_id: TabId,
) -> PathBuf {
  let msg = support::recv_for_tab(ui_rx, tab_id, TIMEOUT, |msg| {
    matches!(
      msg,
      WorkerToUi::DownloadFinished {
        success: true,
        cancelled: false,
        ..
      }
    )
  })
  .unwrap_or_else(|| panic!("timed out waiting for successful DownloadFinished"));

  match msg {
    WorkerToUi::DownloadFinished { path: Some(path), .. } => path,
    other => panic!("unexpected worker message: {other:?}"),
  }
}

#[test]
fn ui_worker_download_filename_collision_suffix() {
  let _lock = super::stage_listener_test_lock();

  let site_dir = tempdir().expect("temp site dir");
  let download_dir = tempdir().expect("temp download dir");

  let payload_path = site_dir.path().join("hello.txt");
  let payload = b"hello world\n".to_vec();
  std::fs::write(&payload_path, &payload).expect("write payload");

  let html = r#"<!doctype html>
    <html>
      <head>
        <meta charset="utf-8">
        <style>
          html, body { margin: 0; padding: 0; }
          a { position: absolute; left: 0; top: 0; width: 200px; height: 40px; background: rgb(0, 255, 0); }
        </style>
      </head>
      <body>
        <a id="dl" download="hello.txt" href="hello.txt">download</a>
      </body>
    </html>
  "#;

  let page_path = site_dir.path().join("page.html");
  std::fs::write(&page_path, html).expect("write page");

  let page_url = Url::from_file_path(&page_path)
    .unwrap_or_else(|()| panic!("failed to build file:// url for {}", page_path.display()))
    .to_string();

  let handle = spawn_ui_worker("fastr-ui-worker-download-collision").expect("spawn ui worker");
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

  click_download_link(&ui_tx, tab_id);
  let first_path = wait_for_download_success(&ui_rx, tab_id);

  click_download_link(&ui_tx, tab_id);
  let second_path = wait_for_download_success(&ui_rx, tab_id);

  assert_ne!(
    first_path, second_path,
    "expected a different file path for the second download"
  );

  let first_name = first_path
    .file_name()
    .and_then(|n| n.to_str())
    .unwrap_or("<non-utf8>");
  let second_name = second_path
    .file_name()
    .and_then(|n| n.to_str())
    .unwrap_or("<non-utf8>");

  assert_eq!(first_name, "hello.txt");
  assert_eq!(
    second_name, "hello (1).txt",
    "expected Chrome-like suffix for the second download"
  );

  let first_contents = std::fs::read(&first_path).expect("read first download");
  let second_contents = std::fs::read(&second_path).expect("read second download");
  assert_eq!(first_contents, payload);
  assert_eq!(second_contents, payload);

  drop(ui_tx);
  join.join().expect("join ui worker thread");
}
