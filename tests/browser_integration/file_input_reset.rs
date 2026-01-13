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

fn collect_file_control_values(node: &fastrender::tree::box_tree::BoxNode, out: &mut Vec<Option<String>>) {
  if let Some(control) = node.form_control.as_deref() {
    if let FormControlKind::File { value } = &control.control {
      out.push(value.clone());
    }
  }

  if let BoxType::Replaced(replaced) = &node.box_type {
    if let ReplacedType::FormControl(control) = &replaced.replaced_type {
      if let FormControlKind::File { value } = &control.control {
        out.push(value.clone());
      }
    }
  }

  if let Some(body) = node.footnote_body.as_deref() {
    collect_file_control_values(body, out);
  }

  for child in &node.children {
    collect_file_control_values(child, out);
  }
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
       #in { position: absolute; left: 0; top: 0; width: 240px; height: 40px; }
       #out { position: absolute; left: 0; top: 45px; width: 240px; height: 40px; }
       #reset { position: absolute; left: 0; top: 100px; width: 120px; height: 40px; }
       #unrelated { position: absolute; left: 0; top: 145px; width: 240px; height: 40px; }
     </style>
   </head>
   <body>
     <form id="form">
       <input id="in" type="file" name="up_in">
       <input id="reset" type="reset" value="Reset">
     </form>
      <input id="out" type="file" name="up_out" form="form">
      <input id="unrelated" type="file" name="up_unrelated">
    </body>
  </html>
  "#;

  let options = RenderOptions::new().with_viewport(320, 220);
  let mut doc = BrowserDocument::new(support::deterministic_renderer(), html, options)?;
  let mut engine = InteractionEngine::new();

  doc.render_frame_with_scroll_state_and_interaction_state(Some(engine.interaction_state()))?;

  // Create a deterministic temp file so we can drop it onto the input.
  let dir = tempdir().expect("temp dir");
  let file_a = dir.path().join("a.txt");
  let file_b = dir.path().join("b.txt");
  let file_c = dir.path().join("c.txt");
  std::fs::write(&file_a, b"hello-a").expect("write a.txt");
  std::fs::write(&file_b, b"hello-b").expect("write b.txt");
  std::fs::write(&file_c, b"hello-c").expect("write c.txt");

  let scroll_state = doc.scroll_state();
  let drop_in_point = Point::new(10.0, 10.0);
  let drop_out_point = Point::new(10.0, 55.0);
  let drop_unrelated_point = Point::new(10.0, 155.0);
  let _drop_changed = doc.mutate_dom_with_layout_artifacts(|dom, box_tree, fragment_tree| {
    let changed_in = engine.drop_files_with_scroll(
      dom,
      box_tree,
      fragment_tree,
      &scroll_state,
      drop_in_point,
      &[file_a.clone()],
    );
    let changed_out = engine.drop_files_with_scroll(
      dom,
      box_tree,
      fragment_tree,
      &scroll_state,
      drop_out_point,
      &[file_b.clone()],
    );
    let changed_unrelated = engine.drop_files_with_scroll(
      dom,
      box_tree,
      fragment_tree,
      &scroll_state,
      drop_unrelated_point,
      &[file_c.clone()],
    );
    let changed = changed_in || changed_out || changed_unrelated;
    (changed, changed)
  })?;

  // File selection should be reflected in internal state and the synthetic value attribute.
  assert_eq!(engine.interaction_state().form_state().file_inputs.len(), 3);
  for (id, expected_name) in [("in", "a.txt"), ("out", "b.txt"), ("unrelated", "c.txt")] {
    let file_node = find_element_by_id(doc.dom(), id);
    let stored = file_node
      .get_attribute_ref("data-fastr-file-value")
      .expect("expected file input to have a synthetic data-fastr-file-value after drop");
    assert!(
      stored.contains(expected_name),
      "expected file input {id} to contain {expected_name}, got {stored:?}"
    );
  }

  doc.render_frame_with_scroll_state_and_interaction_state(Some(engine.interaction_state()))?;
  let prepared = doc
    .prepared()
    .expect("expected BrowserDocument to have cached layout after render");
  let mut values = Vec::new();
  collect_file_control_values(&prepared.box_tree().root, &mut values);
  assert_eq!(values.len(), 3, "expected three file input controls");
  assert!(
    values
      .iter()
      .any(|value| value.as_deref().is_some_and(|value| value.contains("a.txt"))),
    "expected one file input to report selected file a.txt, got {values:?}"
  );
  assert!(
    values
      .iter()
      .any(|value| value.as_deref().is_some_and(|value| value.contains("b.txt"))),
    "expected one file input to report selected file b.txt, got {values:?}"
  );
  assert!(
    values
      .iter()
      .any(|value| value.as_deref().is_some_and(|value| value.contains("c.txt"))),
    "expected one file input to report selected file c.txt, got {values:?}"
  );

  // Click the reset control.
  let reset_point = Point::new(10.0, 110.0);
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
      true,
      "https://example.invalid/page.html",
      "https://example.invalid/page.html",
    );
    (down_changed || up_changed, action)
  })?;

  // Reset should clear the internal file selection state and synthetic value string.
  let remaining: Vec<String> = engine
    .interaction_state()
    .form_state()
    .file_inputs
    .values()
    .flat_map(|files| files.iter().map(|f| f.filename.clone()))
    .collect();
  assert_eq!(
    remaining,
    vec!["c.txt".to_string()],
    "expected reset to clear only file inputs associated with the form; unrelated selections should remain"
  );
  for id in ["in", "out"] {
    let file_node = find_element_by_id(doc.dom(), id);
    assert_eq!(
      file_node.get_attribute_ref("data-fastr-file-value"),
      None,
      "expected reset to clear data-fastr-file-value for {id}"
    );
  }
  let unrelated_node = find_element_by_id(doc.dom(), "unrelated");
  assert!(
    unrelated_node
      .get_attribute_ref("data-fastr-file-value")
      .is_some_and(|v| v.contains("c.txt")),
    "expected reset not to clear data-fastr-file-value for unrelated file input"
  );

  // Re-render and ensure the file control no longer reports a selected value for painting.
  doc.render_frame_with_scroll_state_and_interaction_state(Some(engine.interaction_state()))?;
  let prepared = doc
    .prepared()
    .expect("expected BrowserDocument to have cached layout after reset render");
  let mut values = Vec::new();
  collect_file_control_values(&prepared.box_tree().root, &mut values);
  assert_eq!(values.len(), 3, "expected three file input controls after reset");
  assert!(
    values.iter().filter(|value| value.is_none()).count() == 2,
    "expected in-form and form-associated file inputs to have no selected value after reset, got {values:?}"
  );
  assert!(
    values
      .iter()
      .any(|value| value.as_deref().is_some_and(|value| value.contains("c.txt"))),
    "expected unrelated file input to remain selected after reset, got {values:?}"
  );

  Ok(())
}
