#![cfg(feature = "browser_ui")]

use super::support;
use fastrender::dom::{enumerate_dom_ids, DomNode};
use fastrender::interaction::KeyAction;
use fastrender::ui::messages::{PointerButton, RepaintReason, TabId};
use fastrender::ui::BrowserTabController;
use fastrender::Result;

fn node_id_by_id_attr(root: &DomNode, id_attr: &str) -> usize {
  let ids = enumerate_dom_ids(root);
  let mut stack: Vec<&DomNode> = vec![root];
  while let Some(node) = stack.pop() {
    if node.get_attribute_ref("id").is_some_and(|id| id == id_attr) {
      return *ids
        .get(&(node as *const DomNode))
        .unwrap_or_else(|| panic!("node id missing for element with id={id_attr:?}"));
    }
    for child in node.children.iter().rev() {
      stack.push(child);
    }
  }
  panic!("no element with id attribute {id_attr:?}");
}

#[test]
fn tabindex_negative_pointer_focuses_but_tab_skips() -> Result<()> {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
  let _lock = super::stage_listener_test_lock();
  let tab_id = TabId(1);
  let viewport_css = (200, 120);
  let url = "https://example.com/index.html";

  let html = r#"<!doctype html>
    <html>
      <head>
        <meta charset=\"utf-8\">
        <style>
          html, body { margin: 0; padding: 0; }
          #a { position: absolute; left: 0; top: 0; width: 80px; height: 22px; }
          #t { position: absolute; left: 0; top: 30px; width: 80px; height: 22px; background: rgb(220, 220, 0); }
          #b { position: absolute; left: 0; top: 60px; width: 80px; height: 22px; }
        </style>
      </head>
      <body>
        <input id=\"a\">
        <div id=\"t\" tabindex=\"-1\"></div>
        <input id=\"b\">
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
  let _ = controller.handle_message(support::request_repaint(tab_id, RepaintReason::Explicit))?;

  let node_id_a = node_id_by_id_attr(controller.document().dom(), "a");
  let node_id_t = node_id_by_id_attr(controller.document().dom(), "t");
  let node_id_b = node_id_by_id_attr(controller.document().dom(), "b");

  // Tab focuses the first tab stop, skipping `tabindex < 0`.
  let _ = controller.handle_message(support::key_action(tab_id, KeyAction::Tab))?;
  assert_eq!(controller.interaction_state().focused, Some(node_id_a));

  // Click the `tabindex="-1"` element and ensure it receives focus.
  let click = (10.0, 40.0);
  let _ = controller.handle_message(support::pointer_down(tab_id, click, PointerButton::Primary))?;
  let _ = controller.handle_message(support::pointer_up(tab_id, click, PointerButton::Primary))?;
  assert_eq!(controller.interaction_state().focused, Some(node_id_t));

  // Sequential focus navigation via Tab should skip `tabindex < 0` elements.
  let _ = controller.handle_message(support::key_action(tab_id, KeyAction::Tab))?;
  assert_eq!(controller.interaction_state().focused, Some(node_id_a));
  let _ = controller.handle_message(support::key_action(tab_id, KeyAction::Tab))?;
  assert_eq!(controller.interaction_state().focused, Some(node_id_b));

  Ok(())
}
