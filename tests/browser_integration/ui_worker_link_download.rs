#![cfg(feature = "browser_ui")]

use super::support;
use fastrender::ui::messages::{DownloadOutcome, PointerButton, TabId, UiToWorker, WorkerToUi};
use fastrender::ui::spawn_ui_worker_with_factory;
use std::sync::mpsc::RecvTimeoutError;
use std::time::{Duration, Instant};
use tempfile::tempdir;

const TIMEOUT: Duration = support::DEFAULT_TIMEOUT;

fn click_primary(ui_tx: &std::sync::mpsc::Sender<UiToWorker>, tab_id: TabId) {
  ui_tx
    .send(support::pointer_down(
      tab_id,
      (10.0, 10.0),
      PointerButton::Primary,
    ))
    .expect("pointer down");
  ui_tx
    .send(support::pointer_up(
      tab_id,
      (10.0, 10.0),
      PointerButton::Primary,
    ))
    .expect("pointer up");
}

#[test]
fn ui_worker_link_download_triggers_download_without_navigation() {
  let _lock = super::stage_listener_test_lock();

  let site = support::TempSite::new();

  let payload = "download-payload\n";
  let _file_url = site.write("file.txt", payload);
  let index_url = site.write(
    "index.html",
    r#"<!doctype html>
      <html>
        <head>
          <meta charset="utf-8">
          <style>
            html, body { margin: 0; padding: 0; }
            /* Give the link a predictable hit target so pointer events land on the <a>. */
            #dl { position: absolute; left: 0; top: 0; width: 240px; height: 80px; display: block; background: rgb(255, 0, 0); }
          </style>
        </head>
        <body>
          <a id="dl" href="file.txt" download="myname.txt">Download</a>
        </body>
      </html>"#,
  );

  let download_dir = tempdir().expect("temp download dir");

  let handle = spawn_ui_worker_with_factory(
    "fastr-ui-worker-link-download-default-action",
    support::deterministic_factory(),
  )
  .expect("spawn ui worker");
  let (ui_tx, ui_rx, join) = handle.split();
  let tab_id = TabId::new();

  ui_tx
    .send(UiToWorker::SetDownloadDirectory {
      path: download_dir.path().to_path_buf(),
    })
    .expect("set download dir");

  ui_tx
    .send(support::create_tab_msg(tab_id, Some(index_url.clone())))
    .expect("create tab");
  ui_tx
    .send(support::viewport_changed_msg(tab_id, (300, 120), 1.0))
    .expect("viewport");

  support::recv_for_tab(&ui_rx, tab_id, TIMEOUT, |msg| {
    matches!(msg, WorkerToUi::FrameReady { .. })
  })
  .unwrap_or_else(|| panic!("timed out waiting for FrameReady after navigating to {index_url}"));

  // Drain any follow-up messages from the initial navigation to keep assertions scoped to the click.
  let _ = support::drain_for(&ui_rx, Duration::from_millis(100));

  click_primary(&ui_tx, tab_id);

  // Collect UI messages until the download finishes (or we time out), and assert that navigation
  // doesn't happen as part of clicking an `<a download>`.
  let start = Instant::now();
  let mut msgs: Vec<WorkerToUi> = Vec::new();
  let mut download_id = None;
  let mut download_path = None;
  let mut download_file_name: Option<String> = None;
  let mut finished_outcome: Option<DownloadOutcome> = None;

  while start.elapsed() < TIMEOUT {
    match ui_rx.recv_timeout(Duration::from_millis(25)) {
      Ok(msg) => {
        // Fail fast if the click caused a navigation instead of a download.
        match &msg {
          WorkerToUi::NavigationStarted { tab_id: got, .. }
          | WorkerToUi::NavigationCommitted { tab_id: got, .. }
          | WorkerToUi::NavigationFailed { tab_id: got, .. }
            if *got == tab_id =>
          {
            msgs.push(msg);
            panic!(
              "expected `<a download>` click to avoid same-tab navigation; got:\n{}",
              support::format_messages(&msgs)
            );
          }
          _ => {}
        }

        match &msg {
          WorkerToUi::DownloadStarted {
            tab_id: got,
            download_id: id,
            file_name,
            path,
            ..
          } if *got == tab_id => {
            download_id = Some(*id);
            download_path = Some(path.clone());
            download_file_name = Some(file_name.clone());
          }
          WorkerToUi::DownloadFinished {
            tab_id: got,
            download_id: id,
            outcome,
          } if *got == tab_id => {
            if Some(*id) == download_id {
              finished_outcome = Some(outcome.clone());
              msgs.push(msg);
              break;
            }
          }
          _ => {}
        }

        msgs.push(msg);
      }
      Err(RecvTimeoutError::Timeout) => {}
      Err(RecvTimeoutError::Disconnected) => break,
    }
  }

  let Some(download_path) = download_path else {
    panic!(
      "timed out waiting for DownloadStarted after clicking `<a download>`; got:\n{}",
      support::format_messages(&msgs)
    );
  };
  assert_eq!(
    download_file_name.as_deref(),
    Some("myname.txt"),
    "expected download attribute to control suggested filename; got:\n{}",
    support::format_messages(&msgs)
  );
  assert_eq!(
    finished_outcome,
    Some(DownloadOutcome::Completed),
    "expected download to complete successfully; got:\n{}",
    support::format_messages(&msgs)
  );

  assert!(
    download_path.starts_with(download_dir.path()),
    "expected download path to be inside {} but got {}",
    download_dir.path().display(),
    download_path.display()
  );
  let downloaded = std::fs::read(&download_path)
    .unwrap_or_else(|err| panic!("read downloaded file {}: {err}", download_path.display()));
  assert_eq!(downloaded, payload.as_bytes());

  drop(ui_tx);
  join.join().expect("join ui worker thread");
}

