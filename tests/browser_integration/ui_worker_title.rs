#![cfg(feature = "browser_ui")]

use super::support::{create_tab_msg, navigate_msg, DEFAULT_TIMEOUT};
use fastrender::ui::messages::{NavigationReason, TabId, WorkerToUi};
use fastrender::ui::spawn_ui_worker;
use std::time::{Duration, Instant};
use tempfile::tempdir;
use url::Url;

// Worker startup + the first navigation can take a few seconds under load when integration tests
// run in parallel on CI.
const TIMEOUT: Duration = DEFAULT_TIMEOUT;

fn wait_for_navigation_committed(
  rx: &fastrender::ui::WorkerToUiInbox,
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
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
  let _lock = super::stage_listener_test_lock();
  let handle = spawn_ui_worker("ui_worker_title_about_newtab").expect("spawn ui worker");
  let (ui_tx, ui_rx, join) = handle.split();

  let tab = TabId::new();
  ui_tx.send(create_tab_msg(tab, None)).expect("create tab");
  ui_tx
    .send(navigate_msg(
      tab,
      "about:newtab".to_string(),
      NavigationReason::TypedUrl,
    ))
    .expect("navigate");

  let (_url, title) = wait_for_navigation_committed(&ui_rx, tab, TIMEOUT);
  assert_eq!(title, Some("New Tab".to_string()));

  drop(ui_tx);
  join.join().expect("join ui worker thread");
}

#[test]
fn file_page_navigation_committed_includes_title_and_trims_ascii_whitespace() {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
  let _lock = super::stage_listener_test_lock();
  let dir = tempdir().expect("temp dir");
  let path = dir.path().join("page.html");
  std::fs::write(
    &path,
    "<!doctype html><html><head><title>  Hello \n</title></head><body></body></html>",
  )
  .expect("write html");
  let url = Url::from_file_path(&path).expect("file url").to_string();

  let handle = spawn_ui_worker("ui_worker_title_file").expect("spawn ui worker");
  let (ui_tx, ui_rx, join) = handle.split();

  let tab = TabId::new();
  ui_tx.send(create_tab_msg(tab, None)).expect("create tab");
  ui_tx
    .send(navigate_msg(tab, url.clone(), NavigationReason::TypedUrl))
    .expect("navigate");

  let (committed_url, title) = wait_for_navigation_committed(&ui_rx, tab, TIMEOUT);
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
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
  let _lock = super::stage_listener_test_lock();
  let dir = tempdir().expect("temp dir");
  let path = dir.path().join("page.html");
  std::fs::write(
    &path,
    "<!doctype html><html><head></head><body></body></html>",
  )
  .expect("write html");
  let url = Url::from_file_path(&path).expect("file url").to_string();

  let handle = spawn_ui_worker("ui_worker_title_missing_title").expect("spawn ui worker");
  let (ui_tx, ui_rx, join) = handle.split();

  let tab = TabId::new();
  ui_tx.send(create_tab_msg(tab, None)).expect("create tab");
  ui_tx
    .send(navigate_msg(tab, url, NavigationReason::TypedUrl))
    .expect("navigate");

  let (_url, title) = wait_for_navigation_committed(&ui_rx, tab, TIMEOUT);
  assert_eq!(title, None);

  drop(ui_tx);
  join.join().expect("join ui worker thread");
}
