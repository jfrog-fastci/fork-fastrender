#![cfg(feature = "browser_ui")]

use super::support;
use fastrender::tree::box_tree::{BoxNode, BoxType, FormControlKind, ReplacedType};
use fastrender::ui::messages::{RepaintReason, TabId, UiToWorker};
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

fn find_file_control_value(node: &BoxNode) -> Option<Option<String>> {
  if let Some(control) = node.form_control.as_deref() {
    if let FormControlKind::File { value } = &control.control {
      return Some(value.clone());
    }
  }

  if let BoxType::Replaced(replaced) = &node.box_type {
    if let ReplacedType::FormControl(control) = &replaced.replaced_type {
      if let FormControlKind::File { value } = &control.control {
        return Some(value.clone());
      }
    }
  }

  if let Some(body) = node.footnote_body.as_deref() {
    if let Some(found) = find_file_control_value(body) {
      return Some(found);
    }
  }

  for child in &node.children {
    if let Some(found) = find_file_control_value(child) {
      return Some(found);
    }
  }

  None
}

#[test]
fn dropped_file_on_label_for_selects_associated_file_input() -> Result<()> {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
  let _lock = super::stage_listener_test_lock();
  let tab_id = TabId(1);
  let viewport_css = (320, 120);
  let url = "https://example.com/index.html";

  let dir = tempfile::tempdir()?;
  let file_path = dir.path().join("hello.txt");
  std::fs::write(&file_path, b"hello world")?;

  let html = r#"<!doctype html>
    <html>
      <head>
        <meta charset="utf-8">
        <style>
          html, body { margin: 0; padding: 0; }
          #lab { display: block; position: absolute; left: 0; top: 0; width: 280px; height: 32px; }
          #f { position: absolute; left: 0; top: 60px; width: 280px; height: 32px; }
        </style>
      </head>
      <body>
        <label id="lab" for="f">Drop here</label>
        <input id="f" type="file" required>
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

  // Drop over the label, not the input. The interaction engine should remap the drop to the
  // label's associated control (`for="f"`).
  let _ = controller.handle_message(UiToWorker::DropFiles {
    tab_id,
    pos_css: (10.0, 10.0),
    paths: vec![file_path.clone()],
  })?;

  let input = find_element_by_id(controller.document().dom(), "f");
  let stored = input
    .get_attribute_ref("data-fastr-file-value")
    .expect("expected file input to store selected file value");
  assert!(
    stored.contains("hello.txt"),
    "expected stored file value to contain filename, got {stored:?}"
  );
  assert!(
    !stored.is_empty(),
    "expected stored file value to be non-empty after file drop on label"
  );

  let prepared = controller
    .document()
    .prepared()
    .expect("expected controller to have a prepared document");
  let boxed_value =
    find_file_control_value(&prepared.box_tree().root).expect("expected file control in box tree");
  let boxed_value = boxed_value.expect("expected file control value to be present");
  assert!(
    boxed_value.contains("hello.txt"),
    "expected box generation to surface selected file name, got {boxed_value:?}"
  );

  Ok(())
}
