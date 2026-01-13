use fastrender::dom::{DomNode, DomNodeType};
use fastrender::ui::chrome_frame::dom_mutation::{
  set_text_by_element_id, toggle_class_by_element_id,
};
use fastrender::{BrowserDocument, RenderOptions, Result};

fn serialize_dom_subtree(node: &DomNode) -> String {
  match &node.node_type {
    DomNodeType::Text { content } => escape_text(content),
    DomNodeType::ShadowRoot { .. } | DomNodeType::Document { .. } => {
      let mut out = String::new();
      for child in node.children.iter() {
        out.push_str(&serialize_dom_subtree(child));
      }
      out
    }
    DomNodeType::Slot { attributes, .. } => {
      let mut out = String::new();
      out.push_str("<slot");
      for (name, value) in attributes {
        out.push(' ');
        out.push_str(name);
        out.push('=');
        out.push('"');
        out.push_str(&escape_attr(value));
        out.push('"');
      }
      out.push('>');
      for child in node.children.iter() {
        out.push_str(&serialize_dom_subtree(child));
      }
      out.push_str("</slot>");
      out
    }
    DomNodeType::Element {
      tag_name,
      attributes,
      ..
    } => {
      let mut out = String::new();
      out.push('<');
      out.push_str(tag_name);
      for (name, value) in attributes {
        out.push(' ');
        out.push_str(name);
        out.push('=');
        out.push('"');
        out.push_str(&escape_attr(value));
        out.push('"');
      }
      out.push('>');
      for child in node.children.iter() {
        out.push_str(&serialize_dom_subtree(child));
      }
      out.push_str("</");
      out.push_str(tag_name);
      out.push('>');
      out
    }
  }
}

fn escape_text(value: &str) -> String {
  value.replace('&', "&amp;").replace('<', "&lt;")
}

fn escape_attr(value: &str) -> String {
  value
    .replace('&', "&amp;")
    .replace('<', "&lt;")
    .replace('"', "&quot;")
}

fn find_by_id<'a>(root: &'a DomNode, html_id: &str) -> Option<&'a DomNode> {
  let mut stack = vec![root];
  while let Some(node) = stack.pop() {
    if node.get_attribute_ref("id") == Some(html_id) {
      return Some(node);
    }
    for child in node.children.iter().rev() {
      stack.push(child);
    }
  }
  None
}

#[test]
fn set_text_by_element_id_updates_text_and_serializes_escaped() -> Result<()> {
  let html = r#"<!doctype html><html><body><div id="target">old</div></body></html>"#;
  let mut document =
    BrowserDocument::from_html(html, RenderOptions::default().with_viewport(64, 64))?;

  let changed = document.mutate_dom(|dom| set_text_by_element_id(dom, "target", "<x>"));
  assert!(changed, "expected mutation to report changes");

  let serialized = serialize_dom_subtree(document.dom());
  assert!(
    serialized.contains("&lt;x>"),
    "expected serialized DOM to escape '<' in '<x>', got: {serialized}"
  );
  assert!(
    !serialized.contains("<x>"),
    "expected serialized DOM to not contain raw '<x>' tag, got: {serialized}"
  );
  Ok(())
}

#[test]
fn toggle_class_by_element_id_is_idempotent() -> Result<()> {
  let html = r#"<!doctype html><html><body><div id="target" class="a"></div></body></html>"#;
  let mut document =
    BrowserDocument::from_html(html, RenderOptions::default().with_viewport(64, 64))?;

  let changed = document.mutate_dom(|dom| toggle_class_by_element_id(dom, "target", "loading", true));
  assert!(changed, "expected first enable to mutate");

  let changed = document.mutate_dom(|dom| toggle_class_by_element_id(dom, "target", "loading", true));
  assert!(!changed, "expected second enable to be a no-op");

  let class_attr = find_by_id(document.dom(), "target")
    .and_then(|node| node.get_attribute_ref("class"))
    .unwrap_or("");
  let tokens: Vec<&str> = class_attr.split_ascii_whitespace().collect();
  assert_eq!(tokens.iter().filter(|c| **c == "loading").count(), 1);

  let changed = document.mutate_dom(|dom| toggle_class_by_element_id(dom, "target", "loading", false));
  assert!(changed, "expected disable to mutate");

  let changed = document.mutate_dom(|dom| toggle_class_by_element_id(dom, "target", "loading", false));
  assert!(!changed, "expected second disable to be a no-op");

  let class_attr = find_by_id(document.dom(), "target")
    .and_then(|node| node.get_attribute_ref("class"))
    .unwrap_or("");
  assert_eq!(class_attr, "a");
  Ok(())
}
