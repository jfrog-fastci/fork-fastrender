#![cfg(feature = "browser_ui")]

use fastrender::ui::messages::{NavigationReason, TabId, UiToWorker, WorkerToUi};
use fastrender::ui::worker_loop::spawn_ui_worker;
use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant};
use tempfile::tempdir;
use url::Url;

fn wait_for_navigation_committed(
  rx: &Receiver<WorkerToUi>,
  tab_id: TabId,
  timeout: Duration,
) -> (String, Option<String>) {
  let deadline = Instant::now() + timeout;
  loop {
    let now = Instant::now();
    if now >= deadline {
      panic!("timed out waiting for NavigationCommitted for tab {tab_id:?}");
    }
    let remaining = deadline - now;
    match rx.recv_timeout(remaining) {
      Ok(msg) => match msg {
        WorkerToUi::NavigationCommitted {
          tab_id: msg_tab,
          url,
          title,
          ..
        } if msg_tab == tab_id => return (url, title),
        _ => {}
      },
      Err(err) => panic!("timed out waiting for NavigationCommitted: {err}"),
    }
  }
}

#[test]
fn about_newtab_navigation_committed_includes_title() {
  let _lock = super::stage_listener_test_lock();
  let handle = spawn_ui_worker("ui_worker_title_about_newtab").expect("spawn ui worker");
  let (ui_tx, ui_rx, join) = handle.split();

  let tab = TabId::new();
  ui_tx
    .send(UiToWorker::CreateTab {
      tab_id: tab,
      initial_url: None,
      cancel: Default::default(),
    })
    .expect("create tab");
  ui_tx
    .send(UiToWorker::Navigate {
      tab_id: tab,
      url: "about:newtab".to_string(),
      reason: NavigationReason::TypedUrl,
    })
    .expect("navigate");

  let (_url, title) = wait_for_navigation_committed(&ui_rx, tab, Duration::from_secs(2));
  assert_eq!(title, Some("New Tab".to_string()));

  drop(ui_tx);
  join.join().expect("join ui worker thread");
}

#[test]
fn file_page_navigation_committed_includes_title_and_trims_ascii_whitespace() {
  let _lock = super::stage_listener_test_lock();
  let dir = tempdir().expect("temp dir");
  let path = dir.path().join("page.html");
  std::fs::write(
    &path,
    "<!doctype html><html><head><title>  Hello \n</title></head><body></body></html>",
  )
  .expect("write html");
  let url = Url::from_file_path(&path)
    .expect("file url")
    .to_string();

  let handle = spawn_ui_worker("ui_worker_title_file").expect("spawn ui worker");
  let (ui_tx, ui_rx, join) = handle.split();

  let tab = TabId::new();
  ui_tx
    .send(UiToWorker::CreateTab {
      tab_id: tab,
      initial_url: None,
      cancel: Default::default(),
    })
    .expect("create tab");
  ui_tx
    .send(UiToWorker::Navigate {
      tab_id: tab,
      url: url.clone(),
      reason: NavigationReason::TypedUrl,
    })
    .expect("navigate");

  let (committed_url, title) =
    wait_for_navigation_committed(&ui_rx, tab, Duration::from_secs(2));
  assert_eq!(
    Url::parse(&committed_url).expect("committed url parse"),
    Url::parse(&url).expect("expected url parse")
  );
  assert_eq!(title, Some("Hello".to_string()));

  drop(ui_tx);
  join.join().expect("join ui worker thread");
}

#[test]
fn missing_title_results_in_none() {
  let _lock = super::stage_listener_test_lock();
  let dir = tempdir().expect("temp dir");
  let path = dir.path().join("page.html");
  std::fs::write(&path, "<!doctype html><html><head></head><body></body></html>")
    .expect("write html");
  let url = Url::from_file_path(&path)
    .expect("file url")
    .to_string();

  let handle = spawn_ui_worker("ui_worker_title_missing_title").expect("spawn ui worker");
  let (ui_tx, ui_rx, join) = handle.split();

  let tab = TabId::new();
  ui_tx
    .send(UiToWorker::CreateTab {
      tab_id: tab,
      initial_url: None,
      cancel: Default::default(),
    })
    .expect("create tab");
  ui_tx
    .send(UiToWorker::Navigate {
      tab_id: tab,
      url,
      reason: NavigationReason::TypedUrl,
    })
    .expect("navigate");

  let (_url, title) = wait_for_navigation_committed(&ui_rx, tab, Duration::from_secs(2));
  assert_eq!(title, None);

  drop(ui_tx);
  join.join().expect("join ui worker thread");
}
