#![cfg(feature = "browser_ui")]

use super::support;
use fastrender::ui::messages::{PointerButton, PointerModifiers, TabId, UiToWorker, WorkerToUi};
use fastrender::ui::spawn_ui_worker_with_factory;
use std::time::Duration;

const TIMEOUT: Duration = support::DEFAULT_TIMEOUT;

#[test]
fn ui_worker_shift_click_requests_open_in_new_window() {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
  let _lock = super::stage_listener_test_lock();

  let site = support::TempSite::new();
  let next_url = site.write(
    "next.html",
    r#"<!doctype html>
      <html>
        <body>next</body>
      </html>"#,
  );
  let index_url = site.write(
    "index.html",
    r#"<!doctype html>
      <html>
        <head>
          <style>
            html, body { margin: 0; padding: 0; }
            /* Deterministic hit target for pointer events. */
            #link {
              position: absolute;
              left: 0;
              top: 0;
              width: 120px;
              height: 40px;
              display: block;
              background: rgb(255, 0, 0);
            }
          </style>
        </head>
        <body>
          <a id="link" href="next.html">next</a>
        </body>
      </html>"#,
  );

  let handle = spawn_ui_worker_with_factory(
    "fastr-ui-worker-open-in-new-window",
    support::deterministic_factory(),
  )
  .expect("spawn ui worker");
  let (ui_tx, ui_rx, join) = handle.split();
  let tab_id = TabId::new();

  ui_tx
    .send(support::create_tab_msg(tab_id, Some(index_url.clone())))
    .expect("create tab");
  ui_tx
    .send(support::viewport_changed_msg(tab_id, (200, 100), 1.0))
    .expect("viewport");

  support::recv_for_tab(&ui_rx, tab_id, TIMEOUT, |msg| {
    matches!(msg, WorkerToUi::FrameReady { .. })
  })
  .unwrap_or_else(|| panic!("timed out waiting for FrameReady after navigating to {index_url}"));
  let _ = support::drain_for(&ui_rx, Duration::from_millis(100));

  // Shift+click should request opening in a new window.
  ui_tx
    .send(UiToWorker::PointerDown {
      tab_id,
      pos_css: (10.0, 10.0),
      button: PointerButton::Primary,
      modifiers: PointerModifiers::SHIFT,
      click_count: 1,
    })
    .expect("pointer down");
  ui_tx
    .send(UiToWorker::PointerUp {
      tab_id,
      pos_css: (10.0, 10.0),
      button: PointerButton::Primary,
      modifiers: PointerModifiers::SHIFT,
    })
    .expect("pointer up");

  let msg = support::recv_for_tab(&ui_rx, tab_id, TIMEOUT, |msg| {
    matches!(
      msg,
      WorkerToUi::RequestOpenInNewWindow { .. } | WorkerToUi::NavigationStarted { .. }
    )
  })
  .unwrap_or_else(|| panic!("timed out waiting for new-window request after shift-clicking link"));

  match msg {
    WorkerToUi::RequestOpenInNewWindow { tab_id: got, url } => {
      assert_eq!(got, tab_id);
      assert_eq!(url, next_url);
    }
    WorkerToUi::NavigationStarted { url, .. } => {
      panic!("expected RequestOpenInNewWindow, got NavigationStarted({url})");
    }
    other => panic!("unexpected WorkerToUi message: {other:?}"),
  }

  let msgs = support::drain_for(&ui_rx, Duration::from_millis(300));
  assert!(
    !msgs.iter().any(|msg| matches!(msg, WorkerToUi::NavigationStarted { .. })),
    "expected no same-tab navigation for shift-click; got:\n{}",
    support::format_messages(&msgs)
  );

  drop(ui_tx);
  join.join().expect("worker join");
}