#[test]
fn ui_worker_link_download_prevent_default_cancels_download() {
  let _lock = super::stage_listener_test_lock();

  let site = support::TempSite::new();

  let payload = "download-payload\n";
  let _file_url = site.write("file.txt", payload);
  let index_url = site.write(
    "index.html",
    r#"<!doctype html>
      <html>
        <head>
          <meta charset="utf-8">
          <style>
            html, body { margin: 0; padding: 0; }
            #dl { position: absolute; left: 0; top: 0; width: 240px; height: 80px; display: block; background: rgb(0, 255, 0); }
          </style>
        </head>
        <body>
          <a id="dl" href="file.txt" download="myname.txt">Download</a>
          <script>
            document.getElementById("dl").addEventListener("click", function (ev) {
              ev.preventDefault();
            });
          </script>
        </body>
      </html>"#,
  );

  let download_dir = tempdir().expect("temp download dir");

  let handle = spawn_ui_worker_with_factory(
    "fastr-ui-worker-link-download-prevent-default",
    support::deterministic_factory(),
  )
  .expect("spawn ui worker");
  let (ui_tx, ui_rx, join) = handle.split();
  let tab_id = TabId::new();

  ui_tx
    .send(UiToWorker::SetDownloadDirectory {
      path: download_dir.path().to_path_buf(),
    })
    .expect("set download dir");

  ui_tx
    .send(support::create_tab_msg(tab_id, Some(index_url.clone())))
    .expect("create tab");
  ui_tx
    .send(support::viewport_changed_msg(tab_id, (300, 120), 1.0))
    .expect("viewport");

  support::recv_for_tab(&ui_rx, tab_id, TIMEOUT, |msg| {
    matches!(msg, WorkerToUi::FrameReady { .. })
  })
  .unwrap_or_else(|| panic!("timed out waiting for FrameReady after navigating to {index_url}"));
  let _ = support::drain_for(&ui_rx, Duration::from_millis(100));

  click_primary(&ui_tx, tab_id);

  let msgs = support::drain_for(&ui_rx, Duration::from_secs(2));
  assert!(
    !msgs.iter().any(|msg| {
      matches!(
        msg,
        WorkerToUi::DownloadStarted { .. }
          | WorkerToUi::NavigationStarted { .. }
          | WorkerToUi::NavigationCommitted { .. }
          | WorkerToUi::NavigationFailed { .. }
      )
    }),
    "expected click preventDefault to suppress `<a download>` default action; got:\n{}",
    support::format_messages(&msgs)
  );

  let entries = std::fs::read_dir(download_dir.path())
    .expect("read download dir")
    .collect::<Result<Vec<_>, _>>()
    .expect("read download dir entries");
  assert!(
    entries.is_empty(),
    "expected no files created in download dir after preventDefault, but found {} entries",
    entries.len()
  );

  drop(ui_tx);
  join.join().expect("join ui worker thread");
}
