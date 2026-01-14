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

fn drag_drop_selected_text_within_single_text_input(
  up_modifiers: PointerModifiers,
  expected_value: &str,
) -> Result<()> {
  let tab_id = TabId(1);
  let viewport_css = (320, 100);
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
            top: 0;
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
        <input id="src" value="hello">
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

  // Focus the input.
  let focus_point = (10.0, 20.0);
  let _ = controller.handle_message(support::pointer_down(
    tab_id,
    focus_point,
    PointerButton::Primary,
  ))?;
  let _ = controller.handle_message(support::pointer_up(
    tab_id,
    focus_point,
    PointerButton::Primary,
  ))?;

  // Select "ell" via keyboard: place caret after "h", then shift-select three characters.
  let _ = controller.handle_message(support::key_action(tab_id, KeyAction::Home))?;
  let _ = controller.handle_message(support::key_action(tab_id, KeyAction::ArrowRight))?;
  for _ in 0..3 {
    let _ = controller.handle_message(support::key_action(tab_id, KeyAction::ShiftArrowRight))?;
  }
  assert_eq!(
    controller
      .interaction_state()
      .text_edit
      .as_ref()
      .and_then(|state| state.selection),
    Some((1, 4)),
    "expected selection to cover \"ell\""
  );

  // Drag the selection to the end of the same input.
  let drag_start = (20.0, 20.0);
  let drop_end = (250.0, 20.0);
  let _ = controller.handle_message(support::pointer_down(
    tab_id,
    drag_start,
    PointerButton::Primary,
  ))?;
  let _ = controller.handle_message(support::pointer_move(
    tab_id,
    drop_end,
    PointerButton::Primary,
  ))?;
  let _ = controller.handle_message(support::pointer_up_with(
    tab_id,
    drop_end,
    PointerButton::Primary,
    up_modifiers,
  ))?;

  let src = find_element_by_id(controller.document().dom(), "src");
  assert_eq!(src.get_attribute_ref("value"), Some(expected_value));
  Ok(())
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
fn drag_drop_selected_text_within_text_input_moves_text_by_default() -> Result<()> {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
  let _lock = super::stage_listener_test_lock();
  drag_drop_selected_text_within_single_text_input(PointerModifiers::NONE, "hoell")
}

#[test]
fn drag_drop_selected_text_within_text_input_with_command_copies_text() -> Result<()> {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
  let _lock = super::stage_listener_test_lock();
  let cmd_mods = PointerModifiers::CTRL | PointerModifiers::META;
  drag_drop_selected_text_within_single_text_input(cmd_mods, "helloell")
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
fn drag_drop_document_selection_into_text_input_clamps_maxlength() -> Result<()> {
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
        <p id="src">abcd</p>
        <input id="dst" maxlength="3" value="">
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
  assert_eq!(dst.get_attribute_ref("value"), Some("abc"));
  Ok(())
}

#[test]
fn drag_drop_selected_text_within_text_input_moves_text() -> Result<()> {
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
            top: 0;
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
        <input id="src" value="hello world">
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

  // Focus the input.
  let focus_pos = (10.0, 20.0);
  let _ = controller.handle_message(support::pointer_down(
    tab_id,
    focus_pos,
    PointerButton::Primary,
  ))?;
  let _ = controller.handle_message(support::pointer_up(
    tab_id,
    focus_pos,
    PointerButton::Primary,
  ))?;

  // Double-click selects "hello".
  let dbl_click = (20.0, 20.0);
  let _ = controller.handle_message(support::pointer_down_with(
    tab_id,
    dbl_click,
    PointerButton::Primary,
    PointerModifiers::NONE,
    2,
  ))?;
  let _ = controller.handle_message(support::pointer_up(
    tab_id,
    dbl_click,
    PointerButton::Primary,
  ))?;

  // Drag the selected "hello" to the end of the input.
  let drag_start = (4.0, 20.0);
  let drop_end = (250.0, 20.0);
  let _ = controller.handle_message(support::pointer_down(
    tab_id,
    drag_start,
    PointerButton::Primary,
  ))?;
  let _ = controller.handle_message(support::pointer_move(
    tab_id,
    drop_end,
    PointerButton::Primary,
  ))?;
  let _ = controller.handle_message(support::pointer_up(
    tab_id,
    drop_end,
    PointerButton::Primary,
  ))?;

  let src = find_element_by_id(controller.document().dom(), "src");
  assert_eq!(src.get_attribute_ref("value"), Some(" worldhello"));
  Ok(())
}

#[test]
fn drag_drop_selected_text_within_textarea_moves_text() -> Result<()> {
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
          textarea {
            position: absolute;
            left: 0;
            top: 0;
            width: 260px;
            height: 80px;
            padding: 0;
            border: 0;
            outline: none;
            font-family: "Noto Sans Mono";
            font-size: 24px;
            resize: none;
          }
        </style>
      </head>
      <body>
        <textarea id="src">hello world</textarea>
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

  // Focus the textarea.
  let focus_pos = (10.0, 20.0);
  let _ = controller.handle_message(support::pointer_down(
    tab_id,
    focus_pos,
    PointerButton::Primary,
  ))?;
  let _ = controller.handle_message(support::pointer_up(
    tab_id,
    focus_pos,
    PointerButton::Primary,
  ))?;

  // Double-click selects "hello".
  let dbl_click = (20.0, 20.0);
  let _ = controller.handle_message(support::pointer_down_with(
    tab_id,
    dbl_click,
    PointerButton::Primary,
    PointerModifiers::NONE,
    2,
  ))?;
  let _ = controller.handle_message(support::pointer_up(
    tab_id,
    dbl_click,
    PointerButton::Primary,
  ))?;

  // Drag the selected "hello" to the end of the textarea line.
  let drag_start = (4.0, 20.0);
  let drop_end = (250.0, 20.0);
  let _ = controller.handle_message(support::pointer_down(
    tab_id,
    drag_start,
    PointerButton::Primary,
  ))?;
  let _ = controller.handle_message(support::pointer_move(
    tab_id,
    drop_end,
    PointerButton::Primary,
  ))?;
  let _ = controller.handle_message(support::pointer_up(
    tab_id,
    drop_end,
    PointerButton::Primary,
  ))?;

  let src = find_element_by_id(controller.document().dom(), "src");
  assert_eq!(src.get_attribute_ref("data-fastr-value"), Some(" worldhello"));
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

#[test]
fn click_inside_text_control_selection_defers_collapse_until_mouseup() -> Result<()> {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
  let _lock = super::stage_listener_test_lock();
  let tab_id = TabId(1);
  let viewport_css = (320, 100);
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
            top: 0;
            width: 260px;
            height: 40px;
            padding: 0;
            border: 0;
            outline: none;
            font-family: "Noto Sans Mono", monospace;
            font-size: 24px;
          }
        </style>
      </head>
      <body>
        <input id="src" value="hello">
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

  // Focus the input.
  let src_click = (10.0, 20.0);
  let _ = controller.handle_message(support::pointer_down(
    tab_id,
    src_click,
    PointerButton::Primary,
  ))?;
  let _ = controller.handle_message(support::pointer_up(
    tab_id,
    src_click,
    PointerButton::Primary,
  ))?;

  // Select all text in the input.
  let _ = controller.handle_message(support::key_action(tab_id, KeyAction::SelectAll))?;

  // Precondition: the focused text control should have an active selection highlight.
  let focused = controller.interaction_state().focused;
  assert!(
    controller
      .interaction_state()
      .text_edit
      .as_ref()
      .is_some_and(|state| state.selection.is_some() && Some(state.node_id) == focused),
    "expected focused text control selection after SelectAll"
  );

  // Clicking inside the selection should *not* collapse the selection on pointer down (so the user
  // can still begin a drag-and-drop), but should collapse on mouseup if no drag occurs.
  let _ = controller.handle_message(support::pointer_down(
    tab_id,
    src_click,
    PointerButton::Primary,
  ))?;
  assert!(
    controller
      .interaction_state()
      .text_edit
      .as_ref()
      .is_some_and(|state| state.selection.is_some() && Some(state.node_id) == focused),
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
      .text_edit
      .as_ref()
      .is_some_and(|state| state.selection.is_none() && Some(state.node_id) == focused),
    "selection should collapse on click release when no drag/drop occurs"
  );

  Ok(())
}