#[test]
fn ui_worker_ctrl_shift_click_still_requests_open_in_new_tab() {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
  let _lock = super::stage_listener_test_lock();

  let site = support::TempSite::new();
  let next_url = site.write(
    "next.html",
    r#"<!doctype html>
      <html>
        <body>next</body>
      </html>"#,
  );
  let index_url = site.write(
    "index.html",
    r#"<!doctype html>
      <html>
        <head>
          <style>
            html, body { margin: 0; padding: 0; }
            #link {
              position: absolute;
              left: 0;
              top: 0;
              width: 120px;
              height: 40px;
              display: block;
              background: rgb(255, 0, 0);
            }
          </style>
        </head>
        <body>
          <a id="link" href="next.html">next</a>
        </body>
      </html>"#,
  );

  let handle = spawn_ui_worker_with_factory(
    "fastr-ui-worker-open-in-new-window-ctrl-shift",
    support::deterministic_factory(),
  )
  .expect("spawn ui worker");
  let (ui_tx, ui_rx, join) = handle.split();
  let tab_id = TabId::new();

  ui_tx
    .send(support::create_tab_msg(tab_id, Some(index_url.clone())))
    .expect("create tab");
  ui_tx
    .send(support::viewport_changed_msg(tab_id, (200, 100), 1.0))
    .expect("viewport");

  support::recv_for_tab(&ui_rx, tab_id, TIMEOUT, |msg| {
    matches!(msg, WorkerToUi::FrameReady { .. })
  })
  .unwrap_or_else(|| panic!("timed out waiting for FrameReady after navigating to {index_url}"));
  let _ = support::drain_for(&ui_rx, Duration::from_millis(100));

  let command_mod = if cfg!(target_os = "macos") {
    PointerModifiers::META
  } else {
    PointerModifiers::CTRL
  };
  let modifiers = command_mod | PointerModifiers::SHIFT;

  ui_tx
    .send(UiToWorker::PointerDown {
      tab_id,
      pos_css: (10.0, 10.0),
      button: PointerButton::Primary,
      modifiers,
      click_count: 1,
    })
    .expect("pointer down");
  ui_tx
    .send(UiToWorker::PointerUp {
      tab_id,
      pos_css: (10.0, 10.0),
      button: PointerButton::Primary,
      modifiers,
    })
    .expect("pointer up");

  let msg = support::recv_for_tab(&ui_rx, tab_id, TIMEOUT, |msg| {
    matches!(
      msg,
      WorkerToUi::RequestOpenInNewTab { .. } | WorkerToUi::RequestOpenInNewWindow { .. }
    )
  })
  .unwrap_or_else(|| panic!("timed out waiting for new-tab request after ctrl/cmd+shift click"));

  match msg {
    WorkerToUi::RequestOpenInNewTab { tab_id: got, url } => {
      assert_eq!(got, tab_id);
      assert_eq!(url, next_url);
    }
    WorkerToUi::RequestOpenInNewWindow { url, .. } => {
      panic!("expected RequestOpenInNewTab, got RequestOpenInNewWindow({url})");
    }
    other => panic!("unexpected WorkerToUi message: {other:?}"),
  }

  drop(ui_tx);
  join.join().expect("worker join");
}

#[test]
fn ui_worker_shift_click_respects_click_prevent_default() {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
  let _lock = super::stage_listener_test_lock();

  let site = support::TempSite::new();
  let next_url = site.write(
    "next.html",
    r#"<!doctype html>
      <html>
        <body>next</body>
      </html>"#,
  );
  let index_url = site.write(
    "index.html",
    r#"<!doctype html>
      <html>
        <head>
          <style>
            html, body { margin: 0; padding: 0; }
            #link {
              position: absolute;
              left: 0;
              top: 0;
              width: 120px;
              height: 40px;
              display: block;
              background: rgb(255, 0, 0);
            }
          </style>
        </head>
        <body>
          <a id="link" href="next.html">next</a>
          <script>
            document.getElementById("link").addEventListener("click", function (ev) {
              ev.preventDefault();
            });
          </script>
        </body>
      </html>"#,
  );

  let handle = spawn_ui_worker_with_factory(
    "fastr-ui-worker-open-in-new-window-prevent-default",
    support::deterministic_factory(),
  )
  .expect("spawn ui worker");
  let (ui_tx, ui_rx, join) = handle.split();
  let tab_id = TabId::new();

  ui_tx
    .send(support::create_tab_msg(tab_id, Some(index_url.clone())))
    .expect("create tab");
  ui_tx
    .send(support::viewport_changed_msg(tab_id, (200, 100), 1.0))
    .expect("viewport");

  support::recv_for_tab(&ui_rx, tab_id, TIMEOUT, |msg| {
    matches!(msg, WorkerToUi::FrameReady { .. })
  })
  .unwrap_or_else(|| panic!("timed out waiting for FrameReady after navigating to {index_url}"));
  let _ = support::drain_for(&ui_rx, Duration::from_millis(100));

  ui_tx
    .send(UiToWorker::PointerDown {
      tab_id,
      pos_css: (10.0, 10.0),
      button: PointerButton::Primary,
      modifiers: PointerModifiers::SHIFT,
      click_count: 1,
    })
    .expect("pointer down");
  ui_tx
    .send(UiToWorker::PointerUp {
      tab_id,
      pos_css: (10.0, 10.0),
      button: PointerButton::Primary,
      modifiers: PointerModifiers::SHIFT,
    })
    .expect("pointer up");

  let msgs = support::drain_for(&ui_rx, Duration::from_millis(500));
  assert!(
    !msgs.iter().any(|msg| matches!(
      msg,
      WorkerToUi::NavigationStarted { .. }
        | WorkerToUi::NavigationCommitted { .. }
        | WorkerToUi::NavigationFailed { .. }
        | WorkerToUi::RequestOpenInNewTab { .. }
        | WorkerToUi::RequestOpenInNewWindow { .. }
    )),
    "expected click preventDefault to suppress opening {next_url}; got:\n{}",
    support::format_messages(&msgs)
  );

  drop(ui_tx);
  join.join().expect("worker join");
}

