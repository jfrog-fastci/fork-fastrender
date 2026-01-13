#![cfg(feature = "browser_ui")]

use fastrender::interaction::dom_index::DomIndex;
use fastrender::interaction::KeyAction;
use fastrender::ui::messages::{RepaintReason, TabId, UiToWorker, WorkerToUi};
use fastrender::ui::BrowserTabController;
use fastrender::Result;

fn dom_preorder_id(dom: &fastrender::dom::DomNode, element_id: &str) -> usize {
  let mut clone = dom.clone();
  let index = DomIndex::build(&mut clone);
  *index
    .id_by_element_id
    .get(element_id)
    .unwrap_or_else(|| panic!("expected element with id={element_id:?}"))
}

#[test]
fn browser_tab_controller_select_dropdown_keyboard_open_space_and_enter() -> Result<()> {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
  let _lock = super::stage_listener_test_lock();

  let tab_id = TabId(1);
  let viewport_css = (240, 120);
  let url = "https://example.com/index.html";
  let html = r#"<!doctype html>
    <html>
      <head>
        <meta charset="utf-8">
        <style>
          html, body { margin: 0; padding: 0; }
          #before { position: absolute; top: 10px; left: 10px; width: 120px; height: 22px; }
          #sel { position: absolute; top: 40px; left: 10px; width: 120px; height: 22px; }
        </style>
      </head>
      <body>
        <input id="before" value="x">
        <select id="sel">
          <option>One</option>
          <option>Two</option>
        </select>
      </body>
    </html>
  "#;

  let mut controller = BrowserTabController::from_html(tab_id, html, url, viewport_css, 1.0)?;

  // Ensure the document is laid out before driving keyboard focus traversal.
  let _ = controller.handle_message(UiToWorker::RequestRepaint {
    tab_id,
    reason: RepaintReason::Explicit,
  })?;

  let select_node_id = dom_preorder_id(controller.document().dom(), "sel");

  // Tab until the select is focused.
  for _ in 0..16 {
    if controller.interaction_state().focused == Some(select_node_id) {
      break;
    }
    let _ = controller.handle_message(UiToWorker::KeyAction {
      tab_id,
      key: KeyAction::Tab,
    })?;
  }
  assert_eq!(
    controller.interaction_state().focused,
    Some(select_node_id),
    "expected Tab traversal to focus the <select>"
  );

  // Press Space to open the dropdown popup.
  let open_space = controller.handle_message(UiToWorker::KeyAction {
    tab_id,
    key: KeyAction::Space,
  })?;
  assert!(
    open_space.iter().any(|msg| matches!(
      msg,
      WorkerToUi::SelectDropdownOpened { tab_id: got_tab, select_node_id: got_select, .. }
        if *got_tab == tab_id && *got_select == select_node_id
    )),
    "expected Space to open select dropdown, got {open_space:?}"
  );

  // Enter should also open the dropdown (mirrors pointer activation semantics).
  let open_enter = controller.handle_message(UiToWorker::KeyAction {
    tab_id,
    key: KeyAction::Enter,
  })?;
  assert!(
    open_enter.iter().any(|msg| matches!(
      msg,
      WorkerToUi::SelectDropdownOpened { tab_id: got_tab, select_node_id: got_select, .. }
        if *got_tab == tab_id && *got_select == select_node_id
    )),
    "expected Enter to open select dropdown, got {open_enter:?}"
  );

  Ok(())
}

