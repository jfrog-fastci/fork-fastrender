use fastrender::dom::DomNode;
use fastrender::interaction::InteractionEngine;
use fastrender::tree::box_tree::{FormControlKind, ReplacedType};
use fastrender::ui::messages::{PointerButton, PointerModifiers};
use fastrender::{BoxType, BrowserDocument, Point, RenderOptions, Result};
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

fn find_file_control_value(
  node: &fastrender::tree::box_tree::BoxNode,
) -> Option<Option<String>> {
  if let BoxType::Replaced(replaced) = &node.box_type {
    if let ReplacedType::FormControl(control) = &replaced.replaced_type {
      if let FormControlKind::File { value } = &control.control {
        return Some(value.clone());
      }
    }
  }

  if let Some(body) = node.footnote_body.as_deref() {
    if let Some(value) = find_file_control_value(body) {
      return Some(value);
    }
  }

  for child in &node.children {
    if let Some(value) = find_file_control_value(child) {
      return Some(value);
    }
  }

  None
}

#[test]
fn form_reset_clears_file_input_selection() -> Result<()> {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
  #[cfg(feature = "browser_ui")]
  let _lock = super::stage_listener_test_lock();

  let html = r#"<!doctype html>
<html>
  <head>
    <meta charset="utf-8">
    <style>
      html, body { margin: 0; padding: 0; }
      #f { position: absolute; left: 0; top: 0; width: 240px; height: 40px; }
      #reset { position: absolute; left: 0; top: 60px; width: 120px; height: 40px; }
    </style>
  </head>
  <body>
    <form id="form">
      <input id="f" type="file" name="up">
      <input id="reset" type="reset" value="Reset">
    </form>
  </body>
</html>
"#;

  let options = RenderOptions::new().with_viewport(320, 140);
  let mut doc = BrowserDocument::new(support::deterministic_renderer(), html, options)?;
  let mut engine = InteractionEngine::new();

  doc.render_frame_with_scroll_state_and_interaction_state(Some(engine.interaction_state()))?;

  // Create a deterministic temp file so we can drop it onto the input.
  let dir = tempdir().expect("temp dir");
  let file_path = dir.path().join("a.txt");
  std::fs::write(&file_path, b"hello-a").expect("write a.txt");

  let scroll_state = doc.scroll_state();
  let drop_point = Point::new(10.0, 10.0);
  let _drop_changed = doc.mutate_dom_with_layout_artifacts(|dom, box_tree, fragment_tree| {
    let changed = engine.drop_files_with_scroll(
      dom,
      box_tree,
      fragment_tree,
      &scroll_state,
      drop_point,
      &[file_path.clone()],
    );
    (changed, changed)
  })?;

  // File selection should be reflected in internal state and the synthetic value attribute.
  assert_eq!(engine.interaction_state().form_state.file_inputs.len(), 1);
  let file_node = find_element_by_id(doc.dom(), "f");
  assert!(
    file_node.get_attribute_ref("data-fastr-file-value").is_some(),
    "expected file input to have a synthetic data-fastr-file-value after drop"
  );

  doc.render_frame_with_scroll_state_and_interaction_state(Some(engine.interaction_state()))?;
  let prepared = doc
    .prepared()
    .expect("expected BrowserDocument to have cached layout after render");
  let file_value = find_file_control_value(&prepared.box_tree().root).expect("file input control");
  assert!(
    file_value
      .as_deref()
      .is_some_and(|value| value.contains("a.txt")),
    "expected file input to report selected file path after drop, got {file_value:?}"
  );

  // Click the reset control.
  let reset_point = Point::new(10.0, 70.0);
  let _action = doc.mutate_dom_with_layout_artifacts(|dom, box_tree, fragment_tree| {
    let down_changed = engine.pointer_down(dom, box_tree, fragment_tree, &scroll_state, reset_point);
    let (up_changed, action) = engine.pointer_up_with_scroll(
      dom,
      box_tree,
      fragment_tree,
      &scroll_state,
      reset_point,
      PointerButton::Primary,
      PointerModifiers::NONE,
      "https://example.invalid/page.html",
      "https://example.invalid/page.html",
    );
    (down_changed || up_changed, action)
  })?;

  // Reset should clear the internal file selection state and synthetic value string.
  assert!(
    engine.interaction_state().form_state.file_inputs.is_empty(),
    "expected reset to clear form_state.file_inputs"
  );
  let file_node = find_element_by_id(doc.dom(), "f");
  assert_eq!(
    file_node.get_attribute_ref("data-fastr-file-value"),
    None,
    "expected reset to clear data-fastr-file-value"
  );

  // Re-render and ensure the file control no longer reports a selected value for painting.
  doc.render_frame_with_scroll_state_and_interaction_state(Some(engine.interaction_state()))?;
  let prepared = doc
    .prepared()
    .expect("expected BrowserDocument to have cached layout after reset render");
  let file_value = find_file_control_value(&prepared.box_tree().root).expect("file input control");
  assert!(
    file_value.is_none(),
    "expected file input to have no selected value after reset, got {file_value:?}"
  );

  Ok(())
}
