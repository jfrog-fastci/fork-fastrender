use fastrender::accessibility::AccessibilityNode;
use fastrender::api::{FastRender, RenderOptions};
use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};

pub fn render_accessibility_tree(html: &str) -> AccessibilityNode {
  let mut renderer = FastRender::new().expect("renderer");
  let dom = renderer.parse_html(html).expect("parse html");
  renderer
    .accessibility_tree(&dom, 800, 600)
    .expect("accessibility tree")
}

pub fn render_accessibility_json(html: &str) -> Value {
  let mut renderer = FastRender::new().expect("renderer");
  let dom = renderer.parse_html(html).expect("parse");
  let json = renderer
    .accessibility_tree_json(&dom, 800, 600)
    .expect("accessibility tree json");
  serde_json::from_str(&json).expect("parse json")
}

pub fn render_accessibility_json_with_options(html: &str, options: RenderOptions) -> Value {
  let mut renderer = FastRender::new().expect("renderer");
  renderer
    .accessibility_tree_html_json(html, options)
    .expect("accessibility tree json")
}

pub fn find_json_node<'a>(node: &'a Value, id: &str) -> Option<&'a Value> {
  if node
    .get("id")
    .and_then(|v| v.as_str())
    .is_some_and(|v| v == id)
  {
    return Some(node);
  }

  if let Some(children) = node.get("children").and_then(|c| c.as_array()) {
    for child in children {
      if let Some(found) = find_json_node(child, id) {
        return Some(found);
      }
    }
  }

  None
}

pub fn find_by_id<'a>(node: &'a AccessibilityNode, id: &str) -> Option<&'a AccessibilityNode> {
  if node.id.as_deref() == Some(id) {
    return Some(node);
  }
  for child in node.children.iter() {
    if let Some(found) = find_by_id(child, id) {
      return Some(found);
    }
  }
  None
}

pub fn count_by_id(node: &AccessibilityNode, id: &str) -> usize {
  let mut count = 0usize;
  if node.id.as_deref() == Some(id) {
    count += 1;
  }
  for child in node.children.iter() {
    count += count_by_id(child, id);
  }
  count
}

pub fn find_path<'a>(node: &'a AccessibilityNode, id: &str) -> Option<Vec<&'a AccessibilityNode>> {
  if node.id.as_deref() == Some(id) {
    return Some(vec![node]);
  }
  for child in node.children.iter() {
    if let Some(mut path) = find_path(child, id) {
      path.insert(0, node);
      return Some(path);
    }
  }
  None
}

pub fn accessibility_fixture_dir() -> PathBuf {
  PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/accessibility")
}

pub fn accessibility_fixture_path(path: impl AsRef<Path>) -> PathBuf {
  accessibility_fixture_dir().join(path)
}

pub fn read_accessibility_fixture(file_name: impl AsRef<Path>) -> String {
  let path = accessibility_fixture_path(file_name);
  fs::read_to_string(&path).unwrap_or_else(|err| panic!("read fixture {}: {err}", path.display()))
}

pub fn read_accessibility_fixture_html(name: &str) -> String {
  read_accessibility_fixture(format!("{name}.html"))
}

pub fn read_accessibility_fixture_json(name: &str) -> Value {
  let data = read_accessibility_fixture(format!("{name}.json"));
  serde_json::from_str(&data).expect("parse fixture json")
}
