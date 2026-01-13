use fastrender::dom::DomNode;
use fastrender::interaction::InteractionEngine;
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

#[test]
fn file_input_accept_filters_drop_files() -> Result<()> {
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
        </style>
      </head>
      <body>
        <input id="f" type="file" accept=".txt">
      </body>
    </html>
  "#;

  let options = RenderOptions::new().with_viewport(320, 120);
  let mut doc = BrowserDocument::new(support::deterministic_renderer(), html, options)?;
  let mut engine = InteractionEngine::new();

  doc.render_frame_with_scroll_state_and_interaction_state(Some(engine.interaction_state()))?;

  let dir = tempdir().expect("temp dir");
  let png_path = dir.path().join("a.png");
  let txt_path = dir.path().join("a.txt");
  std::fs::write(&png_path, b"png-bytes").expect("write a.png");
  std::fs::write(&txt_path, b"txt-bytes").expect("write a.txt");

  let scroll_state = doc.scroll_state();
  let drop_point = Point::new(10.0, 10.0);

  // Drop a non-accepted file.
  let _changed = doc.mutate_dom_with_layout_artifacts(|dom, box_tree, fragment_tree| {
    let changed = engine.drop_files_with_scroll(
      dom,
      box_tree,
      fragment_tree,
      &scroll_state,
      drop_point,
      &[png_path.clone()],
    );
    (changed, changed)
  })?;

  assert!(
    engine.interaction_state().form_state().file_inputs.is_empty(),
    "expected file input accept=.txt to reject dropped .png; state={:?}",
    engine.interaction_state().form_state().file_inputs
  );
  let file_node = find_element_by_id(doc.dom(), "f");
  assert_eq!(
    file_node.get_attribute_ref("data-fastr-file-value"),
    None,
    "expected data-fastr-file-value to remain absent after dropping rejected file"
  );

  // Drop an accepted file.
  let _changed = doc.mutate_dom_with_layout_artifacts(|dom, box_tree, fragment_tree| {
    let changed = engine.drop_files_with_scroll(
      dom,
      box_tree,
      fragment_tree,
      &scroll_state,
      drop_point,
      &[txt_path.clone()],
    );
    (changed, changed)
  })?;

  let selected: Vec<String> = engine
    .interaction_state()
    .form_state()
    .file_inputs
    .values()
    .flat_map(|files| files.iter().map(|file| file.filename.clone()))
    .collect();
  assert_eq!(
    selected,
    vec!["a.txt".to_string()],
    "expected file input accept=.txt to accept dropped .txt"
  );

  let file_node = find_element_by_id(doc.dom(), "f");
  let stored = file_node
    .get_attribute_ref("data-fastr-file-value")
    .expect("expected data-fastr-file-value after dropping accepted file");
  assert!(
    stored.contains("a.txt"),
    "expected data-fastr-file-value to reference selected filename; got {stored:?}"
  );

  Ok(())
}
