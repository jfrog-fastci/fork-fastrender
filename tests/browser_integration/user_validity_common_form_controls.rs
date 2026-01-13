use fastrender::dom::{enumerate_dom_ids, DomNode};
use fastrender::interaction::{InteractionEngine, KeyAction};
use fastrender::ui::messages::{PointerButton, PointerModifiers};
use fastrender::{BrowserDocument, Point, RenderOptions, Result};
use tempfile::tempdir;

use super::support;

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

fn dom_has_attr(root: &DomNode, name: &str) -> bool {
  let mut stack = vec![root];
  while let Some(node) = stack.pop() {
    if node.get_attribute_ref(name).is_some() {
      return true;
    }
    for child in node.children.iter().rev() {
      stack.push(child);
    }
  }
  false
}

#[test]
fn user_validity_text_input_edit_sets_flag() -> Result<()> {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
  #[cfg(feature = "browser_ui")]
  let _lock = super::stage_listener_test_lock();

  let html = r#"<!doctype html>
    <html>
      <head>
        <style>
          html, body { margin: 0; padding: 0; }
          #i { position: absolute; left: 0; top: 0; width: 160px; height: 30px; }
        </style>
      </head>
      <body>
        <input id="i" required>
      </body>
    </html>
  "#;

  let options = RenderOptions::new().with_viewport(200, 80);
  let mut doc = BrowserDocument::new(support::deterministic_renderer(), html, options)?;
  doc.render_frame_with_scroll_state()?;

  let click_point = Point::new(10.0, 10.0);
  let scroll_state = doc.scroll_state();
  let mut engine = InteractionEngine::new();

  doc.mutate_dom_with_layout_artifacts(|dom, box_tree, fragment_tree| {
    let mut changed = engine.pointer_down(dom, box_tree, fragment_tree, &scroll_state, click_point);
    let (up_changed, _action) = engine.pointer_up_with_scroll(
      dom,
      box_tree,
      fragment_tree,
      &scroll_state,
      click_point,
      PointerButton::Primary,
      PointerModifiers::NONE,
      true,
      "https://example.invalid/page.html",
      "https://example.invalid/page.html",
    );
    changed |= up_changed;
    changed |= engine.text_input(dom, "a");
    changed |= engine.key_action(dom, KeyAction::Backspace);
    (changed, ())
  })?;

  let input = find_element_by_id(doc.dom(), "i");
  assert_eq!(
    input.get_attribute_ref("value").unwrap_or(""),
    "",
    "expected text input value to be empty after typing and backspacing"
  );
  assert!(
    !dom_has_attr(doc.dom(), "data-fastr-user-validity"),
    "renderer must not inject data-fastr-user-validity onto the DOM"
  );

  let input_id = *enumerate_dom_ids(doc.dom())
    .get(&(input as *const DomNode))
    .expect("input node id");
  assert!(
    engine.interaction_state().has_user_validity(input_id),
    "expected text input to flip internal user validity state after edit"
  );

  Ok(())
}

#[test]
fn user_validity_range_drag_sets_flag() -> Result<()> {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
  #[cfg(feature = "browser_ui")]
  let _lock = super::stage_listener_test_lock();

  let html = r#"<!doctype html>
    <html>
      <head>
        <style>
          html, body { margin: 0; padding: 0; }
          #r { position: absolute; left: 0; top: 0; width: 200px; height: 30px; }
        </style>
      </head>
      <body>
        <input id="r" type="range" min="0" max="10" value="0">
      </body>
    </html>
  "#;

  let options = RenderOptions::new().with_viewport(240, 80);
  let mut doc = BrowserDocument::new(support::deterministic_renderer(), html, options)?;
  doc.render_frame_with_scroll_state()?;

  let scroll_state = doc.scroll_state();
  let click_point = Point::new(150.0, 10.0);
  let mut engine = InteractionEngine::new();
  doc.mutate_dom_with_layout_artifacts(|dom, box_tree, fragment_tree| {
    let mut changed = engine.pointer_down(dom, box_tree, fragment_tree, &scroll_state, click_point);
    let (up_changed, _action) = engine.pointer_up_with_scroll(
      dom,
      box_tree,
      fragment_tree,
      &scroll_state,
      click_point,
      PointerButton::Primary,
      PointerModifiers::NONE,
      true,
      "https://example.invalid/page.html",
      "https://example.invalid/page.html",
    );
    changed |= up_changed;
    (changed, ())
  })?;

  let range = find_element_by_id(doc.dom(), "r");
  let value = range.get_attribute_ref("value").unwrap_or("0");
  assert_ne!(
    value, "0",
    "expected clicking the range track to update its value"
  );
  assert!(
    !dom_has_attr(doc.dom(), "data-fastr-user-validity"),
    "renderer must not inject data-fastr-user-validity onto the DOM"
  );

  let range_id = *enumerate_dom_ids(doc.dom())
    .get(&(range as *const DomNode))
    .expect("range node id");
  assert!(
    engine.interaction_state().has_user_validity(range_id),
    "expected range input to flip internal user validity state after user value change"
  );

  Ok(())
}

#[test]
fn user_validity_file_input_drop_sets_flag() -> Result<()> {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
  #[cfg(feature = "browser_ui")]
  let _lock = super::stage_listener_test_lock();

  let html = r#"<!doctype html>
    <html>
      <head>
        <style>
          html, body { margin: 0; padding: 0; }
          #f { position: absolute; left: 0; top: 0; width: 240px; height: 40px; }
        </style>
      </head>
      <body>
        <input id="f" type="file">
      </body>
    </html>
  "#;

  let options = RenderOptions::new().with_viewport(320, 120);
  let mut doc = BrowserDocument::new(support::deterministic_renderer(), html, options)?;
  let mut engine = InteractionEngine::new();
  doc.render_frame_with_scroll_state_and_interaction_state(Some(engine.interaction_state()))?;

  let dir = tempdir().expect("temp dir");
  let file_path = dir.path().join("upload.txt");
  std::fs::write(&file_path, b"hello").expect("write upload.txt");

  let scroll_state = doc.scroll_state();
  let drop_point = Point::new(10.0, 10.0);
  doc.mutate_dom_with_layout_artifacts(|dom, box_tree, fragment_tree| {
    let changed = engine.drop_files_with_scroll(
      dom,
      box_tree,
      fragment_tree,
      &scroll_state,
      drop_point,
      &[file_path.clone()],
    );
    (changed, ())
  })?;

  let file_input = find_element_by_id(doc.dom(), "f");
  let value = file_input
    .get_attribute_ref("data-fastr-file-value")
    .unwrap_or("");
  assert!(
    value.contains("upload.txt"),
    "expected file input to reflect dropped filename; value={value:?}"
  );
  assert!(
    !dom_has_attr(doc.dom(), "data-fastr-user-validity"),
    "renderer must not inject data-fastr-user-validity onto the DOM"
  );

  let file_id = *enumerate_dom_ids(doc.dom())
    .get(&(file_input as *const DomNode))
    .expect("file input node id");
  assert!(
    engine.interaction_state().has_user_validity(file_id),
    "expected file input to flip internal user validity state after drop"
  );

  Ok(())
}
