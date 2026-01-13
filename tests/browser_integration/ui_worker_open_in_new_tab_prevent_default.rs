#![cfg(feature = "browser_ui")]

use super::support;
use fastrender::ui::messages::{PointerButton, PointerModifiers, TabId, UiToWorker, WorkerToUi};
use fastrender::ui::spawn_ui_worker_with_factory;
use std::time::Duration;

const TIMEOUT: Duration = support::DEFAULT_TIMEOUT;

fn assert_no_navigation_or_new_tab(msgs: &[WorkerToUi], label: &str) {
  assert!(
    !msgs.iter().any(|msg| {
      matches!(
        msg,
        WorkerToUi::NavigationStarted { .. }
          | WorkerToUi::NavigationCommitted { .. }
          | WorkerToUi::NavigationFailed { .. }
          | WorkerToUi::RequestOpenInNewTab { .. }
          | WorkerToUi::RequestOpenInNewTabRequest { .. }
      )
    }),
    "{label}: expected click preventDefault() to suppress navigation/new-tab requests; got:\n{}",
    support::format_messages(msgs)
  );
}

#[test]
fn ui_worker_ctrl_click_open_in_new_tab_honors_click_prevent_default() {
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
            /* Give the link a predictable hit target so pointer events land on the <a>. */
            #link { position: absolute; left: 0; top: 0; width: 120px; height: 40px; display: block; background: rgb(255, 0, 0); }
          </style>
        </head>
        <body>
          <a id="link" href="next.html">next</a>
          <script>
            document.getElementById("link").addEventListener("click", function (ev) { ev.preventDefault(); });
          </script>
        </body>
      </html>"#,
  );

  let handle = spawn_ui_worker_with_factory(
    "fastr-ui-worker-open-in-new-tab-ctrl-prevent-default",
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

  // Wait for an initial frame so hit-testing has prepared layout artifacts.
  support::recv_for_tab(&ui_rx, tab_id, TIMEOUT, |msg| {
    matches!(msg, WorkerToUi::FrameReady { .. })
  })
  .unwrap_or_else(|| panic!("timed out waiting for FrameReady after navigating to {index_url}"));
  let _ = support::drain_for(&ui_rx, Duration::from_millis(100));

  let new_tab_modifiers = if cfg!(target_os = "macos") {
    PointerModifiers::META
  } else {
    PointerModifiers::CTRL
  };

  // Ctrl/Cmd+click should *normally* request a new tab, but JS preventDefault() must suppress it.
  ui_tx
    .send(UiToWorker::PointerDown {
      tab_id,
      pos_css: (10.0, 10.0),
      button: PointerButton::Primary,
      modifiers: new_tab_modifiers,
      click_count: 1,
    })
    .expect("pointer down");
  ui_tx
    .send(UiToWorker::PointerUp {
      tab_id,
      pos_css: (10.0, 10.0),
      button: PointerButton::Primary,
      modifiers: new_tab_modifiers,
    })
    .expect("pointer up");

  let msgs = support::drain_for(&ui_rx, Duration::from_millis(500));
  assert_no_navigation_or_new_tab(
    &msgs,
    &format!("ctrl/cmd click should not open a new tab for {next_url}"),
  );

  drop(ui_tx);
  join.join().expect("worker join");
}

#[test]
fn ui_worker_target_blank_open_in_new_tab_honors_click_prevent_default() {
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
            /* Give the link a predictable hit target so pointer events land on the <a>. */
            #link { position: absolute; left: 0; top: 0; width: 120px; height: 40px; display: block; background: rgb(255, 0, 0); }
          </style>
        </head>
        <body>
          <a id="link" href="next.html" target="_blank">next</a>
          <script>
            document.getElementById("link").addEventListener("click", function (ev) { ev.preventDefault(); });
          </script>
        </body>
      </html>"#,
  );

  let handle = spawn_ui_worker_with_factory(
    "fastr-ui-worker-open-in-new-tab-target-blank-prevent-default",
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

  // `target=_blank` should *normally* request a new tab, but JS preventDefault() must suppress it.
  ui_tx
    .send(support::pointer_down(
      tab_id,
      (10.0, 10.0),
      PointerButton::Primary,
    ))
    .expect("pointer down");
  ui_tx
    .send(support::pointer_up(tab_id, (10.0, 10.0), PointerButton::Primary))
    .expect("pointer up");

  let msgs = support::drain_for(&ui_rx, Duration::from_millis(500));
  assert_no_navigation_or_new_tab(
    &msgs,
    &format!("target=_blank click should not open a new tab for {next_url}"),
  );

  drop(ui_tx);
  join.join().expect("worker join");
}

