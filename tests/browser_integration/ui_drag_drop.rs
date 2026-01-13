#![cfg(feature = "browser_ui")]

use super::support;
use fastrender::interaction::KeyAction;
use fastrender::ui::messages::{PointerButton, RepaintReason, TabId};
use fastrender::ui::BrowserTabController;
use fastrender::{dom::DomNode, Result};

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
fn drag_drop_selected_text_between_text_inputs_copies_text() -> Result<()> {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
  let _lock = super::stage_listener_test_lock();
  let tab_id = TabId(1);
  let viewport_css = (320, 140);
  let url = "https://example.com/index.html";

  let html = r#"<!doctype html>
    <html>
      <head>
        <meta charset="utf-8">
        <style>
          html, body { margin: 0; padding: 0; }
          input {
            position: absolute;
            left: 0;
            width: 260px;
            height: 40px;
            padding: 0;
            border: 0;
            outline: none;
            font-family: "Noto Sans Mono";
            font-size: 24px;
          }
          #src { top: 0; }
          #dst { top: 60px; }
        </style>
      </head>
      <body>
        <input id="src" value="hello">
        <input id="dst" value="">
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

  // Focus the source input.
  let src_focus = (10.0, 20.0);
  let _ = controller.handle_message(support::pointer_down(
    tab_id,
    src_focus,
    PointerButton::Primary,
  ))?;
  let _ = controller.handle_message(support::pointer_up(
    tab_id,
    src_focus,
    PointerButton::Primary,
  ))?;

  // Select all text in the source input.
  let _ = controller.handle_message(support::key_action(tab_id, KeyAction::SelectAll))?;

  // Drag the selected text into the destination input.
  // Start the drag near the left edge of the selection. This exercises the boundary case where the
  // click quantizes to the selection start caret (caret == sel_start), but should still be treated
  // as a drag-drop gesture candidate.
  let src_drag_start = (4.0, 20.0);
  let dst_drop = (10.0, 80.0);
  let _ = controller.handle_message(support::pointer_down(
    tab_id,
    src_drag_start,
    PointerButton::Primary,
  ))?;
  let _ = controller.handle_message(support::pointer_move(
    tab_id,
    dst_drop,
    PointerButton::Primary,
  ))?;
  let _ = controller.handle_message(support::pointer_up(
    tab_id,
    dst_drop,
    PointerButton::Primary,
  ))?;

  let src = find_element_by_id(controller.document().dom(), "src");
  let dst = find_element_by_id(controller.document().dom(), "dst");
  assert_eq!(src.get_attribute_ref("value"), Some("hello"));
  assert_eq!(dst.get_attribute_ref("value"), Some("hello"));
  Ok(())
}
