#![cfg(feature = "browser_ui")]

use super::support;
use fastrender::interaction::KeyAction;
use fastrender::ui::messages::{PointerButton, RepaintReason, TabId, UiToWorker};
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

fn node_id_by_id_attr(dom: &DomNode, element_id: &str) -> usize {
  let node = find_element_by_id(dom, element_id);
  let ids = fastrender::dom::enumerate_dom_ids(dom);
  ids
    .get(&(node as *const DomNode))
    .copied()
    .unwrap_or_else(|| panic!("expected preorder id for element with id={element_id:?}"))
}

fn insert_at_char_idx(original: &str, idx: usize, insert: &str) -> String {
  let mut out = String::with_capacity(original.len().saturating_add(insert.len()));
  out.extend(original.chars().take(idx));
  out.push_str(insert);
  out.extend(original.chars().skip(idx));
  out
}

fn replace_range_at_char_idx(original: &str, start: usize, end: usize, replacement: &str) -> String {
  let mut out = String::with_capacity(original.len().saturating_add(replacement.len()));
  out.extend(original.chars().take(start));
  out.push_str(replacement);
  out.extend(original.chars().skip(end));
  out
}

#[test]
fn click_to_place_caret_then_text_input_inserts_at_caret() -> Result<()> {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
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
fn line_backspace_deletes_to_start_of_textarea_line() -> Result<()> {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
  let _lock = super::stage_listener_test_lock();
  let tab_id = TabId(1);
  let viewport_css = (500, 200);
  let url = "https://example.com/index.html";

  let html = r#"<!doctype html>
    <html>
      <head>
        <meta charset="utf-8">
        <style>
          html, body { margin: 0; padding: 0; }
          #ta { position: absolute; left: 0; top: 0; width: 420px; height: 120px; font-family: "Noto Sans Mono"; font-size: 20px; }
        </style>
      </head>
      <body>
        <textarea id="ta">abc
def
ghi</textarea>
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

  // Focus textarea.
  let click = (10.0, 10.0);
  let _ = controller.handle_message(support::pointer_down(tab_id, click, PointerButton::Primary))?;
  let _ = controller.handle_message(support::pointer_up(tab_id, click, PointerButton::Primary))?;

  // Move caret to second line, after "de".
  let _ = controller.handle_message(support::key_action(tab_id, KeyAction::Home))?;
  let _ = controller.handle_message(support::key_action(tab_id, KeyAction::ArrowDown))?;
  let _ = controller.handle_message(support::key_action(tab_id, KeyAction::ArrowRight))?;
  let _ = controller.handle_message(support::key_action(tab_id, KeyAction::ArrowRight))?;

  let _ = controller.handle_message(support::key_action(tab_id, KeyAction::LineBackspace))?;

  let textarea = find_element_by_id(controller.document().dom(), "ta");
  assert_eq!(
    textarea.get_attribute_ref("data-fastr-value"),
    Some("abc\nf\nghi"),
    "expected LineBackspace to delete from caret to start of the current textarea line"
  );

  Ok(())
}

#[test]
fn line_delete_deletes_to_end_of_textarea_line() -> Result<()> {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
  let _lock = super::stage_listener_test_lock();
  let tab_id = TabId(1);
  let viewport_css = (500, 200);
  let url = "https://example.com/index.html";

  let html = r#"<!doctype html>
    <html>
      <head>
        <meta charset="utf-8">
        <style>
          html, body { margin: 0; padding: 0; }
          #ta { position: absolute; left: 0; top: 0; width: 420px; height: 120px; font-family: "Noto Sans Mono"; font-size: 20px; }
        </style>
      </head>
      <body>
        <textarea id="ta">abc
def
ghi</textarea>
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

  // Focus textarea.
  let click = (10.0, 10.0);
  let _ = controller.handle_message(support::pointer_down(tab_id, click, PointerButton::Primary))?;
  let _ = controller.handle_message(support::pointer_up(tab_id, click, PointerButton::Primary))?;

  // Move caret to second line, after "d".
  let _ = controller.handle_message(support::key_action(tab_id, KeyAction::Home))?;
  let _ = controller.handle_message(support::key_action(tab_id, KeyAction::ArrowDown))?;
  let _ = controller.handle_message(support::key_action(tab_id, KeyAction::ArrowRight))?;

  let _ = controller.handle_message(support::key_action(tab_id, KeyAction::LineDelete))?;

  let textarea = find_element_by_id(controller.document().dom(), "ta");
  assert_eq!(
    textarea.get_attribute_ref("data-fastr-value"),
    Some("abc\nd\nghi"),
    "expected LineDelete to delete from caret to end of the current textarea line"
  );

  Ok(())
}

#[test]
fn appearance_none_text_input_auto_scrolls_and_click_to_place_accounts_for_scroll() -> Result<()> {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
  let _lock = super::stage_listener_test_lock();
  let tab_id = TabId(1);
  let viewport_css = (240, 120);
  let url = "https://example.com/index.html";

  let long_text = "abcdefghijklmnopqrstuvwxyz0123456789abcdefghijklmnopqrstuvwxyz0123456789";

  let html = r#"<!doctype html>
    <html>
      <head>
        <meta charset="utf-8">
        <style>
          html, body { margin: 0; padding: 0; }
          #txt {
            position: absolute;
            left: 0;
            top: 0;
            width: 120px;
            height: 40px;
            appearance: none;
            padding: 0;
            border: 0;
            font-family: "Noto Sans Mono";
            font-size: 20px;
          }
        </style>
      </head>
      <body>
        <input id="txt" value="">
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
  let click = (5.0, 10.0);
  let _ = controller.handle_message(support::pointer_down(tab_id, click, PointerButton::Primary))?;
  let _ = controller.handle_message(support::pointer_up(tab_id, click, PointerButton::Primary))?;

  // Type enough text to overflow the input width. Paint should auto-scroll horizontally so the
  // caret remains visible.
  let _ = controller.handle_message(support::text_input(tab_id, long_text))?;
  let _ = controller.handle_message(support::request_repaint(tab_id, RepaintReason::Explicit))?;
  assert!(
    controller
      .scroll_state()
      .elements
      .values()
      .any(|offset| offset.x > 0.0),
    "expected appearance:none input to auto-scroll horizontally; got {:?}",
    controller.scroll_state().elements
  );

  // Click near the left edge of the visible text and insert a character. The click-to-place caret
  // mapping should account for the scroll offset, so insertion happens in the latter portion of the
  // long value (not at the start).
  let _ = controller.handle_message(support::pointer_down(tab_id, click, PointerButton::Primary))?;
  let _ = controller.handle_message(support::pointer_up(tab_id, click, PointerButton::Primary))?;
  let _ = controller.handle_message(support::text_input(tab_id, "X"))?;

  let input = find_element_by_id(controller.document().dom(), "txt");
  let value = input
    .get_attribute_ref("value")
    .expect("expected input value after insertion");
  let x_pos = value
    .chars()
    .position(|ch| ch == 'X')
    .expect("expected inserted character to appear in input value");
  let base_len = long_text.chars().count();
  assert!(
    x_pos > base_len / 2,
    "expected click-to-place to account for horizontal scroll (inserted X at {x_pos}, len={base_len})"
  );

  Ok(())
}

