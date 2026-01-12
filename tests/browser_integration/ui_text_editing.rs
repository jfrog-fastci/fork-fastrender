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

fn insert_at_char_idx(original: &str, idx: usize, insert: &str) -> String {
  let mut out = String::with_capacity(original.len().saturating_add(insert.len()));
  out.extend(original.chars().take(idx));
  out.push_str(insert);
  out.extend(original.chars().skip(idx));
  out
}

#[test]
fn click_to_place_caret_then_text_input_inserts_at_caret() -> Result<()> {
  let _lock = super::stage_listener_test_lock();
  let tab_id = TabId(1);
  let viewport_css = (500, 200);
  let url = "https://example.com/index.html";
  let initial_value = "01234567890123456789";

  let html = format!(
    r#"<!doctype html>
      <html>
        <head>
          <meta charset="utf-8">
          <style>
            html, body {{ margin: 0; padding: 0; }}
            #txt {{
              position: absolute;
              left: 0;
              top: 0;
              width: 420px;
              height: 40px;
              font-family: "Noto Sans Mono";
              font-size: 20px;
            }}
          </style>
        </head>
        <body>
          <input id="txt" value="{initial_value}">
        </body>
      </html>
    "#
  );

  let mut controller = BrowserTabController::from_html_with_renderer(
    support::deterministic_renderer(),
    tab_id,
    &html,
    url,
    viewport_css,
    1.0,
  )?;
  let _ = controller.handle_message(support::request_repaint(tab_id, RepaintReason::Explicit))?;

  // Click inside the text, not at the edges, so caret placement is expected to land in the middle.
  let click = (160.0, 20.0);
  let _ =
    controller.handle_message(support::pointer_down(tab_id, click, PointerButton::Primary))?;
  let _ = controller.handle_message(support::pointer_up(tab_id, click, PointerButton::Primary))?;

  let input = find_element_by_id(controller.document().dom(), "txt");
  assert_eq!(input.get_attribute_ref("value"), Some(initial_value));

  let _ = controller.handle_message(support::text_input(tab_id, "X"))?;
  let input = find_element_by_id(controller.document().dom(), "txt");
  let after_x = input
    .get_attribute_ref("value")
    .expect("expected input value after typing X");
  let caret = after_x
    .chars()
    .position(|ch| ch == 'X')
    .expect("expected typed character to appear in input value");
  let len = initial_value.chars().count();
  assert!(
    caret > 0 && caret < len,
    "expected click-to-place caret to land inside the text (got {caret} for len={len})"
  );

  assert_eq!(after_x, insert_at_char_idx(initial_value, caret, "X"));

  // Type another character to confirm the caret advanced after insertion.
  let _ = controller.handle_message(support::text_input(tab_id, "Y"))?;
  let input = find_element_by_id(controller.document().dom(), "txt");
  let expected = insert_at_char_idx(initial_value, caret, "XY");
  assert_eq!(input.get_attribute_ref("value"), Some(expected.as_str()));

  Ok(())
}

#[test]
fn arrow_left_moves_caret_and_typing_inserts_before_last_char() -> Result<()> {
  let _lock = super::stage_listener_test_lock();
  let tab_id = TabId(1);
  let viewport_css = (400, 120);
  let url = "https://example.com/index.html";

  let html = r#"<!doctype html>
    <html>
      <head>
        <meta charset="utf-8">
        <style>
          html, body { margin: 0; padding: 0; }
          #txt { position: absolute; left: 0; top: 0; width: 280px; height: 40px; font-family: "Noto Sans Mono"; font-size: 20px; }
        </style>
      </head>
      <body>
        <input id="txt" value="abc">
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

  // Focus the input and force the caret to the end.
  let click = (270.0, 20.0);
  let _ =
    controller.handle_message(support::pointer_down(tab_id, click, PointerButton::Primary))?;
  let _ = controller.handle_message(support::pointer_up(tab_id, click, PointerButton::Primary))?;
  let _ = controller.handle_message(support::key_action(tab_id, KeyAction::End))?;

  let _ = controller.handle_message(support::key_action(tab_id, KeyAction::ArrowLeft))?;
  let _ = controller.handle_message(support::text_input(tab_id, "X"))?;

  let input = find_element_by_id(controller.document().dom(), "txt");
  assert_eq!(input.get_attribute_ref("value"), Some("abXc"));
  Ok(())
}

