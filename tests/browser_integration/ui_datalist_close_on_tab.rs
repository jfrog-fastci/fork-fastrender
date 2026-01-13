#![cfg(feature = "browser_ui")]

use super::support;
use fastrender::interaction::dom_index::DomIndex;
use fastrender::interaction::KeyAction;
use fastrender::ui::messages::{PointerButton, RepaintReason, TabId, WorkerToUi};
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
fn browser_tab_controller_datalist_closes_on_tab_focus_change() -> Result<()> {
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
          #a { position: absolute; left: 10px; top: 10px; width: 180px; height: 22px; }
          #b { position: absolute; left: 10px; top: 50px; width: 180px; height: 22px; }
        </style>
      </head>
      <body>
        <input id="a" list="dl">
        <datalist id="dl">
          <option value="a"></option>
          <option value="aa"></option>
          <option value="b"></option>
        </datalist>
        <input id="b">
      </body>
    </html>
  "#;

  let mut controller = BrowserTabController::from_html_with_renderer(
    support::deterministic_renderer(),
    tab_id,
    html,
    url,
    viewport_css,
    1.0,
  )?;

  // Ensure layout is prepared before driving focus traversal.
  let _ = controller.handle_message(support::request_repaint(tab_id, RepaintReason::Explicit))?;

  let input_a_node_id = dom_preorder_id(controller.document().dom(), "a");
  let input_b_node_id = dom_preorder_id(controller.document().dom(), "b");

  let click_a = (20.0, 20.0);
  let _ = controller.handle_message(support::pointer_down(
    tab_id,
    click_a,
    PointerButton::Primary,
  ))?;
  let _ =
    controller.handle_message(support::pointer_up(tab_id, click_a, PointerButton::Primary))?;
  assert_eq!(
    controller.interaction_state().focused,
    Some(input_a_node_id),
    "expected pointer click to focus <input id=a>"
  );

  let opened = controller.handle_message(support::text_input(tab_id, "a"))?;
  assert!(
    opened
      .iter()
      .any(|msg| matches!(msg, WorkerToUi::DatalistOpened { .. })),
    "expected typing to open datalist popup, got:\n{}",
    support::format_messages(&opened)
  );

  let closed = controller.handle_message(support::key_action(tab_id, KeyAction::Tab))?;
  assert!(
    closed
      .iter()
      .any(|msg| matches!(msg, WorkerToUi::DatalistClosed { .. })),
    "expected Tab focus traversal to close datalist popup, got:\n{}",
    support::format_messages(&closed)
  );
  assert_eq!(
    controller.interaction_state().focused,
    Some(input_b_node_id),
    "expected Tab traversal to move focus to <input id=b>"
  );

  Ok(())
}