#[test]
fn arrow_left_moves_caret_and_typing_inserts_before_last_char() -> Result<()> {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
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
fn arrow_left_moves_across_zwj_emoji_grapheme_cluster() -> Result<()> {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
  let _lock = super::stage_listener_test_lock();
  let tab_id = TabId(1);
  let viewport_css = (400, 120);
  let url = "https://example.com/index.html";
  let emoji = "👨‍👩‍👧‍👦";

  let html = format!(
    r#"<!doctype html>
    <html>
      <head>
        <meta charset="utf-8">
        <style>
          html, body {{ margin: 0; padding: 0; }}
          #txt {{ position: absolute; left: 0; top: 0; width: 280px; height: 40px; font-family: "Noto Sans Mono"; font-size: 20px; }}
        </style>
      </head>
      <body>
        <input id="txt" value="{emoji}">
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

  let click = (10.0, 20.0);
  let _ =
    controller.handle_message(support::pointer_down(tab_id, click, PointerButton::Primary))?;
  let _ = controller.handle_message(support::pointer_up(tab_id, click, PointerButton::Primary))?;
  let _ = controller.handle_message(support::key_action(tab_id, KeyAction::End))?;

  let _ = controller.handle_message(support::key_action(tab_id, KeyAction::ArrowLeft))?;
  let _ = controller.handle_message(support::text_input(tab_id, "X"))?;

  let input = find_element_by_id(controller.document().dom(), "txt");
  let expected = format!("X{emoji}");
  assert_eq!(input.get_attribute_ref("value"), Some(expected.as_str()));
  Ok(())
}

#[test]
fn arrow_left_moves_across_combining_mark_grapheme_cluster() -> Result<()> {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
  let _lock = super::stage_listener_test_lock();
  let tab_id = TabId(1);
  let viewport_css = (400, 120);
  let url = "https://example.com/index.html";
  let composed = "a\u{0301}";

  let html = format!(
    r#"<!doctype html>
    <html>
      <head>
        <meta charset="utf-8">
        <style>
          html, body {{ margin: 0; padding: 0; }}
          #txt {{ position: absolute; left: 0; top: 0; width: 280px; height: 40px; font-family: "Noto Sans Mono"; font-size: 20px; }}
        </style>
      </head>
      <body>
        <input id="txt" value="{composed}">
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

  let click = (10.0, 20.0);
  let _ =
    controller.handle_message(support::pointer_down(tab_id, click, PointerButton::Primary))?;
  let _ = controller.handle_message(support::pointer_up(tab_id, click, PointerButton::Primary))?;
  let _ = controller.handle_message(support::key_action(tab_id, KeyAction::End))?;

  let _ = controller.handle_message(support::key_action(tab_id, KeyAction::ArrowLeft))?;
  let _ = controller.handle_message(support::text_input(tab_id, "X"))?;

  let input = find_element_by_id(controller.document().dom(), "txt");
  let expected = format!("X{composed}");
  assert_eq!(
    input.get_attribute_ref("value"),
    Some(expected.as_str()),
    "expected ArrowLeft to treat combining sequences as a single grapheme cluster"
  );
  Ok(())
}

#[test]
fn arrow_right_moves_across_combining_mark_grapheme_cluster() -> Result<()> {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
  let _lock = super::stage_listener_test_lock();
  let tab_id = TabId(1);
  let viewport_css = (400, 120);
  let url = "https://example.com/index.html";
  let composed = "a\u{0301}";

  let html = format!(
    r#"<!doctype html>
    <html>
      <head>
        <meta charset="utf-8">
        <style>
          html, body {{ margin: 0; padding: 0; }}
          #txt {{ position: absolute; left: 0; top: 0; width: 280px; height: 40px; font-family: "Noto Sans Mono"; font-size: 20px; }}
        </style>
      </head>
      <body>
        <input id="txt" value="{composed}">
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

  let click = (10.0, 20.0);
  let _ =
    controller.handle_message(support::pointer_down(tab_id, click, PointerButton::Primary))?;
  let _ = controller.handle_message(support::pointer_up(tab_id, click, PointerButton::Primary))?;
  let _ = controller.handle_message(support::key_action(tab_id, KeyAction::Home))?;

  let _ = controller.handle_message(support::key_action(tab_id, KeyAction::ArrowRight))?;
  let _ = controller.handle_message(support::text_input(tab_id, "X"))?;

  let input = find_element_by_id(controller.document().dom(), "txt");
  let expected = format!("{composed}X");
  assert_eq!(
    input.get_attribute_ref("value"),
    Some(expected.as_str()),
    "expected ArrowRight to treat combining sequences as a single grapheme cluster"
  );
  Ok(())
}

#[test]
fn shift_arrow_left_extends_selection_across_combining_mark_grapheme_cluster() -> Result<()> {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
  let _lock = super::stage_listener_test_lock();
  let tab_id = TabId(1);
  let viewport_css = (400, 120);
  let url = "https://example.com/index.html";
  let composed = "a\u{0301}";

  let html = format!(
    r#"<!doctype html>
    <html>
      <head>
        <meta charset="utf-8">
        <style>
          html, body {{ margin: 0; padding: 0; }}
          #txt {{ position: absolute; left: 0; top: 0; width: 280px; height: 40px; font-family: "Noto Sans Mono"; font-size: 20px; }}
        </style>
      </head>
      <body>
        <input id="txt" value="{composed}b">
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

  let click = (10.0, 20.0);
  let _ =
    controller.handle_message(support::pointer_down(tab_id, click, PointerButton::Primary))?;
  let _ = controller.handle_message(support::pointer_up(tab_id, click, PointerButton::Primary))?;
  let _ = controller.handle_message(support::key_action(tab_id, KeyAction::End))?;

  // Select trailing "b", then extend selection to include the entire "a\u0301" grapheme cluster.
  let _ = controller.handle_message(support::key_action(tab_id, KeyAction::ShiftArrowLeft))?;
  let _ = controller.handle_message(support::key_action(tab_id, KeyAction::ShiftArrowLeft))?;
  let _ = controller.handle_message(support::text_input(tab_id, "X"))?;

  let input = find_element_by_id(controller.document().dom(), "txt");
  assert_eq!(
    input.get_attribute_ref("value"),
    Some("X"),
    "expected ShiftArrowLeft selection extension to respect grapheme cluster boundaries"
  );
  Ok(())
}

#[test]
fn shift_arrow_right_extends_selection_across_combining_mark_grapheme_cluster() -> Result<()> {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
  let _lock = super::stage_listener_test_lock();
  let tab_id = TabId(1);
  let viewport_css = (400, 120);
  let url = "https://example.com/index.html";
  let composed = "a\u{0301}";

  let html = format!(
    r#"<!doctype html>
    <html>
      <head>
        <meta charset="utf-8">
        <style>
          html, body {{ margin: 0; padding: 0; }}
          #txt {{ position: absolute; left: 0; top: 0; width: 280px; height: 40px; font-family: "Noto Sans Mono"; font-size: 20px; }}
        </style>
      </head>
      <body>
        <input id="txt" value="b{composed}">
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

  let click = (10.0, 20.0);
  let _ =
    controller.handle_message(support::pointer_down(tab_id, click, PointerButton::Primary))?;
  let _ = controller.handle_message(support::pointer_up(tab_id, click, PointerButton::Primary))?;
  let _ = controller.handle_message(support::key_action(tab_id, KeyAction::Home))?;

  // Select leading "b", then extend selection to include the entire "a\u0301" grapheme cluster.
  let _ = controller.handle_message(support::key_action(tab_id, KeyAction::ShiftArrowRight))?;
  let _ = controller.handle_message(support::key_action(tab_id, KeyAction::ShiftArrowRight))?;
  let _ = controller.handle_message(support::text_input(tab_id, "X"))?;

  let input = find_element_by_id(controller.document().dom(), "txt");
  assert_eq!(
    input.get_attribute_ref("value"),
    Some("X"),
    "expected ShiftArrowRight selection extension to respect grapheme cluster boundaries"
  );
  Ok(())
}

#[test]
fn arrow_right_moves_across_zwj_emoji_grapheme_cluster() -> Result<()> {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
  let _lock = super::stage_listener_test_lock();
  let tab_id = TabId(1);
  let viewport_css = (400, 120);
  let url = "https://example.com/index.html";
  let emoji = "👨‍👩‍👧‍👦";

  let html = format!(
    r#"<!doctype html>
    <html>
      <head>
        <meta charset="utf-8">
        <style>
          html, body {{ margin: 0; padding: 0; }}
          #txt {{ position: absolute; left: 0; top: 0; width: 280px; height: 40px; font-family: "Noto Sans Mono"; font-size: 20px; }}
        </style>
      </head>
      <body>
        <input id="txt" value="{emoji}">
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

  let click = (10.0, 20.0);
  let _ =
    controller.handle_message(support::pointer_down(tab_id, click, PointerButton::Primary))?;
  let _ = controller.handle_message(support::pointer_up(tab_id, click, PointerButton::Primary))?;
  let _ = controller.handle_message(support::key_action(tab_id, KeyAction::Home))?;

  let _ = controller.handle_message(support::key_action(tab_id, KeyAction::ArrowRight))?;
  let _ = controller.handle_message(support::text_input(tab_id, "X"))?;

  let input = find_element_by_id(controller.document().dom(), "txt");
  let expected = format!("{emoji}X");
  assert_eq!(input.get_attribute_ref("value"), Some(expected.as_str()));
  Ok(())
}

#[test]
fn click_to_place_caret_on_zwj_emoji_snaps_to_grapheme_boundary() -> Result<()> {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
  let _lock = super::stage_listener_test_lock();
  let tab_id = TabId(1);
  let viewport_css = (400, 120);
  let url = "https://example.com/index.html";
  let emoji = "👨‍👩‍👧‍👦";

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
            width: 280px;
            height: 40px;
            font-family: "Noto Sans Mono";
            font-size: 20px;
            text-align: left;
          }}
        </style>
      </head>
      <body>
        <input id="txt" value="{emoji}">
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

  // Click within the rendered emoji (not at the edges). Even though the emoji is multiple Unicode
  // scalar values, the caret must snap to a grapheme boundary (start/end), never inside the ZWJ
  // sequence.
  let click = (40.0, 20.0);
  let _ =
    controller.handle_message(support::pointer_down(tab_id, click, PointerButton::Primary))?;
  let _ = controller.handle_message(support::pointer_up(tab_id, click, PointerButton::Primary))?;

  let _ = controller.handle_message(support::text_input(tab_id, "X"))?;

  let input = find_element_by_id(controller.document().dom(), "txt");
  let value = input
    .get_attribute_ref("value")
    .expect("expected input value after typing X");
  let left = format!("X{emoji}");
  let right = format!("{emoji}X");
  assert!(
    value == left || value == right,
    "expected click placement to snap to a grapheme boundary; got {value:?}"
  );
  Ok(())
}

#[test]
fn shift_arrow_left_extends_selection_across_zwj_emoji_grapheme_cluster() -> Result<()> {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
  let _lock = super::stage_listener_test_lock();
  let tab_id = TabId(1);
  let viewport_css = (400, 120);
  let url = "https://example.com/index.html";
  let emoji = "👨‍👩‍👧‍👦";

  let html = format!(
    r#"<!doctype html>
    <html>
      <head>
        <meta charset="utf-8">
        <style>
          html, body {{ margin: 0; padding: 0; }}
          #txt {{ position: absolute; left: 0; top: 0; width: 280px; height: 40px; font-family: "Noto Sans Mono"; font-size: 20px; }}
        </style>
      </head>
      <body>
        <input id="txt" value="{emoji}a">
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

  let click = (10.0, 20.0);
  let _ =
    controller.handle_message(support::pointer_down(tab_id, click, PointerButton::Primary))?;
  let _ = controller.handle_message(support::pointer_up(tab_id, click, PointerButton::Primary))?;
  let _ = controller.handle_message(support::key_action(tab_id, KeyAction::End))?;

  // Select "a", then extend selection to include the entire emoji cluster.
  let _ = controller.handle_message(support::key_action(tab_id, KeyAction::ShiftArrowLeft))?;
  let _ = controller.handle_message(support::key_action(tab_id, KeyAction::ShiftArrowLeft))?;
  let _ = controller.handle_message(support::text_input(tab_id, "X"))?;

  let input = find_element_by_id(controller.document().dom(), "txt");
  assert_eq!(input.get_attribute_ref("value"), Some("X"));
  Ok(())
}

#[test]
fn shift_arrow_right_extends_selection_across_zwj_emoji_grapheme_cluster() -> Result<()> {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
  let _lock = super::stage_listener_test_lock();
  let tab_id = TabId(1);
  let viewport_css = (400, 120);
  let url = "https://example.com/index.html";
  let emoji = "👨‍👩‍👧‍👦";

  let html = format!(
    r#"<!doctype html>
    <html>
      <head>
        <meta charset="utf-8">
        <style>
          html, body {{ margin: 0; padding: 0; }}
          #txt {{ position: absolute; left: 0; top: 0; width: 280px; height: 40px; font-family: "Noto Sans Mono"; font-size: 20px; }}
        </style>
      </head>
      <body>
        <input id="txt" value="a{emoji}">
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

  let click = (10.0, 20.0);
  let _ =
    controller.handle_message(support::pointer_down(tab_id, click, PointerButton::Primary))?;
  let _ = controller.handle_message(support::pointer_up(tab_id, click, PointerButton::Primary))?;
  let _ = controller.handle_message(support::key_action(tab_id, KeyAction::Home))?;

  // Select "a", then extend selection to include the entire emoji cluster.
  let _ = controller.handle_message(support::key_action(tab_id, KeyAction::ShiftArrowRight))?;
  let _ = controller.handle_message(support::key_action(tab_id, KeyAction::ShiftArrowRight))?;
  let _ = controller.handle_message(support::text_input(tab_id, "X"))?;

  let input = find_element_by_id(controller.document().dom(), "txt");
  assert_eq!(input.get_attribute_ref("value"), Some("X"));
  Ok(())
}

#[test]
fn shift_arrow_creates_selection_and_typing_replaces_it() -> Result<()> {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
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
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
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
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
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

  // Click on the second line so `End` moves to the end of the textarea value (not just the first
  // line).
  let click = (10.0, 100.0);
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

#[test]
fn textarea_arrow_down_snaps_to_grapheme_boundary_on_zwj_emoji_line() -> Result<()> {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
  let _lock = super::stage_listener_test_lock();
  let tab_id = TabId(1);
  let viewport_css = (500, 220);
  let url = "https://example.com/index.html";
  let emoji = "👨‍👩‍👧‍👦";

  let html = format!(
    r#"<!doctype html>
      <html>
        <head>
          <meta charset="utf-8">
          <style>
            html, body {{ margin: 0; padding: 0; }}
            #ta {{ position: absolute; left: 0; top: 0; width: 420px; height: 120px; font-family: "Noto Sans Mono"; font-size: 20px; }}
          </style>
        </head>
        <body>
          <textarea id="ta">abc
{emoji}</textarea>
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

  let click = (10.0, 20.0);
  let _ =
    controller.handle_message(support::pointer_down(tab_id, click, PointerButton::Primary))?;
  let _ = controller.handle_message(support::pointer_up(tab_id, click, PointerButton::Primary))?;

  // Place caret at column 1 in the first line (between 'a' and 'b'), then move down onto the emoji
  // line. The caret must snap to a grapheme boundary (start/end of the emoji), never inside the ZWJ
  // sequence.
  let _ = controller.handle_message(support::key_action(tab_id, KeyAction::Home))?;
  let _ = controller.handle_message(support::key_action(tab_id, KeyAction::ArrowRight))?;
  let _ = controller.handle_message(support::key_action(tab_id, KeyAction::ArrowDown))?;
  let _ = controller.handle_message(support::text_input(tab_id, "X"))?;

  let textarea = find_element_by_id(controller.document().dom(), "ta");
  let value = textarea
    .get_attribute_ref("data-fastr-value")
    .expect("expected textarea value after typing X");
  let left = format!("abc\nX{emoji}");
  let right = format!("abc\n{emoji}X");
  assert!(
    value == left || value == right,
    "expected ArrowDown caret placement to snap to a grapheme boundary; got {value:?}"
  );

  Ok(())
}

#[test]
fn textarea_home_moves_to_start_of_current_line_not_document() -> Result<()> {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
  let _lock = super::stage_listener_test_lock();
  let tab_id = TabId(1);
  let viewport_css = (400, 200);
  let url = "https://example.com/index.html";

  let html = "<!doctype html>\
    <html>\
      <head>\
        <meta charset=\"utf-8\">\
        <style>\
          html, body { margin: 0; padding: 0; }\
          #ta { position: absolute; left: 0; top: 0; width: 280px; height: 80px; font-family: \"Noto Sans Mono\"; font-size: 20px; }\
        </style>\
      </head>\
      <body>\
        <textarea id=\"ta\">abc\ndef</textarea>\
      </body>\
    </html>";

  let mut controller = BrowserTabController::from_html_with_renderer(
    support::deterministic_renderer(),
    tab_id,
    html,
    url,
    viewport_css,
    1.0,
  )?;
  let _ = controller.handle_message(support::request_repaint(tab_id, RepaintReason::Explicit))?;

  // Focus on the second (newline-delimited) line.
  let click = (10.0, 35.0);
  let _ = controller.handle_message(support::pointer_down(tab_id, click, PointerButton::Primary))?;
  let _ = controller.handle_message(support::pointer_up(tab_id, click, PointerButton::Primary))?;

  // Force caret to the end of the textarea, then Home should move to the start of the current line
  // ("def"), not the start of the whole textarea.
  let _ = controller.handle_message(support::key_action(tab_id, KeyAction::End))?;
  let _ = controller.handle_message(support::key_action(tab_id, KeyAction::Home))?;
  let _ = controller.handle_message(support::text_input(tab_id, "X"))?;

  let textarea = find_element_by_id(controller.document().dom(), "ta");
  assert_eq!(textarea.get_attribute_ref("data-fastr-value"), Some("abc\nXdef"));
  Ok(())
}

#[test]
fn textarea_shift_arrow_up_down_extends_selection_and_typing_replaces_it() -> Result<()> {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
  let _lock = super::stage_listener_test_lock();
  let tab_id = TabId(1);
  let viewport_css = (500, 220);
  let url = "https://example.com/index.html";

  let html = r#"<!doctype html>
    <html>
      <head>
        <meta charset="utf-8">
        <style>
          html, body { margin: 0; padding: 0; height: 2000px; }
          #ta { position: absolute; left: 0; top: 0; width: 420px; height: 120px; font-family: "Noto Sans Mono"; font-size: 20px; }
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

  // Focus the textarea and move the caret to the start.
  let click = (10.0, 10.0);
  let _ = controller.handle_message(support::pointer_down(tab_id, click, PointerButton::Primary))?;
  let _ = controller.handle_message(support::pointer_up(tab_id, click, PointerButton::Primary))?;
  let _ = controller.handle_message(support::key_action(tab_id, KeyAction::Home))?;

  let scroll_before = controller.scroll_state().viewport;

  // Shift+ArrowDown: extend selection to the next line.
  let _ = controller.handle_message(support::key_action(tab_id, KeyAction::ShiftArrowDown))?;
  let ta_id = node_id_by_id_attr(controller.document().dom(), "ta");
  let edit = controller
    .interaction_state()
    .text_edit_for(ta_id)
    .expect("expected text edit state for textarea");
  assert_eq!(edit.caret, 4);
  assert_eq!(edit.selection, Some((0, 4)));
  assert_eq!(
    controller.scroll_state().viewport,
    scroll_before,
    "shift+arrow selection should not trigger viewport scrolling when a textarea is focused"
  );

  // Shift+ArrowUp should shrink the selection back to a caret.
  let _ = controller.handle_message(support::key_action(tab_id, KeyAction::ShiftArrowUp))?;
  let edit = controller
    .interaction_state()
    .text_edit_for(ta_id)
    .expect("expected text edit state for textarea");
  assert_eq!(edit.caret, 0);
  assert_eq!(edit.selection, None);

  // Re-create the selection and confirm typing replaces it.
  let _ = controller.handle_message(support::key_action(tab_id, KeyAction::ShiftArrowDown))?;
  let edit = controller
    .interaction_state()
    .text_edit_for(ta_id)
    .expect("expected text edit state for textarea");
  assert_eq!(edit.selection, Some((0, 4)));

  let _ = controller.handle_message(support::text_input(tab_id, "X"))?;
  let textarea = find_element_by_id(controller.document().dom(), "ta");
  assert_eq!(
    textarea.get_attribute_ref("data-fastr-value"),
    Some("Xdef"),
    "expected typing to replace the shift-selected range"
  );
  let edit = controller
    .interaction_state()
    .text_edit_for(ta_id)
    .expect("expected text edit state for textarea");
  assert_eq!(edit.selection, None);

  Ok(())
}

#[test]
fn textarea_end_moves_to_end_of_current_visual_line_when_wrapped() -> Result<()> {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
  let _lock = super::stage_listener_test_lock();
  let tab_id = TabId(1);
  let viewport_css = (240, 160);
  let url = "https://example.com/index.html";

  // Keep this identical to the textarea visual-line layout heuristics used by interaction code:
  // - monospace font
  // - width chosen so that (content_width - 4px inset) / (font_size * 0.6) floors to 10 chars/line.
  let initial = "012345678901234567890123456789";
  let html = format!(
    r#"<!doctype html>
      <html>
        <head>
          <meta charset="utf-8">
          <style>
            html, body {{ margin: 0; padding: 0; }}
            #ta {{
              position: absolute;
              left: 0;
              top: 0;
              width: 124px;
              height: 80px;
              padding: 0;
              border: 0;
              font-family: "Noto Sans Mono";
              font-size: 20px;
            }}
          </style>
        </head>
        <body>
          <textarea id="ta">{initial}</textarea>
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

  // Focus the textarea.
  let click = (10.0, 10.0);
  let _ = controller.handle_message(support::pointer_down(tab_id, click, PointerButton::Primary))?;
  let _ = controller.handle_message(support::pointer_up(tab_id, click, PointerButton::Primary))?;

  // Move to the second wrapped (visual) line and place the caret somewhere in the middle.
  let _ = controller.handle_message(support::key_action(tab_id, KeyAction::Home))?;
  let _ = controller.handle_message(support::key_action(tab_id, KeyAction::ArrowDown))?;
  for _ in 0..3 {
    let _ = controller.handle_message(support::key_action(tab_id, KeyAction::ArrowRight))?;
  }

  // End should move to the end of the *current visual line*, not to the end of the entire value.
  let _ = controller.handle_message(support::key_action(tab_id, KeyAction::End))?;
  let _ = controller.handle_message(support::text_input(tab_id, "X"))?;

  let textarea = find_element_by_id(controller.document().dom(), "ta");
  assert_eq!(
    textarea.get_attribute_ref("data-fastr-value"),
    Some("01234567890123456789X0123456789")
  );
  Ok(())
}

#[test]
fn word_left_moves_by_word_and_typing_inserts_at_word_boundary() -> Result<()> {
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
        <input id="txt" value="hello world">
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
  let _ = controller.handle_message(support::key_action(tab_id, KeyAction::End))?;

  let _ = controller.handle_message(support::key_action(tab_id, KeyAction::WordLeft))?;
  let _ = controller.handle_message(support::text_input(tab_id, "X"))?;

  let input = find_element_by_id(controller.document().dom(), "txt");
  assert_eq!(input.get_attribute_ref("value"), Some("hello Xworld"));
  Ok(())
}

#[test]
fn shift_word_left_selects_word_and_typing_replaces_it() -> Result<()> {
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
        <input id="txt" value="hello world">
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

  // Focus the input and place caret at end.
  let click = (10.0, 20.0);
  let _ = controller.handle_message(support::pointer_down(tab_id, click, PointerButton::Primary))?;
  let _ = controller.handle_message(support::pointer_up(tab_id, click, PointerButton::Primary))?;
  let _ = controller.handle_message(support::key_action(tab_id, KeyAction::End))?;

  // Ctrl/Cmd/Alt+Shift+ArrowLeft: select the previous word ("world").
  let _ = controller.handle_message(support::key_action(tab_id, KeyAction::ShiftWordLeft))?;
  let _ = controller.handle_message(support::text_input(tab_id, "X"))?;

  let input = find_element_by_id(controller.document().dom(), "txt");
  assert_eq!(input.get_attribute_ref("value"), Some("hello X"));
  Ok(())
}

#[test]
fn word_shift_arrow_extends_selection_by_word_in_input() -> Result<()> {
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
        <input id="txt" value="hello world">
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
  let click = (10.0, 20.0);
  let _ = controller.handle_message(support::pointer_down(tab_id, click, PointerButton::Primary))?;
  let _ = controller.handle_message(support::pointer_up(tab_id, click, PointerButton::Primary))?;

  // Move caret to the start of "world".
  let _ = controller.handle_message(support::key_action(tab_id, KeyAction::Home))?;
  let _ = controller.handle_message(support::key_action(tab_id, KeyAction::WordRight))?;

  // Ctrl/Cmd/Alt+Shift+ArrowRight: select the next word ("world").
  let _ = controller.handle_message(support::key_action(tab_id, KeyAction::ShiftWordRight))?;
  let input_id = node_id_by_id_attr(controller.document().dom(), "txt");
  let edit = controller
    .interaction_state()
    .text_edit_for(input_id)
    .expect("expected text edit state for input");
  assert_eq!(edit.selection, Some((6, 11)));

  // Collapse back to the word start, then select the previous word.
  let _ = controller.handle_message(support::key_action(tab_id, KeyAction::ArrowLeft))?;
  let _ = controller.handle_message(support::key_action(tab_id, KeyAction::ShiftWordLeft))?;
  let edit = controller
    .interaction_state()
    .text_edit_for(input_id)
    .expect("expected text edit state for input");
  assert_eq!(edit.selection, Some((0, 6)));

  Ok(())
}

#[test]
fn word_shift_arrow_extends_selection_by_word_in_textarea() -> Result<()> {
  let _lock = super::stage_listener_test_lock();
  let tab_id = TabId(1);
  let viewport_css = (400, 160);
  let url = "https://example.com/index.html";

  let html = r#"<!doctype html>
    <html>
      <head>
        <meta charset="utf-8">
        <style>
          html, body { margin: 0; padding: 0; }
          #ta { position: absolute; left: 0; top: 0; width: 280px; height: 80px; font-family: "Noto Sans Mono"; font-size: 20px; }
        </style>
      </head>
      <body>
        <textarea id="ta">hello world</textarea>
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
  let click = (10.0, 20.0);
  let _ = controller.handle_message(support::pointer_down(tab_id, click, PointerButton::Primary))?;
  let _ = controller.handle_message(support::pointer_up(tab_id, click, PointerButton::Primary))?;

  // Move caret to the start of "world".
  let _ = controller.handle_message(support::key_action(tab_id, KeyAction::Home))?;
  let _ = controller.handle_message(support::key_action(tab_id, KeyAction::WordRight))?;

  // Select the next word ("world").
  let _ = controller.handle_message(support::key_action(tab_id, KeyAction::ShiftWordRight))?;
  let ta_id = node_id_by_id_attr(controller.document().dom(), "ta");
  let edit = controller
    .interaction_state()
    .text_edit_for(ta_id)
    .expect("expected text edit state for textarea");
  assert_eq!(edit.selection, Some((6, 11)));

  // Collapse back to the word start, then select the previous word.
  let _ = controller.handle_message(support::key_action(tab_id, KeyAction::ArrowLeft))?;
  let _ = controller.handle_message(support::key_action(tab_id, KeyAction::ShiftWordLeft))?;
  let edit = controller
    .interaction_state()
    .text_edit_for(ta_id)
    .expect("expected text edit state for textarea");
  assert_eq!(edit.selection, Some((0, 6)));

  Ok(())
}

#[test]
fn word_backspace_deletes_word_left_of_caret() -> Result<()> {
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
        <input id="txt" value="hello world">
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
  let _ = controller.handle_message(support::key_action(tab_id, KeyAction::End))?;
  let _ = controller.handle_message(support::key_action(tab_id, KeyAction::WordBackspace))?;

  let input = find_element_by_id(controller.document().dom(), "txt");
  assert_eq!(input.get_attribute_ref("value"), Some("hello "));
  Ok(())
}

#[test]
fn backspace_deletes_grapheme_cluster_emoji_sequence() -> Result<()> {
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
        <input id="txt" value="👨‍👩‍👧‍👦">
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
  let _ = controller.handle_message(support::key_action(tab_id, KeyAction::End))?;
  let _ = controller.handle_message(support::key_action(tab_id, KeyAction::Backspace))?;

  let input = find_element_by_id(controller.document().dom(), "txt");
  assert_eq!(input.get_attribute_ref("value"), Some(""));
  Ok(())
}

#[test]
fn undo_redo_restores_text_input_value() -> Result<()> {
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
        <input id="txt" value="">
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

  let _ = controller.handle_message(support::text_input(tab_id, "a"))?;
  let _ = controller.handle_message(support::text_input(tab_id, "b"))?;
  let input = find_element_by_id(controller.document().dom(), "txt");
  assert_eq!(input.get_attribute_ref("value"), Some("ab"));

  let _ = controller.handle_message(support::key_action(tab_id, KeyAction::Undo))?;
  let input = find_element_by_id(controller.document().dom(), "txt");
  assert_eq!(input.get_attribute_ref("value"), Some("a"));

  let _ = controller.handle_message(support::key_action(tab_id, KeyAction::Redo))?;
  let input = find_element_by_id(controller.document().dom(), "txt");
  assert_eq!(input.get_attribute_ref("value"), Some("ab"));

  Ok(())
}

#[test]
fn textarea_arrow_down_moves_by_visual_lines_when_wrapped() -> Result<()> {
  let _lock = super::stage_listener_test_lock();
  let tab_id = TabId(1);
  let viewport_css = (240, 160);
  let url = "https://example.com/index.html";
  let initial = "012345678901234567890123456789";

  let html = format!(
    r#"<!doctype html>
      <html>
        <head>
          <meta charset="utf-8">
          <style>
            html, body {{ margin: 0; padding: 0; }}
            #ta {{
              position: absolute;
              left: 0;
              top: 0;
              width: 120px;
              height: 80px;
              font-family: "Noto Sans Mono";
              font-size: 20px;
            }}
          </style>
        </head>
        <body>
          <textarea id="ta">{initial}</textarea>
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

  // Focus the textarea, then force caret to the beginning.
  let click = (10.0, 10.0);
  let _ =
    controller.handle_message(support::pointer_down(tab_id, click, PointerButton::Primary))?;
  let _ = controller.handle_message(support::pointer_up(tab_id, click, PointerButton::Primary))?;
  let _ = controller.handle_message(support::key_action(tab_id, KeyAction::Home))?;

  // ArrowDown should move to the next *visual* line (soft wrap), not be a no-op.
  let _ = controller.handle_message(support::key_action(tab_id, KeyAction::ArrowDown))?;
  let _ = controller.handle_message(support::text_input(tab_id, "X"))?;

  let textarea = find_element_by_id(controller.document().dom(), "ta");
  let value = textarea
    .get_attribute_ref("data-fastr-value")
    .expect("expected textarea value after insertion");
  let x_pos = value
    .chars()
    .position(|ch| ch == 'X')
    .expect("expected inserted character to appear in textarea value");
  assert!(
    x_pos > 0,
    "expected ArrowDown to move caret to a wrapped line (inserted X at {x_pos})"
  );

  Ok(())
}

#[test]
fn textarea_shift_arrow_up_down_extends_selection_and_typing_replaces_it() -> Result<()> {
  let _lock = super::stage_listener_test_lock();
  let viewport_css = (240, 160);
  let url = "https://example.com/index.html";
  let initial = "012345678901234567890123456789";

  let html = format!(
    r#"<!doctype html>
      <html>
        <head>
          <meta charset="utf-8">
          <style>
            html, body {{ margin: 0; padding: 0; }}
            #ta {{
              position: absolute;
              left: 0;
              top: 0;
              width: 120px;
              height: 80px;
              font-family: "Noto Sans Mono";
              font-size: 20px;
            }}
          </style>
        </head>
        <body>
          <textarea id="ta">{initial}</textarea>
        </body>
      </html>
    "#
  );

  let initial_len = initial.chars().count();

  // Determine how many characters fit on a visual line (soft wrap) for the given textarea width + font.
  // This depends on UA defaults (e.g. padding/border) and can change if layout heuristics change, so
  // derive it from actual ArrowDown behaviour instead of hardcoding.
  let chars_per_visual_line = {
    let tab_id = TabId(1);
    let mut controller = BrowserTabController::from_html_with_renderer(
      support::deterministic_renderer(),
      tab_id,
      &html,
      url,
      viewport_css,
      1.0,
    )?;
    let _ = controller.handle_message(support::request_repaint(tab_id, RepaintReason::Explicit))?;

    let click = (10.0, 10.0);
    let _ = controller.handle_message(support::pointer_down(tab_id, click, PointerButton::Primary))?;
    let _ = controller.handle_message(support::pointer_up(tab_id, click, PointerButton::Primary))?;
    let _ = controller.handle_message(support::key_action(tab_id, KeyAction::Home))?;

    let _ = controller.handle_message(support::key_action(tab_id, KeyAction::ArrowDown))?;
    let _ = controller.handle_message(support::text_input(tab_id, "M"))?;

    let textarea = find_element_by_id(controller.document().dom(), "ta");
    let value = textarea
      .get_attribute_ref("data-fastr-value")
      .expect("expected textarea value after insertion");
    let m_pos = value
      .chars()
      .position(|ch| ch == 'M')
      .expect("expected inserted marker to appear in textarea value");
    assert!(
      m_pos > 0 && m_pos < initial_len,
      "expected textarea to wrap to multiple visual lines (marker at {m_pos}, initial_len={initial_len})",
    );
    m_pos
  };

  assert!(
    chars_per_visual_line.saturating_mul(2) <= initial_len,
    "expected initial textarea value to span at least 3 visual lines; got chars_per_visual_line={chars_per_visual_line}, initial_len={initial_len}",
  );

  // Shift+ArrowDown should extend selection to the next visual line.
  {
    let tab_id = TabId(1);
    let mut controller = BrowserTabController::from_html_with_renderer(
      support::deterministic_renderer(),
      tab_id,
      &html,
      url,
      viewport_css,
      1.0,
    )?;
    let _ = controller.handle_message(support::request_repaint(tab_id, RepaintReason::Explicit))?;

    let click = (10.0, 10.0);
    let _ =
      controller.handle_message(support::pointer_down(tab_id, click, PointerButton::Primary))?;
    let _ = controller.handle_message(support::pointer_up(tab_id, click, PointerButton::Primary))?;
    let _ = controller.handle_message(support::key_action(tab_id, KeyAction::Home))?;

    let _ = controller.handle_message(support::key_action(tab_id, KeyAction::ArrowDown))?;
    let _ = controller.handle_message(support::key_action(tab_id, KeyAction::ShiftArrowDown))?;
    let _ = controller.handle_message(support::text_input(tab_id, "X"))?;

    let textarea = find_element_by_id(controller.document().dom(), "ta");
    let expected = replace_range_at_char_idx(
      initial,
      chars_per_visual_line,
      chars_per_visual_line.saturating_mul(2),
      "X",
    );
    assert_eq!(
      textarea.get_attribute_ref("data-fastr-value"),
      Some(expected.as_str()),
      "expected ShiftArrowDown selection to be replaced by typed text"
    );
  }

  // Shift+ArrowUp should extend selection to the previous visual line.
  {
    let tab_id = TabId(2);
    let mut controller = BrowserTabController::from_html_with_renderer(
      support::deterministic_renderer(),
      tab_id,
      &html,
      url,
      viewport_css,
      1.0,
    )?;
    let _ = controller.handle_message(support::request_repaint(tab_id, RepaintReason::Explicit))?;

    let click = (10.0, 10.0);
    let _ =
      controller.handle_message(support::pointer_down(tab_id, click, PointerButton::Primary))?;
    let _ = controller.handle_message(support::pointer_up(tab_id, click, PointerButton::Primary))?;
    let _ = controller.handle_message(support::key_action(tab_id, KeyAction::Home))?;

    let _ = controller.handle_message(support::key_action(tab_id, KeyAction::ArrowDown))?;
    let _ = controller.handle_message(support::key_action(tab_id, KeyAction::ArrowDown))?;
    let _ = controller.handle_message(support::key_action(tab_id, KeyAction::ShiftArrowUp))?;
    let _ = controller.handle_message(support::text_input(tab_id, "Y"))?;

    let textarea = find_element_by_id(controller.document().dom(), "ta");
    let expected = replace_range_at_char_idx(
      initial,
      chars_per_visual_line,
      chars_per_visual_line.saturating_mul(2),
      "Y",
    );
    assert_eq!(
      textarea.get_attribute_ref("data-fastr-value"),
      Some(expected.as_str()),
      "expected ShiftArrowUp selection to be replaced by typed text"
    );
  }

  Ok(())
}

#[test]
fn wheel_scroll_over_textarea_scrolls_textarea_before_page() -> Result<()> {
  let _lock = super::stage_listener_test_lock();
  let tab_id = TabId(1);
  let viewport_css = (240, 160);
  let url = "https://example.com/index.html";

  let mut textarea_lines = String::new();
  for idx in 0..80 {
    textarea_lines.push_str(&format!("line {idx}\n"));
  }

  let html = format!(
    r#"<!doctype html>
      <html>
        <head>
          <meta charset="utf-8">
          <style>
            html, body {{ margin: 0; padding: 0; }}
            #ta {{
              position: absolute;
              left: 0;
              top: 0;
              width: 180px;
              height: 60px;
              font-family: "Noto Sans Mono";
              font-size: 20px;
            }}
            #spacer {{ height: 2000px; }}
          </style>
        </head>
        <body>
          <textarea id="ta">{textarea_lines}</textarea>
          <div id="spacer"></div>
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

  // First wheel scroll: textarea should consume it without scrolling the page.
  let _ = controller.handle_message(support::scroll_at_pointer(
    tab_id,
    (0.0, 120.0),
    (10.0, 10.0),
  ))?;
  assert!(
    controller.scroll_state().viewport.y.abs() <= f32::EPSILON,
    "expected page scroll to remain at 0 after scrolling textarea; got {:?}",
    controller.scroll_state().viewport
  );
  assert!(
    controller
      .scroll_state()
      .elements
      .values()
      .any(|offset| offset.y > 0.0),
    "expected textarea to have a non-zero element scroll offset after wheel scroll; got {:?}",
    controller.scroll_state().elements
  );

  // Large wheel scroll: textarea should hit its max and bubble the remaining scroll to the page.
  let _ = controller.handle_message(support::scroll_at_pointer(
    tab_id,
    (0.0, 10_000.0),
    (10.0, 10.0),
  ))?;
  assert!(
    controller.scroll_state().viewport.y > 0.0,
    "expected page to scroll after textarea reaches bounds; got {:?}",
    controller.scroll_state().viewport
  );

  Ok(())
}

#[test]
fn textarea_auto_scrolls_to_keep_caret_visible_after_typing_many_lines() -> Result<()> {
  let _lock = super::stage_listener_test_lock();
  let tab_id = TabId(1);
  let viewport_css = (240, 160);
  let url = "https://example.com/index.html";

  let html = r#"<!doctype html>
    <html>
      <head>
        <meta charset="utf-8">
        <style>
          html, body { margin: 0; padding: 0; }
          #ta { position: absolute; left: 0; top: 0; width: 180px; height: 60px; font-family: "Noto Sans Mono"; font-size: 20px; }
        </style>
      </head>
      <body>
        <textarea id="ta"></textarea>
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
  let click = (10.0, 10.0);
  let _ =
    controller.handle_message(support::pointer_down(tab_id, click, PointerButton::Primary))?;
  let _ = controller.handle_message(support::pointer_up(tab_id, click, PointerButton::Primary))?;

  // Insert enough lines to overflow the textarea height. Paint should auto-scroll so the caret
  // remains visible at the bottom.
  for _ in 0..40 {
    let _ = controller.handle_message(support::key_action(tab_id, KeyAction::Enter))?;
  }

  assert!(
    controller
      .scroll_state()
      .elements
      .values()
      .any(|offset| offset.y > 0.0),
    "expected textarea to auto-scroll after inserting many lines; got {:?}",
    controller.scroll_state().elements
  );

  Ok(())
}

#[test]
fn input_maxlength_clamps_text_input() -> Result<()> {
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
          #txt { position: absolute; left: 0; top: 0; width: 180px; height: 40px; font-family: "Noto Sans Mono"; font-size: 20px; }
        </style>
      </head>
      <body>
        <input id="txt" maxlength="3" value="">
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
  let click = (10.0, 20.0);
  let _ = controller.handle_message(support::pointer_down(tab_id, click, PointerButton::Primary))?;
  let _ = controller.handle_message(support::pointer_up(tab_id, click, PointerButton::Primary))?;

  let _ = controller.handle_message(support::text_input(tab_id, "abcd"))?;
  let input = find_element_by_id(controller.document().dom(), "txt");
  assert_eq!(input.get_attribute_ref("value"), Some("abc"));

  Ok(())
}

#[test]
fn textarea_maxlength_clamps_paste() -> Result<()> {
  let _lock = super::stage_listener_test_lock();
  let tab_id = TabId(1);
  let viewport_css = (240, 160);
  let url = "https://example.com/index.html";

  let html = r#"<!doctype html>
    <html>
      <head>
        <meta charset="utf-8">
        <style>
          html, body { margin: 0; padding: 0; }
          #ta { position: absolute; left: 0; top: 0; width: 180px; height: 80px; font-family: "Noto Sans Mono"; font-size: 20px; }
        </style>
      </head>
      <body>
        <textarea id="ta" maxlength="3"></textarea>
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
  let click = (10.0, 10.0);
  let _ = controller.handle_message(support::pointer_down(tab_id, click, PointerButton::Primary))?;
  let _ = controller.handle_message(support::pointer_up(tab_id, click, PointerButton::Primary))?;

  let _ = controller.handle_message(UiToWorker::Paste {
    tab_id,
    text: "abcd".to_string(),
  })?;
  let textarea = find_element_by_id(controller.document().dom(), "ta");
  assert_eq!(textarea.get_attribute_ref("data-fastr-value"), Some("abc"));

  Ok(())
}

#[test]
fn text_input_auto_scrolls_horizontally_to_keep_caret_visible() -> Result<()> {
  let _lock = super::stage_listener_test_lock();
  let tab_id = TabId(1);
  let viewport_css = (240, 120);
  let url = "https://example.com/index.html";

  let value = "0123456789".repeat(10);
  let html = format!(
    r#"<!doctype html>
      <html>
        <head>
          <meta charset="utf-8">
          <style>
            html, body {{ margin: 0; padding: 0; }}
            #txt {{ position: absolute; left: 0; top: 0; width: 140px; height: 40px; font-family: "Noto Sans Mono"; font-size: 20px; }}
          </style>
        </head>
        <body>
          <input id="txt" value="{value}">
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

  // Focus the input and move the caret to the end of the value.
  let click = (10.0, 10.0);
  let _ = controller.handle_message(support::pointer_down(tab_id, click, PointerButton::Primary))?;
  let _ = controller.handle_message(support::pointer_up(tab_id, click, PointerButton::Primary))?;
  let _ = controller.handle_message(support::key_action(tab_id, KeyAction::End))?;

  assert!(
    controller
      .scroll_state()
      .elements
      .values()
      .any(|offset| offset.x > 0.0),
    "expected text input to auto-scroll horizontally after moving caret to end; got {:?}",
    controller.scroll_state().elements
  );

  // Move caret back to the start and ensure the horizontal scroll offset returns to 0.
  let _ = controller.handle_message(support::key_action(tab_id, KeyAction::Home))?;
  assert!(
    !controller
      .scroll_state()
      .elements
      .values()
      .any(|offset| offset.x > 0.0),
    "expected horizontal scroll offset to return to 0 after moving caret to start; got {:?}",
    controller.scroll_state().elements
  );

  Ok(())
}

#[test]
fn textarea_auto_scrolls_after_caret_moves_to_end_across_wrapped_lines() -> Result<()> {
  let _lock = super::stage_listener_test_lock();
  let tab_id = TabId(1);
  let viewport_css = (240, 160);
  let url = "https://example.com/index.html";

  let long_text = "a".repeat(400);
  let html = format!(
    r#"<!doctype html>
      <html>
        <head>
          <meta charset="utf-8">
          <style>
            html, body {{ margin: 0; padding: 0; }}
            #ta {{ position: absolute; left: 0; top: 0; width: 180px; height: 60px; font-family: "Noto Sans Mono"; font-size: 20px; }}
          </style>
        </head>
        <body>
          <textarea id="ta">{long_text}</textarea>
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

  // Focus near the start of the textarea.
  let click = (10.0, 10.0);
  let _ =
    controller.handle_message(support::pointer_down(tab_id, click, PointerButton::Primary))?;
  let _ = controller.handle_message(support::pointer_up(tab_id, click, PointerButton::Primary))?;

  // Move caret to end; this should scroll within the textarea because the content wraps into many
  // visual lines.
  let _ = controller.handle_message(support::key_action(tab_id, KeyAction::End))?;

  assert!(
    controller
      .scroll_state()
      .elements
      .values()
      .any(|offset| offset.y > 0.0),
    "expected textarea to auto-scroll after moving caret to end; got {:?}",
    controller.scroll_state().elements
  );

  Ok(())
}

#[test]
fn ime_preedit_commit_cancel_routes_through_browser_tab_controller() -> Result<()> {
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
          #txt { position: absolute; left: 0; top: 0; width: 180px; height: 40px; font-family: "Noto Sans Mono"; font-size: 20px; }
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

  // Focus the input.
  let click = (10.0, 10.0);
  let _ = controller.handle_message(support::pointer_down(tab_id, click, PointerButton::Primary))?;
  let _ = controller.handle_message(support::pointer_up(tab_id, click, PointerButton::Primary))?;

  // Place caret after the first character.
  let _ = controller.handle_message(support::key_action(tab_id, KeyAction::Home))?;
  let _ = controller.handle_message(support::key_action(tab_id, KeyAction::ArrowRight))?;

  let _ = controller.handle_message(UiToWorker::ImePreedit {
    tab_id,
    text: "X".to_string(),
    cursor: None,
  })?;
  let input = find_element_by_id(controller.document().dom(), "txt");
  assert_eq!(
    input.get_attribute_ref("value"),
    Some("abc"),
    "IME preedit should not mutate the input value attribute"
  );

  let _ = controller.handle_message(UiToWorker::ImeCommit {
    tab_id,
    text: "X".to_string(),
  })?;
  let input = find_element_by_id(controller.document().dom(), "txt");
  assert_eq!(
    input.get_attribute_ref("value"),
    Some("aXbc"),
    "IME commit should insert committed text at the caret"
  );

  let _ = controller.handle_message(UiToWorker::ImePreedit {
    tab_id,
    text: "Y".to_string(),
    cursor: None,
  })?;
  let input = find_element_by_id(controller.document().dom(), "txt");
  assert_eq!(
    input.get_attribute_ref("value"),
    Some("aXbc"),
    "IME preedit should not mutate the input value attribute after commit"
  );

  let _ = controller.handle_message(UiToWorker::ImeCancel { tab_id })?;
  let input = find_element_by_id(controller.document().dom(), "txt");
  assert_eq!(
    input.get_attribute_ref("value"),
    Some("aXbc"),
    "IME cancel should not mutate the input value attribute"
  );

  Ok(())
}

#[test]
fn input_maxlength_clamps_typed_and_pasted_ascii() -> Result<()> {
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
          #txt { position: absolute; left: 0; top: 0; width: 200px; height: 30px; }
        </style>
      </head>
      <body>
        <input id="txt" maxlength="5" value="">
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

  let click = (10.0, 15.0);
  let _ = controller.handle_message(support::pointer_down(tab_id, click, PointerButton::Primary))?;
  let _ = controller.handle_message(support::pointer_up(tab_id, click, PointerButton::Primary))?;

  let _ = controller.handle_message(support::text_input(tab_id, "abcdefg"))?;
  let input = find_element_by_id(controller.document().dom(), "txt");
  assert_eq!(input.get_attribute_ref("value"), Some("abcde"));

  // Pasting should be clamped as well (replace selection).
  let _ = controller.handle_message(support::key_action(tab_id, KeyAction::SelectAll))?;
  let _ = controller.handle_message(UiToWorker::Paste {
    tab_id,
    text: "1234567".to_string(),
  })?;
  let input = find_element_by_id(controller.document().dom(), "txt");
  assert_eq!(input.get_attribute_ref("value"), Some("12345"));

  Ok(())
}

#[test]
fn input_maxlength_clamps_paste_replacement_to_selection_capacity() -> Result<()> {
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
          #txt { position: absolute; left: 0; top: 0; width: 200px; height: 30px; }
        </style>
      </head>
      <body>
        <input id="txt" maxlength="5" value="abcde">
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

  let click = (10.0, 15.0);
  let _ = controller.handle_message(support::pointer_down(tab_id, click, PointerButton::Primary))?;
  let _ = controller.handle_message(support::pointer_up(tab_id, click, PointerButton::Primary))?;
  let _ = controller.handle_message(support::key_action(tab_id, KeyAction::End))?;

  // Move caret between "d" and "e", then select "cd".
  let _ = controller.handle_message(support::key_action(tab_id, KeyAction::ArrowLeft))?;
  let _ = controller.handle_message(support::key_action(tab_id, KeyAction::ShiftArrowLeft))?;
  let _ = controller.handle_message(support::key_action(tab_id, KeyAction::ShiftArrowLeft))?;

  // "abcde", replace "cd" with "ZZZ" under maxlength=5 should clamp to "ZZ".
  let _ = controller.handle_message(UiToWorker::Paste {
    tab_id,
    text: "ZZZ".to_string(),
  })?;

  let input = find_element_by_id(controller.document().dom(), "txt");
  assert_eq!(input.get_attribute_ref("value"), Some("abZZe"));
  Ok(())
}

#[test]
fn input_maxlength_enforces_utf16_code_unit_length() -> Result<()> {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
  let _lock = super::stage_listener_test_lock();
  let tab_id = TabId(1);
  let viewport_css = (240, 140);
  let url = "https://example.com/index.html";

  let html = r#"<!doctype html>
    <html>
      <head>
        <meta charset="utf-8">
        <style>
          html, body { margin: 0; padding: 0; }
          #i2 { position: absolute; left: 0; top: 0; width: 200px; height: 30px; }
          #i1 { position: absolute; left: 0; top: 50px; width: 200px; height: 30px; }
        </style>
      </head>
      <body>
        <input id="i2" maxlength="2" value="">
        <input id="i1" maxlength="1" value="">
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

  let emoji = "😀"; // U+1F600, 2 UTF-16 code units.

  // maxlength=2 should accept a single emoji.
  let click = (10.0, 15.0);
  let _ = controller.handle_message(support::pointer_down(tab_id, click, PointerButton::Primary))?;
  let _ = controller.handle_message(support::pointer_up(tab_id, click, PointerButton::Primary))?;
  let _ = controller.handle_message(support::text_input(tab_id, emoji))?;
  let input = find_element_by_id(controller.document().dom(), "i2");
  assert_eq!(input.get_attribute_ref("value"), Some(emoji));

  // maxlength=1 should reject a single emoji (cannot split the surrogate pair).
  let click = (10.0, 65.0);
  let _ = controller.handle_message(support::pointer_down(tab_id, click, PointerButton::Primary))?;
  let _ = controller.handle_message(support::pointer_up(tab_id, click, PointerButton::Primary))?;
  let _ = controller.handle_message(support::text_input(tab_id, emoji))?;
  let input = find_element_by_id(controller.document().dom(), "i1");
  assert_eq!(input.get_attribute_ref("value").unwrap_or(""), "");

  Ok(())
}

#[test]
fn textarea_maxlength_clamps_typed_and_pasted_ascii() -> Result<()> {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
  let _lock = super::stage_listener_test_lock();
  let tab_id = TabId(1);
  let viewport_css = (240, 160);
  let url = "https://example.com/index.html";

  let html = r#"<!doctype html>
    <html>
      <head>
        <meta charset="utf-8">
        <style>
          html, body { margin: 0; padding: 0; }
          #ta { position: absolute; left: 0; top: 0; width: 200px; height: 120px; }
        </style>
      </head>
      <body>
        <textarea id="ta" maxlength="5"></textarea>
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

  let click = (10.0, 15.0);
  let _ = controller.handle_message(support::pointer_down(tab_id, click, PointerButton::Primary))?;
  let _ = controller.handle_message(support::pointer_up(tab_id, click, PointerButton::Primary))?;

  let _ = controller.handle_message(support::text_input(tab_id, "abcdefg"))?;
  let textarea = find_element_by_id(controller.document().dom(), "ta");
  assert_eq!(textarea.get_attribute_ref("data-fastr-value"), Some("abcde"));

  let _ = controller.handle_message(support::key_action(tab_id, KeyAction::SelectAll))?;
  let _ = controller.handle_message(UiToWorker::Paste {
    tab_id,
    text: "1234567".to_string(),
  })?;
  let textarea = find_element_by_id(controller.document().dom(), "ta");
  assert_eq!(textarea.get_attribute_ref("data-fastr-value"), Some("12345"));

  Ok(())
}

#[test]
fn textarea_maxlength_clamps_paste_replacement_to_selection_capacity() -> Result<()> {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
  let _lock = super::stage_listener_test_lock();
  let tab_id = TabId(1);
  let viewport_css = (240, 160);
  let url = "https://example.com/index.html";

  let html = r#"<!doctype html>
    <html>
      <head>
        <meta charset="utf-8">
        <style>
          html, body { margin: 0; padding: 0; }
          #ta { position: absolute; left: 0; top: 0; width: 200px; height: 120px; }
        </style>
      </head>
      <body>
        <textarea id="ta" maxlength="5">abcde</textarea>
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

  let click = (10.0, 15.0);
  let _ = controller.handle_message(support::pointer_down(tab_id, click, PointerButton::Primary))?;
  let _ = controller.handle_message(support::pointer_up(tab_id, click, PointerButton::Primary))?;
  let _ = controller.handle_message(support::key_action(tab_id, KeyAction::End))?;

  // Move caret between "d" and "e", then select "cd".
  let _ = controller.handle_message(support::key_action(tab_id, KeyAction::ArrowLeft))?;
  let _ = controller.handle_message(support::key_action(tab_id, KeyAction::ShiftArrowLeft))?;
  let _ = controller.handle_message(support::key_action(tab_id, KeyAction::ShiftArrowLeft))?;

  let _ = controller.handle_message(UiToWorker::Paste {
    tab_id,
    text: "ZZZ".to_string(),
  })?;

  let textarea = find_element_by_id(controller.document().dom(), "ta");
  assert_eq!(textarea.get_attribute_ref("data-fastr-value"), Some("abZZe"));
  Ok(())
}

#[test]
fn textarea_maxlength_enforces_utf16_code_unit_length() -> Result<()> {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
  let _lock = super::stage_listener_test_lock();
  let tab_id = TabId(1);
  let viewport_css = (240, 220);
  let url = "https://example.com/index.html";

  let html = r#"<!doctype html>
    <html>
      <head>
        <meta charset="utf-8">
        <style>
          html, body { margin: 0; padding: 0; }
          #t2 { position: absolute; left: 0; top: 0; width: 200px; height: 60px; }
          #t1 { position: absolute; left: 0; top: 100px; width: 200px; height: 60px; }
        </style>
      </head>
      <body>
        <textarea id="t2" maxlength="2"></textarea>
        <textarea id="t1" maxlength="1"></textarea>
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

  let emoji = "😀"; // U+1F600, 2 UTF-16 code units.

  // maxlength=2 should accept a single emoji.
  let click = (10.0, 15.0);
  let _ = controller.handle_message(support::pointer_down(tab_id, click, PointerButton::Primary))?;
  let _ = controller.handle_message(support::pointer_up(tab_id, click, PointerButton::Primary))?;
  let _ = controller.handle_message(support::text_input(tab_id, emoji))?;
  let textarea = find_element_by_id(controller.document().dom(), "t2");
  assert_eq!(textarea.get_attribute_ref("data-fastr-value"), Some(emoji));

  // maxlength=1 should reject a single emoji (cannot split the surrogate pair).
  let click = (10.0, 115.0);
  let _ = controller.handle_message(support::pointer_down(tab_id, click, PointerButton::Primary))?;
  let _ = controller.handle_message(support::pointer_up(tab_id, click, PointerButton::Primary))?;
  let _ = controller.handle_message(support::text_input(tab_id, emoji))?;
  let textarea = find_element_by_id(controller.document().dom(), "t1");
  assert_eq!(textarea.get_attribute_ref("data-fastr-value").unwrap_or(""), "");

  Ok(())
}
