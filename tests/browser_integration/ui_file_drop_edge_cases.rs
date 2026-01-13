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
fn dropping_multiple_files_on_single_file_input_selects_only_first() -> Result<()> {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
  let _lock = super::stage_listener_test_lock();
  let tab_id = TabId(1);
  let viewport_css = (320, 120);
  let url = "https://example.com/index.html";

  let dir = tempfile::tempdir()?;
  let a_path = dir.path().join("a.txt");
  let b_path = dir.path().join("b.txt");
  std::fs::write(&a_path, b"a")?;
  std::fs::write(&b_path, b"b")?;

  let html = r#"<!doctype html>
    <html>
      <head>
        <meta charset="utf-8">
        <style>
          html, body { margin: 0; padding: 0; }
          #f { position: absolute; left: 0; top: 0; width: 280px; height: 32px; }
        </style>
      </head>
      <body>
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

  let _ = controller.handle_message(UiToWorker::DropFiles {
    tab_id,
    pos_css: (10.0, 10.0),
    paths: vec![a_path.clone(), b_path.clone()],
  })?;

  let input = find_element_by_id(controller.document().dom(), "f");
  let stored = input
    .get_attribute_ref("data-fastr-file-value")
    .expect("expected file input to store selected file value");
  assert!(
    stored.contains("a.txt"),
    "expected stored file value to contain first filename, got {stored:?}"
  );
  assert!(
    !stored.contains("b.txt"),
    "expected stored file value to ignore second filename, got {stored:?}"
  );

  let prepared = controller
    .document()
    .prepared()
    .expect("expected controller to have a prepared document");
  let boxed_value =
    find_file_control_value(&prepared.box_tree().root).expect("expected file control in box tree");
  let boxed_value = boxed_value.expect("expected file control value to be present");
  assert!(
    boxed_value.contains("a.txt"),
    "expected box generation to surface only first file name, got {boxed_value:?}"
  );
  assert!(
    !boxed_value.contains("b.txt"),
    "expected box generation to ignore second file name, got {boxed_value:?}"
  );

  Ok(())
}

#[test]
fn dropping_file_on_disabled_file_input_is_ignored() -> Result<()> {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
  let _lock = super::stage_listener_test_lock();
  let tab_id = TabId(2);
  let viewport_css = (320, 120);
  let url = "https://example.com/index.html";

  let dir = tempfile::tempdir()?;
  let a_path = dir.path().join("a.txt");
  std::fs::write(&a_path, b"a")?;

  let html = r#"<!doctype html>
    <html>
      <head>
        <meta charset="utf-8">
        <style>
          html, body { margin: 0; padding: 0; }
          #f { position: absolute; left: 0; top: 0; width: 280px; height: 32px; }
        </style>
      </head>
      <body>
        <input id="f" type="file" disabled>
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

  let _ = controller.handle_message(UiToWorker::DropFiles {
    tab_id,
    pos_css: (10.0, 10.0),
    paths: vec![a_path.clone()],
  })?;

  let input = find_element_by_id(controller.document().dom(), "f");
  let stored = input.get_attribute_ref("data-fastr-file-value");
  assert!(
    stored.is_none() || stored.is_some_and(|s| s.is_empty()),
    "expected disabled file input to ignore drop, got stored={stored:?}"
  );

  let prepared = controller
    .document()
    .prepared()
    .expect("expected controller to have a prepared document");
  let boxed_value =
    find_file_control_value(&prepared.box_tree().root).expect("expected file control in box tree");
  assert!(
    boxed_value.is_none() || boxed_value.as_deref().is_some_and(|s| s.is_empty()),
    "expected disabled file input to have no selection label, got {boxed_value:?}"
  );

  Ok(())
}

