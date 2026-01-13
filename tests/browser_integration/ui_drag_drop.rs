#![cfg(feature = "browser_ui")]

use super::support;
use fastrender::interaction::KeyAction;
use fastrender::ui::messages::{PointerButton, PointerModifiers, RepaintReason, TabId};
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

#[test]
fn drag_drop_document_selection_into_text_input_inserts_text() -> Result<()> {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
  let _lock = super::stage_listener_test_lock();
  let tab_id = TabId(1);
  let viewport_css = (320, 180);
  let url = "https://example.com/index.html";

  let html = r#"<!doctype html>
    <html>
      <head>
        <meta charset="utf-8">
        <style>
          html, body { margin: 0; padding: 0; }
          body { font: 40px/80px monospace; }
          #src { position: absolute; top: 0; left: 10px; margin: 0; }
          input {
            position: absolute;
            left: 0;
            top: 90px;
            width: 260px;
            height: 40px;
            padding: 0;
            border: 0;
            outline: none;
            font-family: "Noto Sans Mono";
            font-size: 24px;
          }
        </style>
      </head>
      <body>
        <p id="src">hello</p>
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

  // Double-click selects the word in the document (document selection, not a text control).
  let src_click = (20.0, 40.0);
  let _ = controller.handle_message(support::pointer_down_with(
    tab_id,
    src_click,
    PointerButton::Primary,
    PointerModifiers::NONE,
    2,
  ))?;
  let _ = controller.handle_message(support::pointer_up(
    tab_id,
    src_click,
    PointerButton::Primary,
  ))?;

  // Drag the selected text into the input.
  let dst_drop = (10.0, 110.0);
  let _ = controller.handle_message(support::pointer_down(
    tab_id,
    src_click,
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

  let dst = find_element_by_id(controller.document().dom(), "dst");
  assert_eq!(dst.get_attribute_ref("value"), Some("hello"));
  Ok(())
}

#[test]
fn click_inside_document_selection_defers_collapse_until_mouseup() -> Result<()> {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
  let _lock = super::stage_listener_test_lock();
  let tab_id = TabId(1);
  let viewport_css = (320, 180);
  let url = "https://example.com/index.html";

  let html = r#"<!doctype html>
    <html>
      <head>
        <meta charset="utf-8">
        <style>
          html, body { margin: 0; padding: 0; }
          body { font: 40px/80px monospace; }
          #src { position: absolute; top: 0; left: 10px; margin: 0; }
        </style>
      </head>
      <body>
        <p id="src">hello</p>
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

  // Double-click selects the word, producing a highlighted document selection.
  let src_click = (20.0, 40.0);
  let _ = controller.handle_message(support::pointer_down_with(
    tab_id,
    src_click,
    PointerButton::Primary,
    PointerModifiers::NONE,
    2,
  ))?;
  let _ = controller.handle_message(support::pointer_up(
    tab_id,
    src_click,
    PointerButton::Primary,
  ))?;
  assert!(
    controller
      .interaction_state()
      .document_selection
      .as_ref()
      .is_some_and(|sel| sel.has_highlight()),
    "expected document selection highlight after double-click"
  );

  // Clicking inside the highlight should *not* collapse the selection on pointer down (so the user
  // can still begin a drag-and-drop), but should collapse on mouseup if no drag occurs.
  let _ = controller.handle_message(support::pointer_down(
    tab_id,
    src_click,
    PointerButton::Primary,
  ))?;
  assert!(
    controller
      .interaction_state()
      .document_selection
      .as_ref()
      .is_some_and(|sel| sel.has_highlight()),
    "selection should remain highlighted during pointer down"
  );

  let _ = controller.handle_message(support::pointer_up(
    tab_id,
    src_click,
    PointerButton::Primary,
  ))?;
  assert!(
    controller
      .interaction_state()
      .document_selection
      .as_ref()
      .is_some_and(|sel| !sel.has_highlight()),
    "selection should collapse on click release when no drag/drop occurs"
  );

  Ok(())
}
