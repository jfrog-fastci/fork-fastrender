#![cfg(feature = "browser_ui")]

use super::support;
use fastrender::ui::cancel::CancelGens;
use fastrender::ui::messages::{
  DateTimeInputKind, KeyAction, PointerButton, PointerModifiers, TabId, UiToWorker, WorkerToUi,
};
use std::time::Duration;

// Worker startup + first render can take a few seconds under parallel load (CI).
const TIMEOUT: Duration = Duration::from_secs(20);

fn open_picker_choose_and_submit(
  input_type: &str,
  input_name: &str,
  expected_kind: DateTimeInputKind,
  chosen_value: &str,
) -> url::Url {
  let site = support::TempSite::new();
  let html = format!(
    r#"<!doctype html>
    <html>
      <head>
        <meta charset="utf-8">
        <style>
          html, body {{ margin: 0; padding: 0; }}
          #i {{ position: absolute; left: 0; top: 0; width: 120px; height: 30px; }}
          #submit {{ position: absolute; left: 0; top: 40px; width: 120px; height: 30px; }}
        </style>
      </head>
      <body>
        <form method="get">
          <input id="i" type="{input_type}" name="{input_name}">
          <input id="submit" type="submit" value="Go">
        </form>
      </body>
    </html>
  "#
  );
  let url = site.write("page.html", &html);

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

  // Click within the input control.
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
  assert_eq!(kind, expected_kind);

  tx.send(UiToWorker::DateTimePickerChoose {
    tab_id,
    input_node_id,
    value: chosen_value.to_string(),
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

  drop(tx);
  drop(rx);
  join.join().unwrap();

  parsed
}

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
fn browser_thread_time_picker_choose_updates_form_submission() {
  let _lock = super::stage_listener_test_lock();

  let parsed =
    open_picker_choose_and_submit("time", "t", DateTimeInputKind::Time, "12:34");
  assert_eq!(parsed.query(), Some("t=12%3A34"));
}

#[test]
fn browser_thread_datetime_local_picker_choose_updates_form_submission() {
  let _lock = super::stage_listener_test_lock();

  let parsed = open_picker_choose_and_submit(
    "datetime-local",
    "dt",
    DateTimeInputKind::DateTimeLocal,
    "2020-01-02T03:04",
  );
  assert_eq!(parsed.query(), Some("dt=2020-01-02T03%3A04"));
}

#[test]
fn browser_thread_month_picker_choose_updates_form_submission() {
  let _lock = super::stage_listener_test_lock();

  let parsed =
    open_picker_choose_and_submit("month", "m", DateTimeInputKind::Month, "2020-01");
  assert_eq!(parsed.query(), Some("m=2020-01"));
}

#[test]
fn browser_thread_week_picker_choose_updates_form_submission() {
  let _lock = super::stage_listener_test_lock();

  let parsed =
    open_picker_choose_and_submit("week", "w", DateTimeInputKind::Week, "2020-W01");
  assert_eq!(parsed.query(), Some("w=2020-W01"));
}

#[test]
fn browser_thread_date_picker_choose_invalid_value_sanitizes_to_empty() {
  let _lock = super::stage_listener_test_lock();

  let parsed =
    open_picker_choose_and_submit("date", "d", DateTimeInputKind::Date, "not-a-date");
  assert_eq!(parsed.query(), Some("d="));
}

#[test]
fn browser_thread_time_picker_choose_invalid_value_sanitizes_to_empty() {
  let _lock = super::stage_listener_test_lock();

  let parsed =
    open_picker_choose_and_submit("time", "t", DateTimeInputKind::Time, "not-a-date");
  assert_eq!(parsed.query(), Some("t="));
}

#[test]
fn browser_thread_datetime_local_picker_choose_invalid_value_sanitizes_to_empty() {
  let _lock = super::stage_listener_test_lock();

  let parsed = open_picker_choose_and_submit(
    "datetime-local",
    "dt",
    DateTimeInputKind::DateTimeLocal,
    "not-a-date",
  );
  assert_eq!(parsed.query(), Some("dt="));
}

#[test]
fn browser_thread_month_picker_choose_invalid_value_sanitizes_to_empty() {
  let _lock = super::stage_listener_test_lock();

  let parsed =
    open_picker_choose_and_submit("month", "m", DateTimeInputKind::Month, "not-a-date");
  assert_eq!(parsed.query(), Some("m="));
}

#[test]
fn browser_thread_week_picker_choose_invalid_value_sanitizes_to_empty() {
  let _lock = super::stage_listener_test_lock();

  let parsed =
    open_picker_choose_and_submit("week", "w", DateTimeInputKind::Week, "not-a-date");
  assert_eq!(parsed.query(), Some("w="));
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
  assert_eq!(kind, DateTimeInputKind::Date);

  drop(tx);
  drop(rx);
  join.join().unwrap();
}
