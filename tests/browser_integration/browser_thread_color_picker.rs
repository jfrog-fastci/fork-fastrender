#![cfg(feature = "browser_ui")]

use super::support;
use fastrender::ui::cancel::CancelGens;
use fastrender::ui::messages::{KeyAction, TabId, UiToWorker, WorkerToUi};
use std::time::Duration;

// Worker startup + first render can take a few seconds under parallel load (CI).
const TIMEOUT: Duration = Duration::from_secs(20);

#[test]
fn browser_thread_color_picker_keyboard_activation_opens() {
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
        <input type="color" name="c">
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
  match support::recv_until(&rx, TIMEOUT, |msg| {
    matches!(msg, WorkerToUi::FrameReady { tab_id: t, .. } if *t == tab_id)
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

  support::recv_until(&rx, TIMEOUT, |msg| {
    matches!(msg, WorkerToUi::ColorPickerOpened { tab_id: t, .. } if *t == tab_id)
  })
  .expect("expected ColorPickerOpened after keyboard activation");

  tx.send(UiToWorker::ColorPickerCancel { tab_id })
    .expect("ColorPickerCancel");

  support::recv_until(&rx, TIMEOUT, |msg| {
    matches!(msg, WorkerToUi::ColorPickerClosed { tab_id: t, .. } if *t == tab_id)
  })
  .expect("expected ColorPickerClosed after cancel");

  drop(tx);
  drop(rx);
  join.join().unwrap();
}

#[test]
fn browser_thread_color_picker_choose_updates_form_submission() {
  let _lock = super::stage_listener_test_lock();

  let site = support::TempSite::new();
  let html = r#"<!doctype html>
    <html>
      <head>
        <meta charset="utf-8">
        <style>
          html, body { margin: 0; padding: 0; }
          #c { position: absolute; left: 0; top: 0; width: 120px; height: 30px; }
          #submit { position: absolute; left: 0; top: 40px; width: 120px; height: 30px; }
        </style>
      </head>
      <body>
        <form method="get">
          <input id="c" type="color" name="c">
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
  match support::recv_until(&rx, TIMEOUT, |msg| {
    matches!(msg, WorkerToUi::FrameReady { tab_id: t, .. } if *t == tab_id)
  }) {
    Some(WorkerToUi::FrameReady { .. }) => {}
    Some(other) => panic!("expected FrameReady, got {other:?}"),
    None => panic!("timed out waiting for FrameReady"),
  }

  // Clear any queued messages from the initial navigation/render.
  while rx.try_recv().is_ok() {}

  // Click within the color input control.
  let click_pos = (10.0, 10.0);
  tx.send(UiToWorker::PointerDown {
    tab_id,
    pos_css: click_pos,
    button: fastrender::ui::messages::PointerButton::Primary,
    modifiers: fastrender::ui::messages::PointerModifiers::NONE,
    click_count: 1,
  })
  .expect("PointerDown");
  tx.send(UiToWorker::PointerUp {
    tab_id,
    pos_css: click_pos,
    button: fastrender::ui::messages::PointerButton::Primary,
    modifiers: fastrender::ui::messages::PointerModifiers::NONE,
  })
  .expect("PointerUp");

  let msg = support::recv_until(&rx, TIMEOUT, |msg| {
    matches!(msg, WorkerToUi::ColorPickerOpened { tab_id: t, .. } if *t == tab_id)
  })
  .expect("expected ColorPickerOpened message");

  let WorkerToUi::ColorPickerOpened {
    input_node_id,
    value,
    ..
  } = msg
  else {
    unreachable!("filtered above");
  };
  assert_eq!(
    value, "#000000",
    "default <input type=color> value should be #000000"
  );

  tx.send(UiToWorker::ColorPickerChoose {
    tab_id,
    input_node_id,
    value: "#00ff00".to_string(),
  })
  .expect("ColorPickerChoose");

  support::recv_until(&rx, TIMEOUT, |msg| {
    matches!(msg, WorkerToUi::ColorPickerClosed { tab_id: t, .. } if *t == tab_id)
  })
  .expect("expected ColorPickerClosed after choose");

  // Click submit.
  let submit_pos = (10.0, 50.0);
  tx.send(UiToWorker::PointerDown {
    tab_id,
    pos_css: submit_pos,
    button: fastrender::ui::messages::PointerButton::Primary,
    modifiers: fastrender::ui::messages::PointerModifiers::NONE,
    click_count: 1,
  })
  .expect("PointerDown submit");
  tx.send(UiToWorker::PointerUp {
    tab_id,
    pos_css: submit_pos,
    button: fastrender::ui::messages::PointerButton::Primary,
    modifiers: fastrender::ui::messages::PointerModifiers::NONE,
  })
  .expect("PointerUp submit");

  let nav = support::recv_until(&rx, TIMEOUT, |msg| {
    matches!(msg, WorkerToUi::NavigationStarted { tab_id: t, .. } if *t == tab_id)
  })
  .expect("expected NavigationStarted after submitting the form");

  let WorkerToUi::NavigationStarted { url, .. } = nav else {
    unreachable!("filtered above");
  };
  assert!(
    url.contains("c=%2300ff00"),
    "expected form submission URL to contain URL-encoded color value (url={url})"
  );

  let parsed = url::Url::parse(&url).expect("parse navigation URL");
  let params: std::collections::HashMap<String, String> = parsed.query_pairs().into_owned().collect();
  assert_eq!(params.get("c"), Some(&"#00ff00".to_string()));

  drop(tx);
  drop(rx);
  join.join().unwrap();
}
