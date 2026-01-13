use fastrender::interaction::InteractionEngine;
use fastrender::tree::box_tree::{FormControlKind, ReplacedType};
use fastrender::ui::messages::{PointerButton, PointerModifiers};
use fastrender::{BoxType, BrowserDocument, Point, RenderOptions, Result};
use tempfile::tempdir;

use super::support;

fn find_file_input_box_id(box_tree: &fastrender::BoxTree) -> Option<usize> {
  let mut stack = vec![&box_tree.root];
  while let Some(node) = stack.pop() {
    if let BoxType::Replaced(replaced) = &node.box_type {
      if let ReplacedType::FormControl(control) = &replaced.replaced_type {
        if matches!(control.control, FormControlKind::File { .. }) {
          return Some(node.id);
        }
      }
    }

    if let Some(body) = node.footnote_body.as_deref() {
      stack.push(body);
    }
    for child in node.children.iter().rev() {
      stack.push(child);
    }
  }
  None
}

fn find_box_node<'a>(
  node: &'a fastrender::tree::box_tree::BoxNode,
  box_id: usize,
) -> Option<&'a fastrender::tree::box_tree::BoxNode> {
  if node.id == box_id {
    return Some(node);
  }
  if let Some(body) = node.footnote_body.as_deref() {
    if let Some(found) = find_box_node(body, box_id) {
      return Some(found);
    }
  }
  for child in &node.children {
    if let Some(found) = find_box_node(child, box_id) {
      return Some(found);
    }
  }
  None
}

#[test]
fn dropping_multiple_files_sets_summary_label_and_submits_all_files_multipart() -> Result<()> {
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
          #submit { position: absolute; left: 0; top: 60px; width: 120px; height: 40px; }
        </style>
      </head>
      <body>
        <form action="https://example.invalid/result" method="post" enctype="multipart/form-data">
          <input id="f" type="file" multiple name="up">
          <input id="submit" type="submit" value="Go">
        </form>
      </body>
    </html>
  "#;

  let options = RenderOptions::new().with_viewport(320, 160);
  let mut doc = BrowserDocument::new(support::deterministic_renderer(), html, options)?;
  let mut engine = InteractionEngine::new();

  doc.render_frame_with_scroll_state_and_interaction_state(Some(engine.interaction_state()))?;

  // Create two deterministic temp files so we can assert multipart bodies.
  let dir = tempdir().expect("temp dir");
  let file_a_path = dir.path().join("a.txt");
  let file_b_path = dir.path().join("b.txt");
  let bytes_a = b"hello-a-bytes".to_vec();
  let bytes_b = b"hello-b-bytes".to_vec();
  std::fs::write(&file_a_path, &bytes_a).expect("write a.txt");
  std::fs::write(&file_b_path, &bytes_b).expect("write b.txt");

  let scroll_state = doc.scroll_state();
  let drop_point = Point::new(10.0, 10.0);
  let _drop_changed = doc.mutate_dom_with_layout_artifacts(|dom, box_tree, fragment_tree| {
    let changed = engine.drop_files_with_scroll(
      dom,
      box_tree,
      fragment_tree,
      &scroll_state,
      drop_point,
      &[file_a_path.clone(), file_b_path.clone()],
    );
    (changed, changed)
  })?;

  // Re-render with the updated interaction state so box generation sees the file selection list.
  doc.render_frame_with_scroll_state_and_interaction_state(Some(engine.interaction_state()))?;
  let prepared = doc
    .prepared()
    .expect("expected BrowserDocument to have cached layout after render");
  let file_box_id = find_file_input_box_id(prepared.box_tree()).expect("expected file input");

  let file_node =
    find_box_node(&prepared.box_tree().root, file_box_id).expect("expected file box node");
  let BoxType::Replaced(replaced) = &file_node.box_type else {
    panic!("expected replaced box node for file input");
  };
  let ReplacedType::FormControl(control) = &replaced.replaced_type else {
    panic!("expected replaced form control for file input");
  };
  let FormControlKind::File { value } = &control.control else {
    panic!("expected FormControlKind::File for file input");
  };

  assert!(
    value.as_deref() == Some("2 files"),
    "expected file input to render summary label; value={value:?}"
  );

  // Click submit to trigger a multipart form submission.
  let submit_point = Point::new(10.0, 70.0);
  let action = doc.mutate_dom_with_layout_artifacts(|dom, box_tree, fragment_tree| {
    let down_changed = engine.pointer_down(dom, box_tree, fragment_tree, &scroll_state, submit_point);
    let (up_changed, action) = engine.pointer_up_with_scroll(
      dom,
      box_tree,
      fragment_tree,
      &scroll_state,
      submit_point,
      PointerButton::Primary,
      PointerModifiers::NONE,
      true,
      "https://example.invalid/page.html",
      "https://example.invalid/page.html",
    );
    (down_changed || up_changed, action)
  })?;

  let fastrender::interaction::InteractionAction::NavigateRequest { request } = action else {
    panic!("expected NavigateRequest, got {action:?}");
  };
  let body = request.body.expect("expected POST body");

  let body_str = String::from_utf8_lossy(&body);
  assert!(
    body_str.contains("multipart/form-data")
      || request
        .headers
        .iter()
        .any(|(name, value)| name.eq_ignore_ascii_case("content-type")
          && value.starts_with("multipart/form-data; boundary=")),
    "expected multipart Content-Type; headers={:?}",
    request.headers
  );
  assert!(
    body_str.contains("filename=\"a.txt\"") && body_str.contains("filename=\"b.txt\""),
    "expected multipart body to contain both filenames; body={body_str}"
  );
  assert!(
    body.windows(bytes_a.len()).any(|w| w == bytes_a.as_slice()),
    "expected multipart body to contain a.txt payload"
  );
  assert!(
    body.windows(bytes_b.len()).any(|w| w == bytes_b.as_slice()),
    "expected multipart body to contain b.txt payload"
  );

  Ok(())
}
