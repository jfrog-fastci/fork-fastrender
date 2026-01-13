#![cfg(feature = "browser_ui")]

use super::support;
use fastrender::ui::messages::{
  NavigationReason, PointerButton, PointerModifiers, TabId, UiToWorker, WorkerToUi,
};
use fastrender::ui::spawn_ui_worker;
use std::sync::mpsc::Receiver;
use std::time::Duration;
use tempfile::tempdir;
use url::Url;

const TIMEOUT: Duration = support::DEFAULT_TIMEOUT;

fn wait_for_open_in_new_tab(rx: &Receiver<WorkerToUi>, tab_id: TabId, expected_url: &str) {
  let msg = support::recv_for_tab(rx, tab_id, TIMEOUT, |msg| {
    matches!(msg, WorkerToUi::RequestOpenInNewTab { .. })
  })
  .unwrap_or_else(|| panic!("timed out waiting for RequestOpenInNewTab for tab {tab_id:?}"));

  match msg {
    WorkerToUi::RequestOpenInNewTab {
      tab_id: got_tab,
      url,
    } => {
      assert_eq!(got_tab, tab_id);
      assert_eq!(url, expected_url);
    }
    other => panic!("unexpected WorkerToUi message: {other:?}"),
  }
}

fn wait_for_navigation_failed_unsupported_scheme(rx: &Receiver<WorkerToUi>, tab_id: TabId, url: &str) {
  let msg = support::recv_for_tab(rx, tab_id, TIMEOUT, |msg| {
    matches!(msg, WorkerToUi::NavigationFailed { url: failed, .. } if failed == url)
  })
  .unwrap_or_else(|| panic!("timed out waiting for NavigationFailed for tab {tab_id:?} url {url}"));

  match msg {
    WorkerToUi::NavigationFailed {
      tab_id: got_tab,
      url: failed_url,
      error,
      ..
    } => {
      assert_eq!(got_tab, tab_id);
      assert_eq!(failed_url, url);

      let lower = error.to_ascii_lowercase();
      assert!(
        lower.contains("unsupported url scheme"),
        "expected error to mention unsupported scheme; got: {error}"
      );

      let scheme = Url::parse(url)
        .unwrap_or_else(|err| panic!("failed to parse url {url:?}: {err}"))
        .scheme()
        .to_ascii_lowercase();
      assert!(
        lower.contains(&scheme),
        "expected error to mention scheme {scheme:?}; got: {error}"
      );
    }
    other => panic!("unexpected WorkerToUi message: {other:?}"),
  }
}

#[test]
fn untrusted_open_in_new_tab_cannot_navigate_to_privileged_schemes() {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
  let _lock = super::stage_listener_test_lock();

  let dir = tempdir().expect("temp dir");
  let page1_path = dir.path().join("page1.html");

  let page1 = r#"<!doctype html>
    <html>
      <head>
        <style>
          html, body { margin: 0; padding: 0; }
          #blank_act { position: absolute; left: 0; top: 0; width: 200px; height: 40px; background: rgb(255, 0, 0); }
          #blank_chr { position: absolute; left: 0; top: 50px; width: 200px; height: 40px; background: rgb(0, 0, 255); }
        </style>
      </head>
      <body>
        <a id="blank_act" href="chrome-action:back" target="_blank">action</a>
        <a id="blank_chr" href="chrome://styles/chrome.css" target="_blank">chrome</a>
      </body>
    </html>
  "#;

  std::fs::write(&page1_path, page1).expect("write page1");

  let page1_url = Url::from_file_path(&page1_path)
    .unwrap_or_else(|()| panic!("failed to build file:// url for {}", page1_path.display()))
    .to_string();

  let handle =
    spawn_ui_worker("fastr-ui-worker-open-in-new-tab-privileged-scheme").expect("spawn ui worker");
  let (ui_tx, ui_rx, join) = handle.split();
  let tab_id = TabId::new();

  ui_tx.send(support::create_tab_msg(tab_id, None)).unwrap();
  ui_tx
    .send(support::viewport_changed_msg(tab_id, (240, 140), 1.0))
    .unwrap();
  ui_tx
    .send(support::navigate_msg(
      tab_id,
      page1_url.clone(),
      NavigationReason::TypedUrl,
    ))
    .unwrap();

  // Wait for an initial frame so hit-testing has prepared layout artifacts.
  support::recv_for_tab(&ui_rx, tab_id, TIMEOUT, |msg| {
    matches!(msg, WorkerToUi::FrameReady { .. })
  })
  .unwrap_or_else(|| panic!("timed out waiting for FrameReady after navigating to {page1_url}"));

  // Drain any follow-up messages from the initial navigation to keep assertions scoped to the click.
  let _ = support::drain_for(&ui_rx, Duration::from_millis(100));

  for (expected_url, pos_css) in [
    ("chrome-action:back", (10.0, 10.0)),
    ("chrome://styles/chrome.css", (10.0, 60.0)),
  ] {
    // Click the `target=_blank` link in the untrusted page; the worker should ask the UI to open a
    // new tab.
    ui_tx
      .send(UiToWorker::PointerDown {
        tab_id,
        pos_css,
        button: PointerButton::Primary,
        modifiers: PointerModifiers::NONE,
        click_count: 1,
      })
      .unwrap();
    ui_tx
      .send(UiToWorker::PointerUp {
        tab_id,
        pos_css,
        button: PointerButton::Primary,
        modifiers: PointerModifiers::NONE,
      })
      .unwrap();

    wait_for_open_in_new_tab(&ui_rx, tab_id, expected_url);
    let _ = support::drain_for(&ui_rx, Duration::from_millis(100));

    // Simulate the UI opening the requested URL in a new tab. The worker must fail safely rather
    // than executing any privileged chrome actions.
    let new_tab = TabId::new();
    ui_tx.send(support::create_tab_msg(new_tab, None)).unwrap();
    ui_tx
      .send(support::viewport_changed_msg(new_tab, (240, 140), 1.0))
      .unwrap();
    ui_tx
      .send(support::navigate_msg(
        new_tab,
        expected_url.to_string(),
        NavigationReason::LinkClick,
      ))
      .unwrap();

    wait_for_navigation_failed_unsupported_scheme(&ui_rx, new_tab, expected_url);
    let _ = support::drain_for(&ui_rx, Duration::from_millis(100));
  }

  drop(ui_tx);
  join.join().expect("worker join");
}

