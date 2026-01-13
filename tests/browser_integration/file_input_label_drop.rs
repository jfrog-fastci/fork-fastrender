use fastrender::dom::DomNode;
use fastrender::interaction::InteractionEngine;
use fastrender::{BrowserDocument, Point, RenderOptions, Result};
use tempfile::tempdir;

use super::support;

fn find_dom_node_id_by_html_id(root: &DomNode, html_id: &str) -> Option<usize> {
  // Match `dom::enumerate_dom_ids`: 1-based pre-order traversal.
  let mut next_id = 1usize;
  let mut stack: Vec<&DomNode> = vec![root];
  while let Some(node) = stack.pop() {
    let id = next_id;
    next_id += 1;

    if node.is_element() && node.get_attribute_ref("id") == Some(html_id) {
      return Some(id);
    }

    for child in node.children.iter().rev() {
      stack.push(child);
    }
  }
  None
}

fn find_dom_attribute_by_html_id(root: &DomNode, html_id: &str, attr: &str) -> Option<String> {
  let mut stack: Vec<&DomNode> = vec![root];
  while let Some(node) = stack.pop() {
    if node.is_element() && node.get_attribute_ref("id") == Some(html_id) {
      return node.get_attribute_ref(attr).map(str::to_string);
    }
    for child in node.children.iter().rev() {
      stack.push(child);
    }
  }
  None
}

#[test]
fn dropping_file_on_label_targets_associated_file_input() -> Result<()> {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
  #[cfg(feature = "browser_ui")]
  let _lock = super::stage_listener_test_lock();

  let html = r#"<!doctype html>
    <html>
      <head>
        <meta charset="utf-8">
        <style>
          html, body { margin: 0; padding: 0; }
          #lbl { position: absolute; left: 0; top: 0; width: 200px; height: 40px; display: block; }
          #f { position: absolute; left: 0; top: 80px; width: 240px; height: 40px; }
        </style>
      </head>
      <body>
        <label id="lbl" for="f">Drop here</label>
        <input id="f" type="file" name="up">
      </body>
    </html>
  "#;

  let options = RenderOptions::new().with_viewport(320, 160);
  let mut doc = BrowserDocument::new(support::deterministic_renderer(), html, options)?;
  let mut engine = InteractionEngine::new();

  // Ensure box/fragment trees exist for hit-testing.
  doc.render_frame_with_scroll_state_and_interaction_state(Some(engine.interaction_state()))?;

  let input_node_id =
    find_dom_node_id_by_html_id(doc.dom(), "f").expect("expected to find file input");
  assert!(
    find_dom_node_id_by_html_id(doc.dom(), "lbl").is_some(),
    "expected to find label"
  );

  // Create a deterministic temp file.
  let dir = tempdir().expect("temp dir");
  let file_path = dir.path().join("upload.txt");
  let file_bytes = b"label-drop-bytes".to_vec();
  std::fs::write(&file_path, &file_bytes).expect("write upload.txt");

  let scroll_state = doc.scroll_state();
  let drop_point_inside_label = Point::new(10.0, 10.0);

  let drop_changed = doc.mutate_dom_with_layout_artifacts(|dom, box_tree, fragment_tree| {
    let changed = engine.drop_files_with_scroll(
      dom,
      box_tree,
      fragment_tree,
      &scroll_state,
      drop_point_inside_label,
      &[file_path.clone()],
    );
    (changed, changed)
  })?;
  assert!(
    drop_changed,
    "expected drop to mutate state/DOM when dropping on label"
  );

  let files = engine
    .interaction_state()
    .form_state()
    .files_for(input_node_id)
    .expect("expected file input selection to be stored in form state");
  assert_eq!(
    files.len(),
    1,
    "expected single-file input to select exactly one file; got {}",
    files.len()
  );
  assert_eq!(files[0].filename, "upload.txt");
  assert_eq!(files[0].bytes, file_bytes);
  assert_eq!(
    files[0].path,
    std::fs::canonicalize(&file_path).expect("canonicalize upload.txt"),
    "expected stored file selection path to match the dropped file path"
  );

  // DOM value hint should be updated for browser-like semantics.
  assert_eq!(
    find_dom_attribute_by_html_id(doc.dom(), "f", "data-fastr-file-value").as_deref(),
    Some("C:\\fakepath\\upload.txt")
  );

  Ok(())
}
