#![cfg(feature = "browser_ui")]

use super::support;
use fastrender::tree::box_tree::SelectItem;
use fastrender::ui::messages::{PointerButton, TabId, UiToWorker, WorkerToUi};
use std::time::Duration;

// Worker startup + first render can take a few seconds under parallel load (CI).
const TIMEOUT: Duration = Duration::from_secs(20);

#[test]
fn browser_thread_click_dropdown_select_emits_select_dropdown_opened_message() {
  let _lock = super::stage_listener_test_lock();

  let site = support::TempSite::new();
  let html = r#"<!doctype html>
    <html>
      <head>
        <style>
          html, body { margin: 0; padding: 0; }
          #sel { position: absolute; left: 0; top: 0; width: 120px; height: 30px; }
        </style>
      </head>
      <body>
        <select id="sel">
          <option>One</option>
          <option selected>Two</option>
          <option>Three</option>
        </select>
      </body>
    </html>
  "#;
  let url = site.write("page.html", html);

  let worker = fastrender::ui::spawn_browser_worker().expect("spawn browser worker");
  let fastrender::ui::BrowserWorkerHandle { tx, rx, join } = worker;

  let tab_id = TabId::new();
  tx
    .send(support::create_tab_msg(tab_id, Some(url)))
    .expect("CreateTab");
  tx.send(UiToWorker::SetActiveTab { tab_id })
    .expect("SetActiveTab");
  tx
    .send(support::viewport_changed_msg(tab_id, (200, 80), 1.0))
    .expect("ViewportChanged");

  let _frame = match support::recv_for_tab(&rx, tab_id, TIMEOUT, |msg| {
    matches!(msg, WorkerToUi::FrameReady { .. })
  }) {
    Some(WorkerToUi::FrameReady { frame, .. }) => frame,
    Some(other) => panic!("expected FrameReady, got {other:?}"),
    None => panic!("timed out waiting for FrameReady"),
  };

  // Clear any queued messages from the initial navigation/render.
  while rx.try_recv().is_ok() {}

  // Click within the select control.
  tx.send(UiToWorker::PointerDown {
    tab_id,
    pos_css: (10.0, 10.0),
    button: PointerButton::Primary,
  })
  .expect("PointerDown");
  tx.send(UiToWorker::PointerUp {
    tab_id,
    pos_css: (10.0, 10.0),
    button: PointerButton::Primary,
  })
  .expect("PointerUp");

  let msg = support::recv_for_tab(&rx, tab_id, TIMEOUT, |msg| {
    matches!(msg, WorkerToUi::OpenSelectDropdown { .. })
  })
  .expect("expected OpenSelectDropdown message");

  let WorkerToUi::OpenSelectDropdown {
    tab_id: msg_tab,
    select_node_id,
    control,
  } = msg
  else {
    unreachable!("filtered above");
  };
  assert_eq!(msg_tab, tab_id);
  assert!(select_node_id > 0, "expected non-zero select_node_id");
  assert!(!control.multiple, "expected dropdown select to be single-select");
  assert_eq!(control.size, 1);
  assert_eq!(control.items.len(), 3);
  assert_eq!(control.selected, vec![1]);

  let labels: Vec<String> = control
    .items
    .iter()
    .filter_map(|item| match item {
      SelectItem::Option { label, .. } => Some(label.clone()),
      _ => None,
    })
    .collect();
  assert_eq!(labels, vec!["One", "Two", "Three"]);

  let msg = support::recv_for_tab(&rx, tab_id, TIMEOUT, |msg| {
    matches!(msg, WorkerToUi::SelectDropdownOpened { .. })
  })
  .expect("expected SelectDropdownOpened message");

  let WorkerToUi::SelectDropdownOpened {
    tab_id: msg_tab,
    select_node_id: anchored_select_node_id,
    control: anchored_control,
    anchor_css: anchor_rect_css,
  } = msg
  else {
    unreachable!("filtered above");
  };

  assert_eq!(msg_tab, tab_id);
  assert_eq!(
    anchored_select_node_id, select_node_id,
    "expected SelectDropdownOpened select_node_id to match OpenSelectDropdown"
  );
  assert_eq!(
    anchored_control.selected, control.selected,
    "expected SelectDropdownOpened control to match OpenSelectDropdown"
  );
  assert!(
    anchor_rect_css.origin.x.is_finite()
      && anchor_rect_css.origin.y.is_finite()
      && anchor_rect_css.size.width.is_finite()
      && anchor_rect_css.size.height.is_finite(),
    "expected finite anchor_css rect, got {anchor_rect_css:?}"
  );
  assert!(
    anchor_rect_css.x().abs() < 1.0 && anchor_rect_css.y().abs() < 1.0,
    "expected anchor_css to be near the top-left of the viewport, got {anchor_rect_css:?}"
  );
  assert!(
    anchor_rect_css.width() > 80.0 && anchor_rect_css.width() < 200.0,
    "expected anchor_css width to reflect the styled <select> width, got {anchor_rect_css:?}"
  );
  assert!(
    anchor_rect_css.height() > 10.0 && anchor_rect_css.height() < 80.0,
    "expected anchor_css height to reflect the styled <select> height, got {anchor_rect_css:?}"
  );

  // Clean shutdown: dropping the sender allows the worker thread to exit its recv loop.
  drop(tx);
  drop(rx);
  join.join().unwrap();
}
