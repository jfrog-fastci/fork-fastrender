#![cfg(feature = "browser_ui")]

use super::support;
use base64::Engine as _;
use fastrender::ui::messages::{KeyAction, PointerButton, PointerModifiers, TabId, UiToWorker, WorkerToUi};
use std::time::Duration;

// Worker startup + first render can take a while in debug builds (font init, cache warming, etc).
const TIMEOUT: Duration = Duration::from_secs(60);

#[test]
fn browser_thread_datalist_scroll_offscreen_closes_popup() {
  let _lock = super::stage_listener_test_lock();

  let html = r#"<!doctype html>
    <html>
      <head>
        <meta charset="utf-8">
        <style>
          html, body { margin: 0; padding: 0; }
          #in { position: absolute; left: 0; top: 0; width: 200px; height: 30px; }
          #spacer { height: 2000px; }
        </style>
      </head>
      <body>
        <input id="in" list="dl">
        <datalist id="dl">
          <option value="One"></option>
          <option value="Two"></option>
        </datalist>
        <div id="spacer"></div>
      </body>
    </html>
  "#;
  let encoded = base64::engine::general_purpose::STANDARD.encode(html.as_bytes());
  let url = format!("data:text/html;base64,{encoded}");

  let worker = fastrender::ui::spawn_browser_worker_with_factory(support::deterministic_factory())
    .expect("spawn browser worker");
  let fastrender::ui::BrowserWorkerHandle { tx, rx, join } = worker;

  let tab_id = TabId::new();
  tx.send(UiToWorker::CreateTab {
    tab_id,
    initial_url: Some(url),
    cancel: Default::default(),
  })
  .expect("CreateTab");
  tx.send(UiToWorker::SetActiveTab { tab_id })
    .expect("SetActiveTab");
  tx.send(UiToWorker::ViewportChanged {
    tab_id,
    viewport_css: (240, 120),
    dpr: 1.0,
  })
  .expect("ViewportChanged");

  match support::recv_until(&rx, TIMEOUT, |msg: &WorkerToUi| {
    msg.tab_id() == tab_id && matches!(msg, WorkerToUi::FrameReady { .. })
  }) {
    Some(WorkerToUi::FrameReady { .. }) => {}
    Some(other) => panic!("expected FrameReady, got {other:?}"),
    None => panic!("timed out waiting for FrameReady"),
  }

  // Drain initial messages.
  while rx.try_recv().is_ok() {}

  // Focus the input via click.
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

  // Datalist suggestions are emitted on text input (and/or ArrowDown), not on focus alone.
  tx.send(UiToWorker::TextInput {
    tab_id,
    text: "o".to_string(),
  })
  .expect("TextInput");
  tx.send(UiToWorker::KeyAction {
    tab_id,
    key: KeyAction::ArrowDown,
  })
  .expect("KeyAction(ArrowDown)");

  let opened = support::recv_until(&rx, TIMEOUT, |msg: &WorkerToUi| {
    msg.tab_id() == tab_id && matches!(msg, WorkerToUi::DatalistOpened { .. })
  })
  .expect("expected DatalistOpened message");
  let WorkerToUi::DatalistOpened { tab_id: opened_tab, .. } = opened else {
    unreachable!("filtered above");
  };
  assert_eq!(opened_tab, tab_id);

  // Scroll far enough that the input is fully offscreen. The worker should close the popup.
  tx.send(UiToWorker::ScrollTo {
    tab_id,
    pos_css: (0.0, 500.0),
  })
  .expect("ScrollTo");

  support::recv_until(&rx, TIMEOUT, |msg: &WorkerToUi| {
    msg.tab_id() == tab_id && matches!(msg, WorkerToUi::DatalistClosed { .. })
  })
  .expect("expected DatalistClosed after scroll");

  drop(tx);
  drop(rx);
  join.join().unwrap();
}
