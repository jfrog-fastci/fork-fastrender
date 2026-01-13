#![cfg(feature = "browser_ui")]

use super::support;
use fastrender::dom::DomNode;
use fastrender::ui::messages::{PointerButton, RepaintReason, TabId};
use fastrender::ui::BrowserTabController;
use fastrender::Result;

fn find_element_by_id<'a>(dom: &'a DomNode, element_id: &str) -> &'a DomNode {
  let mut stack = vec![dom];
  while let Some(node) = stack.pop() {
    if node.get_attribute_ref("id") == Some(element_id) {
      return node;
    }
    for child in node.children.iter().rev() {
      stack.push(child);
    }
  }
  panic!("expected element with id={element_id:?}");
}

#[test]
fn range_input_rtl_pointer_drag_respects_visual_direction() -> Result<()> {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
  let _lock = super::stage_listener_test_lock();

  let tab_id = TabId(1);
  let viewport_css = (220, 60);
  let url = "https://example.com/index.html";

  let html = r#"<!doctype html>
    <html>
      <head>
        <meta charset="utf-8">
        <style>
          html, body { margin: 0; padding: 0; }
          #r { position: absolute; left: 0; top: 0; width: 200px; height: 30px; padding: 0; margin: 0; border: 0; direction: rtl; }
        </style>
      </head>
      <body>
        <input id="r" type="range" dir="rtl" min="0" max="2" step="1" value="0">
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

  // In RTL, the painted range thumb is mirrored: min on the right, max on the left. Pointer
  // mapping should match.
  let click_left = (5.0, 15.0);
  let _ = controller.handle_message(support::pointer_down(tab_id, click_left, PointerButton::Primary))?;
  let _ = controller.handle_message(support::pointer_up(tab_id, click_left, PointerButton::Primary))?;
  assert_eq!(
    find_element_by_id(controller.document().dom(), "r").get_attribute_ref("value"),
    Some("2")
  );

  let click_right = (195.0, 15.0);
  let _ =
    controller.handle_message(support::pointer_down(tab_id, click_right, PointerButton::Primary))?;
  let _ = controller.handle_message(support::pointer_up(tab_id, click_right, PointerButton::Primary))?;
  assert_eq!(
    find_element_by_id(controller.document().dom(), "r").get_attribute_ref("value"),
    Some("0")
  );

  Ok(())
}