#[test]
fn shift_arrow_creates_selection_and_typing_replaces_it() -> Result<()> {
  let _lock = super::stage_listener_test_lock();
  let tab_id = TabId(1);
  let viewport_css = (400, 120);
  let url = "https://example.com/index.html";

  let html = r#"<!doctype html>
    <html>
      <head>
        <meta charset="utf-8">
        <style>
          html, body { margin: 0; padding: 0; }
          #txt { position: absolute; left: 0; top: 0; width: 280px; height: 40px; font-family: "Noto Sans Mono"; font-size: 20px; }
        </style>
      </head>
      <body>
        <input id="txt" value="abcd">
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

  let click = (270.0, 20.0);
  let _ =
    controller.handle_message(support::pointer_down(tab_id, click, PointerButton::Primary))?;
  let _ = controller.handle_message(support::pointer_up(tab_id, click, PointerButton::Primary))?;
  let _ = controller.handle_message(support::key_action(tab_id, KeyAction::End))?;

  // Select the last two characters ("cd").
  let _ = controller.handle_message(support::key_action(tab_id, KeyAction::ShiftArrowLeft))?;
  let _ = controller.handle_message(support::key_action(tab_id, KeyAction::ShiftArrowLeft))?;
  let _ = controller.handle_message(support::text_input(tab_id, "X"))?;

  let input = find_element_by_id(controller.document().dom(), "txt");
  assert_eq!(input.get_attribute_ref("value"), Some("abX"));
  Ok(())
}

#[test]
fn delete_removes_following_character() -> Result<()> {
  let _lock = super::stage_listener_test_lock();
  let tab_id = TabId(1);
  let viewport_css = (400, 120);
  let url = "https://example.com/index.html";

  let html = r#"<!doctype html>
    <html>
      <head>
        <meta charset="utf-8">
        <style>
          html, body { margin: 0; padding: 0; }
          #txt { position: absolute; left: 0; top: 0; width: 280px; height: 40px; font-family: "Noto Sans Mono"; font-size: 20px; }
        </style>
      </head>
      <body>
        <input id="txt" value="abc">
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

  let click = (10.0, 20.0);
  let _ =
    controller.handle_message(support::pointer_down(tab_id, click, PointerButton::Primary))?;
  let _ = controller.handle_message(support::pointer_up(tab_id, click, PointerButton::Primary))?;

  // Move caret to after "a", then delete "b".
  let _ = controller.handle_message(support::key_action(tab_id, KeyAction::Home))?;
  let _ = controller.handle_message(support::key_action(tab_id, KeyAction::ArrowRight))?;
  let _ = controller.handle_message(support::key_action(tab_id, KeyAction::Delete))?;

  let input = find_element_by_id(controller.document().dom(), "txt");
  assert_eq!(input.get_attribute_ref("value"), Some("ac"));
  Ok(())
}

#[test]
fn textarea_enter_inserts_newline_and_arrow_up_down_moves_between_lines() -> Result<()> {
  let _lock = super::stage_listener_test_lock();
  let tab_id = TabId(1);
  let viewport_css = (500, 220);
  let url = "https://example.com/index.html";

  let html = r#"<!doctype html>
    <html>
      <head>
        <meta charset="utf-8">
        <style>
          html, body { margin: 0; padding: 0; }
          #ta { position: absolute; left: 0; top: 60px; width: 420px; height: 120px; font-family: "Noto Sans Mono"; font-size: 20px; }
        </style>
      </head>
      <body>
        <textarea id="ta">abc
def</textarea>
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

  let click = (10.0, 80.0);
  let _ =
    controller.handle_message(support::pointer_down(tab_id, click, PointerButton::Primary))?;
  let _ = controller.handle_message(support::pointer_up(tab_id, click, PointerButton::Primary))?;
  let _ = controller.handle_message(support::key_action(tab_id, KeyAction::End))?;

  let _ = controller.handle_message(support::key_action(tab_id, KeyAction::Enter))?;
  let textarea = find_element_by_id(controller.document().dom(), "ta");
  assert_eq!(
    textarea.get_attribute_ref("data-fastr-value"),
    Some("abc\ndef\n"),
    "expected Enter to insert a newline in textarea value"
  );

  let _ = controller.handle_message(support::key_action(tab_id, KeyAction::ArrowUp))?;
  let _ = controller.handle_message(support::text_input(tab_id, "X"))?;
  let textarea = find_element_by_id(controller.document().dom(), "ta");
  assert_eq!(
    textarea.get_attribute_ref("data-fastr-value"),
    Some("abc\nXdef\n"),
    "expected ArrowUp to move caret to previous line before insertion"
  );

  let _ = controller.handle_message(support::key_action(tab_id, KeyAction::ArrowDown))?;
  let _ = controller.handle_message(support::text_input(tab_id, "Y"))?;
  let textarea = find_element_by_id(controller.document().dom(), "ta");
  assert_eq!(
    textarea.get_attribute_ref("data-fastr-value"),
    Some("abc\nXdef\nY"),
    "expected ArrowDown to move caret back to last line before insertion"
  );

  Ok(())
}
