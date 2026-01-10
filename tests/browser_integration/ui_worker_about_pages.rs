#![cfg(feature = "browser_ui")]

use super::support::{create_tab_msg, navigate_msg, DEFAULT_TIMEOUT};
use fastrender::ui::messages::{NavigationReason, TabId, WorkerToUi};
use fastrender::ui::spawn_ui_worker;
use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant};

// Worker startup + the first navigation can take a few seconds under load when integration tests
// run in parallel on CI.
const TIMEOUT: Duration = DEFAULT_TIMEOUT;

fn wait_for_navigation_committed_and_frame(
  rx: &Receiver<WorkerToUi>,
  tab_id: TabId,
  expected_url: &str,
  expected_title: &str,
  timeout: Duration,
) {
  let deadline = Instant::now() + timeout;
  let mut committed = false;

  loop {
    let now = Instant::now();
    if now >= deadline {
      panic!("timed out waiting for NavigationCommitted+FrameReady for {expected_url}");
    }
    let remaining = deadline - now;
    match rx.recv_timeout(remaining) {
      Ok(msg) => match msg {
        WorkerToUi::NavigationCommitted {
          tab_id: msg_tab,
          url,
          title,
          ..
        } if msg_tab == tab_id && url == expected_url => {
          assert_eq!(
            title,
            Some(expected_title.to_string()),
            "unexpected title for {expected_url}"
          );
          committed = true;
        }
        WorkerToUi::NavigationFailed {
          tab_id: msg_tab,
          url,
          error,
          ..
        } if msg_tab == tab_id && url == expected_url => {
          panic!("navigation failed for {url}: {error}");
        }
        WorkerToUi::FrameReady { tab_id: msg_tab, .. } if committed && msg_tab == tab_id => return,
        _ => {}
      },
      Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
      Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
        panic!("worker channel disconnected while waiting for {expected_url}");
      }
    }
  }
}

#[test]
fn about_pages_render_and_have_titles() {
  let _lock = super::stage_listener_test_lock();
  let handle = spawn_ui_worker("ui_worker_about_pages").expect("spawn ui worker");
  let (ui_tx, ui_rx, join) = handle.split();

  let tab = TabId::new();
  ui_tx
    .send(create_tab_msg(tab, None))
    .expect("create tab");

  for (url, title) in [
    ("about:newtab", "New Tab"),
    ("about:help", "Help"),
    ("about:version", "Version"),
    ("about:gpu", "GPU"),
  ] {
    ui_tx
      .send(navigate_msg(tab, url.to_string(), NavigationReason::TypedUrl))
      .unwrap_or_else(|_| panic!("navigate to {url}"));
    wait_for_navigation_committed_and_frame(&ui_rx, tab, url, title, TIMEOUT);
  }

  drop(ui_tx);
  join.join().expect("join ui worker thread");
}

