#![cfg(feature = "browser_ui")]

use super::support;
use fastrender::tree::box_tree::SelectItem;
use fastrender::ui::cancel::CancelGens;
use fastrender::ui::messages::{PointerButton, PointerModifiers, TabId, UiToWorker, WorkerToUi};
use std::time::{Duration, Instant};

// Worker startup + first render can take a few seconds under parallel load (CI).
const TIMEOUT: Duration = Duration::from_secs(20);

#[test]
fn browser_thread_select_dropdown_choose_updates_styles_and_repaints() {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
  let _lock = super::stage_listener_test_lock();

  let site = support::TempSite::new();
  let html = r#"<!doctype html>
    <html>
      <head>
        <style>
          html, body { margin: 0; padding: 0; }
          #sel { position: absolute; left: 0; top: 0; width: 120px; height: 30px; }
          #marker { position: absolute; left: 0; top: 40px; width: 64px; height: 64px; background: rgb(255, 0, 0); }
          /* React to option[selected] mutation via :has so we can assert via pixels. */
          select:has(option#opt_two[selected]) + #marker { background: rgb(0, 255, 0); }
        </style>
      </head>
      <body>
        <select id="sel">
          <option id="opt_one" selected>One</option>
          <option id="opt_two">Two</option>
          <option id="opt_three">Three</option>
        </select>
        <div id="marker"></div>
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
  tx.send(support::viewport_changed_msg(tab_id, (200, 140), 1.0))
    .expect("ViewportChanged");

  let initial_frame = match support::recv_for_tab(&rx, tab_id, TIMEOUT, |msg| {
    matches!(msg, WorkerToUi::FrameReady { .. })
  }) {
    Some(WorkerToUi::FrameReady { frame, .. }) => frame,
    Some(other) => panic!("expected FrameReady, got {other:?}"),
    None => panic!("timed out waiting for FrameReady"),
  };
  assert_eq!(
    support::rgba_at(&initial_frame.pixmap, 10, 50),
    [255, 0, 0, 255],
    "expected marker to start red"
  );

  // Clear any queued messages from the initial navigation/render.
  while rx.try_recv().is_ok() {}

  // Click within the select control.
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
    matches!(msg, WorkerToUi::SelectDropdownOpened { .. })
  })
  .expect("expected SelectDropdownOpened message");

  let WorkerToUi::SelectDropdownOpened {
    tab_id: msg_tab,
    select_node_id,
    control,
    ..
  } = msg
  else {
    unreachable!("filtered above");
  };
  assert_eq!(msg_tab, tab_id);

  let option_node_id = control
    .items
    .iter()
    .find_map(|item| match item {
      SelectItem::Option { label, node_id, .. } if label == "Two" => Some(*node_id),
      _ => None,
    })
    .expect("expected dropdown to contain option 'Two'");

  tx.send(UiToWorker::SelectDropdownChoose {
    tab_id,
    select_node_id,
    option_node_id,
  })
  .expect("SelectDropdownChoose");

  let deadline = Instant::now() + TIMEOUT;
  loop {
    let remaining = deadline.saturating_duration_since(Instant::now());
    assert!(
      !remaining.is_zero(),
      "timed out waiting for marker to turn green after SelectDropdownChoose"
    );

    let msg = support::recv_for_tab(
      &rx,
      tab_id,
      remaining.min(Duration::from_millis(200)),
      |msg| matches!(msg, WorkerToUi::FrameReady { .. }),
    );
    let Some(WorkerToUi::FrameReady { frame, .. }) = msg else {
      continue;
    };

    if support::rgba_at(&frame.pixmap, 10, 50) == [0, 255, 0, 255] {
      break;
    }
  }

  // Clean shutdown: dropping the sender allows the worker thread to exit its recv loop.
  drop(tx);
  drop(rx);
  join.join().unwrap();
}
