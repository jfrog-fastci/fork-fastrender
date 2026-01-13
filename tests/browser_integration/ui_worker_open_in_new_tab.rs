#![cfg(feature = "browser_ui")]

use super::support;
use fastrender::ui::messages::{
  NavigationReason, PointerButton, PointerModifiers, TabId, UiToWorker, WorkerToUi,
};
use fastrender::ui::spawn_ui_worker;
use std::time::Duration;
use tempfile::tempdir;
use url::Url;

const TIMEOUT: Duration = support::DEFAULT_TIMEOUT;

fn wait_for_open_in_new_tab(
  rx: &fastrender::ui::WorkerToUiInbox,
  tab_id: TabId,
  expected_url: &str,
) {
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

#[test]
fn link_activation_can_request_open_in_new_tab() {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
  let _lock = super::stage_listener_test_lock();
  let dir = tempdir().expect("temp dir");
  let page1_path = dir.path().join("page1.html");
  let page2_path = dir.path().join("page2.html");

  let page1 = r#"<!doctype html>
    <html>
      <head>
        <style>
          html, body { margin: 0; padding: 0; }
          #blank { position: absolute; left: 0; top: 0; width: 100px; height: 40px; background: rgb(255, 0, 0); }
          #same { position: absolute; left: 0; top: 50px; width: 100px; height: 40px; background: rgb(0, 0, 255); }
        </style>
      </head>
      <body>
        <a id="blank" href="page2.html" target="_blank">blank</a>
        <a id="same" href="page2.html">same</a>
      </body>
    </html>
  "#;
  let page2 = r#"<!doctype html>
    <html>
      <head>
        <style>
          html, body { margin: 0; padding: 0; background: rgb(0, 255, 0); }
        </style>
      </head>
      <body>Second</body>
    </html>
  "#;

  std::fs::write(&page1_path, page1).expect("write page1");
  std::fs::write(&page2_path, page2).expect("write page2");

  // Use `Url::from_file_path` so the test works on Windows (drive letters/backslashes) and properly
  // percent-encodes paths when needed.
  let page1_url = Url::from_file_path(&page1_path)
    .unwrap_or_else(|()| panic!("failed to build file:// url for {}", page1_path.display()))
    .to_string();
  let page2_url = Url::from_file_path(&page2_path)
    .unwrap_or_else(|()| panic!("failed to build file:// url for {}", page2_path.display()))
    .to_string();

  let handle = spawn_ui_worker("fastr-ui-worker-open-in-new-tab").expect("spawn ui worker");
  let (ui_tx, ui_rx, join) = handle.split();
  let tab_id = TabId::new();

  ui_tx.send(support::create_tab_msg(tab_id, None)).unwrap();
  ui_tx
    .send(support::viewport_changed_msg(tab_id, (200, 120), 1.0))
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

  // 1) `target=_blank` should request a new tab on a normal primary click.
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
  wait_for_open_in_new_tab(&ui_rx, tab_id, &page2_url);
  let _ = support::drain_for(&ui_rx, Duration::from_millis(100));

  // 2) Middle click on a normal link should request a new tab.
  ui_tx
    .send(UiToWorker::PointerDown {
      tab_id,
      pos_css: (10.0, 60.0),
      button: PointerButton::Middle,
      modifiers: PointerModifiers::NONE,
      click_count: 1,
    })
    .unwrap();
  ui_tx
    .send(UiToWorker::PointerUp {
      tab_id,
      pos_css: (10.0, 60.0),
      button: PointerButton::Middle,
      modifiers: PointerModifiers::NONE,
    })
    .unwrap();
  wait_for_open_in_new_tab(&ui_rx, tab_id, &page2_url);
  let _ = support::drain_for(&ui_rx, Duration::from_millis(100));

  // 3) Ctrl/Cmd+click on a normal link should request a new tab.
  let new_tab_modifiers = if cfg!(target_os = "macos") {
    PointerModifiers::META
  } else {
    PointerModifiers::CTRL
  };
  ui_tx
    .send(UiToWorker::PointerDown {
      tab_id,
      pos_css: (10.0, 60.0),
      button: PointerButton::Primary,
      modifiers: new_tab_modifiers,
      click_count: 1,
    })
    .unwrap();
  ui_tx
    .send(UiToWorker::PointerUp {
      tab_id,
      pos_css: (10.0, 60.0),
      button: PointerButton::Primary,
      modifiers: new_tab_modifiers,
    })
    .unwrap();
  wait_for_open_in_new_tab(&ui_rx, tab_id, &page2_url);
  let _ = support::drain_for(&ui_rx, Duration::from_millis(100));

  // 4) A normal primary click on a normal link should still navigate in the same tab.
  ui_tx
    .send(UiToWorker::PointerDown {
      tab_id,
      pos_css: (10.0, 60.0),
      button: PointerButton::Primary,
      modifiers: PointerModifiers::NONE,
      click_count: 1,
    })
    .unwrap();
  ui_tx
    .send(UiToWorker::PointerUp {
      tab_id,
      pos_css: (10.0, 60.0),
      button: PointerButton::Primary,
      modifiers: PointerModifiers::NONE,
    })
    .unwrap();

  let msg = support::recv_for_tab(&ui_rx, tab_id, TIMEOUT, |msg| {
    matches!(
      msg,
      WorkerToUi::RequestOpenInNewTab { .. }
        | WorkerToUi::NavigationStarted { .. }
        | WorkerToUi::NavigationFailed { .. }
    )
  })
  .unwrap_or_else(|| panic!("timed out waiting for navigation outcome after clicking normal link"));

  match msg {
    WorkerToUi::NavigationStarted {
      tab_id: got_tab,
      url,
    } => {
      assert_eq!(got_tab, tab_id);
      assert_eq!(url, page2_url);
    }
    WorkerToUi::RequestOpenInNewTab { url, .. } => {
      panic!("expected same-tab navigation, got RequestOpenInNewTab({url})");
    }
    WorkerToUi::NavigationFailed { url, error, .. } => {
      panic!("navigation failed for {url}: {error}");
    }
    other => panic!("unexpected WorkerToUi message: {other:?}"),
  }

  drop(ui_tx);
  join.join().expect("worker join");
}
