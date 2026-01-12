#![cfg(feature = "browser_ui")]

use super::support;
use fastrender::ui::cancel::CancelGens;
use fastrender::ui::messages::{KeyAction, PointerButton, PointerModifiers, TabId, UiToWorker, WorkerToUi};
use std::time::Duration;

// Worker startup + first render can take a few seconds under parallel load (CI).
const TIMEOUT: Duration = Duration::from_secs(20);

#[test]
fn browser_thread_date_picker_choose_updates_form_submission() {
  let _lock = super::stage_listener_test_lock();

  let site = support::TempSite::new();
  let html = r#"<!doctype html>
    <html>
      <head>
        <meta charset="utf-8">
        <style>
          html, body { margin: 0; padding: 0; }
          #d { position: absolute; left: 0; top: 0; width: 120px; height: 30px; }
          #submit { position: absolute; left: 0; top: 40px; width: 120px; height: 30px; }
        </style>
      </head>
      <body>
        <form method="get">
          <input id="d" type="date" name="d">
          <input id="submit" type="submit" value="Go">
        </form>
      </body>
    </html>
  "#;
  let url = site.write("page.html", html);

  let worker = fastrender::ui::spawn_browser_worker().expect("spawn browser worker");
  let fastrender::ui::BrowserWorkerHandle { tx, rx, join } = worker;

  let tab_id = TabId::new();
  tx.send(support::create_tab_msg_with_cancel(
    tab_id,
    Some(url),
    CancelGens::new(),
  ))
  .expect("CreateTab");
  tx.send(UiToWorker::SetActiveTab { tab_id })
    .expect("SetActiveTab");
  tx.send(support::viewport_changed_msg(tab_id, (240, 120), 1.0))
    .expect("ViewportChanged");

  // Wait for the first rendered frame so the tab has a live document.
  match support::recv_for_tab(&rx, tab_id, TIMEOUT, |msg| {
    matches!(msg, WorkerToUi::FrameReady { .. })
  }) {
    Some(WorkerToUi::FrameReady { .. }) => {}
    Some(other) => panic!("expected FrameReady, got {other:?}"),
    None => panic!("timed out waiting for FrameReady"),
  }

  // Clear any queued messages from the initial navigation/render.
  while rx.try_recv().is_ok() {}

  // Click within the date input control.
  let click_pos = (10.0, 10.0);
  tx.send(UiToWorker::PointerDown {
    tab_id,
    pos_css: click_pos,
    button: PointerButton::Primary,
    modifiers: PointerModifiers::NONE,
    click_count: 1,
  })
  .expect("PointerDown");
  tx.send(UiToWorker::PointerUp {
    tab_id,
    pos_css: click_pos,
    button: PointerButton::Primary,
    modifiers: PointerModifiers::NONE,
  })
  .expect("PointerUp");

  let msg = support::recv_for_tab(&rx, tab_id, TIMEOUT, |msg| {
    matches!(msg, WorkerToUi::DateTimePickerOpened { .. })
  })
  .expect("expected DateTimePickerOpened message");

  let WorkerToUi::DateTimePickerOpened {
    input_node_id,
    kind,
    ..
  } = msg
  else {
    unreachable!("filtered above");
  };
  assert_eq!(kind, fastrender::ui::messages::DateTimeInputKind::Date);

  tx.send(UiToWorker::DateTimePickerChoose {
    tab_id,
    input_node_id,
    value: "2020-01-02".to_string(),
  })
  .expect("DateTimePickerChoose");

  // Click submit.
  let submit_pos = (10.0, 50.0);
  tx.send(UiToWorker::PointerDown {
    tab_id,
    pos_css: submit_pos,
    button: PointerButton::Primary,
    modifiers: PointerModifiers::NONE,
    click_count: 1,
  })
  .expect("PointerDown submit");
  tx.send(UiToWorker::PointerUp {
    tab_id,
    pos_css: submit_pos,
    button: PointerButton::Primary,
    modifiers: PointerModifiers::NONE,
  })
  .expect("PointerUp submit");

  let nav = support::recv_for_tab(&rx, tab_id, TIMEOUT, |msg| {
    matches!(msg, WorkerToUi::NavigationStarted { .. })
  })
  .expect("expected NavigationStarted after submitting the form");

  let WorkerToUi::NavigationStarted { url, .. } = nav else {
    unreachable!("filtered above");
  };

  let parsed = url::Url::parse(&url).expect("parse navigation URL");
  let params: std::collections::HashMap<String, String> =
    parsed.query_pairs().into_owned().collect();
  assert_eq!(params.get("d"), Some(&"2020-01-02".to_string()));

  drop(tx);
  drop(rx);
  join.join().unwrap();
}

#[test]
fn browser_thread_date_picker_opens_on_keyboard_activation() {
  let _lock = super::stage_listener_test_lock();

  let site = support::TempSite::new();
  let html = r#"<!doctype html>
    <html>
      <head>
        <meta charset="utf-8">
        <style>
          html, body { margin: 0; padding: 0; }
        </style>
      </head>
      <body>
        <input type="date" name="d">
      </body>
    </html>
  "#;
  let url = site.write("page.html", html);

  let worker = fastrender::ui::spawn_browser_worker().expect("spawn browser worker");
  let fastrender::ui::BrowserWorkerHandle { tx, rx, join } = worker;

  let tab_id = TabId::new();
  tx.send(support::create_tab_msg_with_cancel(
    tab_id,
    Some(url),
    CancelGens::new(),
  ))
  .expect("CreateTab");
  tx.send(UiToWorker::SetActiveTab { tab_id })
    .expect("SetActiveTab");
  tx.send(support::viewport_changed_msg(tab_id, (240, 120), 1.0))
    .expect("ViewportChanged");

  match support::recv_for_tab(&rx, tab_id, TIMEOUT, |msg| {
    matches!(msg, WorkerToUi::FrameReady { .. })
  }) {
    Some(WorkerToUi::FrameReady { .. }) => {}
    Some(other) => panic!("expected FrameReady, got {other:?}"),
    None => panic!("timed out waiting for FrameReady"),
  }

  // Drain initial messages.
  while rx.try_recv().is_ok() {}

  // Focus the input via Tab, then activate it via Space (matching native controls).
  tx.send(UiToWorker::KeyAction {
    tab_id,
    key: KeyAction::Tab,
  })
  .expect("KeyAction Tab");
  tx.send(UiToWorker::KeyAction {
    tab_id,
    key: KeyAction::Space,
  })
  .expect("KeyAction Space");

  let msg = support::recv_for_tab(&rx, tab_id, TIMEOUT, |msg| {
    matches!(msg, WorkerToUi::DateTimePickerOpened { .. })
  })
  .expect("expected DateTimePickerOpened after keyboard activation");

  let WorkerToUi::DateTimePickerOpened { kind, .. } = msg else {
    unreachable!("filtered above");
  };
  assert_eq!(kind, fastrender::ui::messages::DateTimeInputKind::Date);

  drop(tx);
  drop(rx);
  join.join().unwrap();
}
