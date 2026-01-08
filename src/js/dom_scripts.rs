use crate::css::loader::resolve_href_with_base;
use crate::dom::{DomNode, DomNodeType, HTML_NAMESPACE};
use crate::html::document_base_url;

use super::{determine_script_type, ScriptElementSpec};

/// Extract `<script>` elements from a parsed DOM tree in document order.
///
/// This is a best-effort bridge between FastRender's HTML parser (`DomNode`) and the JavaScript
/// script scheduler. Traversal is iterative (non-recursive) and skips inert subtrees:
///
/// - `<template>` contents (`DomNode::traversal_children`)
/// - Shadow roots (`DomNodeType::ShadowRoot`) (conservative initial behavior)
pub fn extract_script_elements(dom: &DomNode, document_url: Option<&str>) -> Vec<ScriptElementSpec> {
  let base_url = document_base_url(dom, document_url);

  let mut out: Vec<ScriptElementSpec> = Vec::new();
  let mut stack: Vec<&DomNode> = Vec::new();
  stack.push(dom);

  while let Some(node) = stack.pop() {
    if let DomNodeType::Element {
      tag_name,
      namespace,
      ..
    } = &node.node_type
    {
      if tag_name.eq_ignore_ascii_case("script")
        && (namespace.is_empty() || namespace == HTML_NAMESPACE)
      {
        let async_attr = node.get_attribute_ref("async").is_some();
        let defer_attr = node.get_attribute_ref("defer").is_some();

        let src = node
          .get_attribute_ref("src")
          .and_then(|value| resolve_href_with_base(base_url.as_deref(), value));

        let mut inline_text = String::new();
        for child in &node.children {
          if let DomNodeType::Text { content } = &child.node_type {
            inline_text.push_str(content);
          }
        }

        out.push(ScriptElementSpec {
          base_url: base_url.clone(),
          src,
          inline_text,
          async_attr,
          defer_attr,
          parser_inserted: true,
          script_type: determine_script_type(node),
        });
      }
    }

    if matches!(node.node_type, DomNodeType::ShadowRoot { .. }) {
      continue;
    }

    // Push children in reverse so we traverse left-to-right in document order.
    for child in node.traversal_children().iter().rev() {
      stack.push(child);
    }
  }

  out
}

#[cfg(test)]
mod tests {
  use super::extract_script_elements;
  use crate::dom::parse_html;

  #[test]
  fn extracts_inline_script_text() {
    let dom = parse_html("<!doctype html><script>console.log(1)</script>").unwrap();
    let scripts = extract_script_elements(&dom, None);
    assert_eq!(scripts.len(), 1);
    assert_eq!(scripts[0].inline_text, "console.log(1)");
    assert!(scripts[0].src.is_none());
  }

  #[test]
  fn resolves_relative_src_against_base_href() {
    let dom = parse_html(
      r#"<!doctype html>
      <html>
        <head>
          <base href="https://example.com/base/">
        </head>
        <body>
          <script src="app.js"></script>
        </body>
      </html>"#,
    )
    .unwrap();

    let scripts = extract_script_elements(&dom, None);
    assert_eq!(scripts.len(), 1);
    assert_eq!(
      scripts[0].src.as_deref(),
      Some("https://example.com/base/app.js")
    );
    assert_eq!(scripts[0].base_url.as_deref(), Some("https://example.com/base/"));
  }

  #[test]
  fn resolves_relative_src_against_document_url() {
    let dom = parse_html(r#"<!doctype html><script src="app.js"></script>"#).unwrap();
    let scripts = extract_script_elements(&dom, Some("https://example.com/dir/page.html"));
    assert_eq!(scripts.len(), 1);
    assert_eq!(
      scripts[0].src.as_deref(),
      Some("https://example.com/dir/app.js")
    );
    assert_eq!(
      scripts[0].base_url.as_deref(),
      Some("https://example.com/dir/page.html")
    );
  }

  #[test]
  fn trims_src_using_ascii_whitespace() {
    let dom = parse_html(
      r#"<!doctype html>
      <html>
        <head>
          <base href="https://example.com/base/">
        </head>
        <body>
          <script src="  app.js  "></script>
        </body>
      </html>"#,
    )
    .unwrap();
    let scripts = extract_script_elements(&dom, None);
    assert_eq!(scripts.len(), 1);
    assert_eq!(scripts[0].src.as_deref(), Some("https://example.com/base/app.js"));
  }

  #[test]
  fn src_does_not_trim_non_ascii_whitespace() {
    let nbsp = "\u{00A0}";
    let html = format!(
      "<!doctype html><html><head><base href=\"https://example.com/base/\"></head>\
       <body><script src=\"app.js{nbsp}\"></script></body></html>"
    );
    let dom = parse_html(&html).unwrap();
    let scripts = extract_script_elements(&dom, None);
    assert_eq!(scripts.len(), 1);
    assert_eq!(
      scripts[0].src.as_deref(),
      Some("https://example.com/base/app.js%C2%A0")
    );
  }

  #[test]
  fn skips_template_and_shadow_root_scripts_and_preserves_order() {
    let dom = parse_html(
      r#"<!doctype html>
      <html>
        <body>
          <script>a</script>
          <template><script>ignored-template</script></template>
          <div id="host">
            <template shadowroot="open"><script>ignored-shadow</script></template>
            <script>b</script>
          </div>
          <script>c</script>
        </body>
      </html>"#,
    )
    .unwrap();

    let scripts = extract_script_elements(&dom, None);
    let texts: Vec<&str> = scripts.iter().map(|s| s.inline_text.as_str()).collect();
    assert_eq!(texts, vec!["a", "b", "c"]);
  }
}
