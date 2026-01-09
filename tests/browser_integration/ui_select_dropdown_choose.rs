#![cfg(feature = "browser_ui")]

use fastrender::ui::messages::{PointerButton, RepaintReason, TabId, UiToWorker, WorkerToUi};
use fastrender::ui::BrowserTabController;
use fastrender::Result;

fn find_element_by_id<'a>(
  dom: &'a fastrender::dom::DomNode,
  element_id: &str,
) -> Option<&'a fastrender::dom::DomNode> {
  let mut stack = vec![dom];
  while let Some(node) = stack.pop() {
    if node.get_attribute_ref("id") == Some(element_id) {
      return Some(node);
    }
    for child in node.children.iter().rev() {
      stack.push(child);
    }
  }
  None
}

#[test]
fn browser_tab_controller_select_dropdown_choose_updates_dom_and_repaints() -> Result<()> {
  let _lock = super::stage_listener_test_lock();

  let tab_id = TabId(1);
  let viewport_css = (200, 80);
  let url = "https://example.com/index.html";
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
          <option id="o1" selected>One</option>
          <option id="o2">Two</option>
        </select>
      </body>
    </html>
  "#;

  let mut controller = BrowserTabController::from_html(tab_id, html, url, viewport_css, 1.0)?;

  // Ensure the document is laid out before hit-testing the click.
  let initial_msgs = controller.handle_message(UiToWorker::RequestRepaint {
    tab_id,
    reason: RepaintReason::Explicit,
  })?;
  assert!(
    initial_msgs
      .iter()
      .any(|msg| matches!(msg, WorkerToUi::FrameReady { .. })),
    "expected initial FrameReady"
  );

  // Click within the select control to open the dropdown.
  let _ = controller.handle_message(UiToWorker::PointerDown {
    tab_id,
    pos_css: (10.0, 10.0),
    button: PointerButton::Primary,
  })?;
  let open_msgs = controller.handle_message(UiToWorker::PointerUp {
    tab_id,
    pos_css: (10.0, 10.0),
    button: PointerButton::Primary,
  })?;

  let (select_node_id, control) = open_msgs
    .iter()
    .find_map(|msg| match msg {
      WorkerToUi::OpenSelectDropdown {
        tab_id: msg_tab,
        select_node_id,
        control,
      } if *msg_tab == tab_id => Some((*select_node_id, control.clone())),
      _ => None,
    })
    .expect("expected WorkerToUi::OpenSelectDropdown after clicking select");

  let option_node_id = control
    .items
    .iter()
    .find_map(|item| match item {
      fastrender::tree::box_tree::SelectItem::Option { node_id, label, .. } if label == "Two" => {
        Some(*node_id)
      }
      _ => None,
    })
    .expect("expected to find option with label \"Two\"");

  let choose_msgs = controller.handle_message(UiToWorker::SelectDropdownChoose {
    tab_id,
    select_node_id,
    option_node_id,
  })?;

  assert!(
    choose_msgs
      .iter()
      .any(|msg| matches!(msg, WorkerToUi::FrameReady { .. })),
    "expected FrameReady after selecting option"
  );

  let dom = controller.document().dom();
  let o1 = find_element_by_id(dom, "o1").expect("expected <option id=o1>");
  let o2 = find_element_by_id(dom, "o2").expect("expected <option id=o2>");
  assert!(
    o2.get_attribute_ref("selected").is_some(),
    "expected o2 to be selected after SelectDropdownChoose"
  );
  assert!(
    o1.get_attribute_ref("selected").is_none(),
    "expected o1 to have selected cleared after SelectDropdownChoose"
  );

  Ok(())
}
