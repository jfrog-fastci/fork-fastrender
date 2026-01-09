use crate::dom::HTML_NAMESPACE;

use super::{Document, NodeId, NodeKind};

fn is_html_namespace(namespace: &str) -> bool {
  namespace.is_empty() || namespace == HTML_NAMESPACE
}

fn is_void_html_element(tag_name: &str) -> bool {
  // https://html.spec.whatwg.org/multipage/syntax.html#void-elements
  tag_name.eq_ignore_ascii_case("area")
    || tag_name.eq_ignore_ascii_case("base")
    || tag_name.eq_ignore_ascii_case("br")
    || tag_name.eq_ignore_ascii_case("col")
    || tag_name.eq_ignore_ascii_case("embed")
    || tag_name.eq_ignore_ascii_case("hr")
    || tag_name.eq_ignore_ascii_case("img")
    || tag_name.eq_ignore_ascii_case("input")
    || tag_name.eq_ignore_ascii_case("link")
    || tag_name.eq_ignore_ascii_case("meta")
    || tag_name.eq_ignore_ascii_case("param")
    || tag_name.eq_ignore_ascii_case("source")
    || tag_name.eq_ignore_ascii_case("track")
    || tag_name.eq_ignore_ascii_case("wbr")
}

fn push_lowercase_ascii(out: &mut String, value: &str) {
  if value.bytes().any(|b| b.is_ascii_uppercase()) {
    out.push_str(&value.to_ascii_lowercase());
  } else {
    out.push_str(value);
  }
}

fn escape_text(out: &mut String, value: &str) {
  for ch in value.chars() {
    match ch {
      '&' => out.push_str("&amp;"),
      '<' => out.push_str("&lt;"),
      _ => out.push(ch),
    }
  }
}

fn escape_attr_value(out: &mut String, value: &str) {
  for ch in value.chars() {
    match ch {
      '&' => out.push_str("&amp;"),
      '"' => out.push_str("&quot;"),
      _ => out.push(ch),
    }
  }
}

fn serialize_element_start_tag(
  out: &mut String,
  tag_name: &str,
  namespace: &str,
  attributes: &[(String, String)],
) -> bool {
  let is_html = is_html_namespace(namespace);
  let is_void = is_html && is_void_html_element(tag_name);

  out.push('<');
  if is_html {
    push_lowercase_ascii(out, tag_name);
  } else {
    out.push_str(tag_name);
  }

  let mut attrs: Vec<&(String, String)> = attributes.iter().collect();
  attrs.sort_by(|(a_name, a_val), (b_name, b_val)| match a_name.cmp(b_name) {
    std::cmp::Ordering::Equal => a_val.cmp(b_val),
    other => other,
  });
  for (name, value) in attrs {
    out.push(' ');
    if is_html {
      push_lowercase_ascii(out, name);
    } else {
      out.push_str(name);
    }
    out.push_str("=\"");
    escape_attr_value(out, value);
    out.push('"');
  }

  out.push('>');
  is_void
}

fn serialize_element_end_tag(out: &mut String, tag_name: &str, namespace: &str) {
  let is_html = is_html_namespace(namespace);
  out.push_str("</");
  if is_html {
    push_lowercase_ascii(out, tag_name);
  } else {
    out.push_str(tag_name);
  }
  out.push('>');
}

pub(super) fn serialize_node(doc: &Document, root: NodeId, out: &mut String) {
  let mut stack: Vec<(NodeId, bool)> = vec![(root, false)];

  while let Some((id, exiting)) = stack.pop() {
    let Some(node) = doc.nodes.get(id.index()) else {
      continue;
    };

    match &node.kind {
      NodeKind::Text { content } => {
        if !exiting {
          escape_text(out, content);
        }
      }

      NodeKind::Element {
        tag_name,
        namespace,
        attributes,
      } => {
        let is_void = is_html_namespace(namespace) && is_void_html_element(tag_name);
        if !exiting {
          let wrote_void = serialize_element_start_tag(out, tag_name, namespace, attributes);
          if wrote_void {
            continue;
          }

          stack.push((id, true));
          for &child in node.children.iter().rev() {
            // Shadow roots are stored as element children for renderer traversal, but they are not
            // part of the light DOM tree and must not appear in Element.innerHTML/outerHTML output.
            if matches!(
              doc.nodes.get(child.index()).map(|n| &n.kind),
              Some(NodeKind::ShadowRoot { .. })
            ) {
              continue;
            }
            stack.push((child, false));
          }
        } else {
          if is_void {
            continue;
          }
          serialize_element_end_tag(out, tag_name, namespace);
        }
      }

      NodeKind::Slot {
        namespace,
        attributes,
        ..
      } => {
        // Serialize slots as elements.
        let is_void = is_html_namespace(namespace) && is_void_html_element("slot");
        if !exiting {
          let wrote_void = serialize_element_start_tag(out, "slot", namespace, attributes);
          if wrote_void {
            continue;
          }

          stack.push((id, true));
          for &child in node.children.iter().rev() {
            if matches!(
              doc.nodes.get(child.index()).map(|n| &n.kind),
              Some(NodeKind::ShadowRoot { .. })
            ) {
              continue;
            }
            stack.push((child, false));
          }
        } else {
          if is_void {
            continue;
          }
          serialize_element_end_tag(out, "slot", namespace);
        }
      }

      // Intentionally ignore unsupported node kinds (document, shadow root, etc.).
      _ => {}
    }
  }
}

pub(super) fn serialize_children(doc: &Document, parent: NodeId) -> String {
  let Some(node) = doc.nodes.get(parent.index()) else {
    return String::new();
  };

  let mut out = String::new();
  for &child in &node.children {
    // Skip ShadowRoot nodes stored for renderer traversal; they are not part of the light DOM.
    if matches!(
      doc.nodes.get(child.index()).map(|n| &n.kind),
      Some(NodeKind::ShadowRoot { .. })
    ) {
      continue;
    }
    serialize_node(doc, child, &mut out);
  }
  out
}

pub(super) fn serialize_outer(doc: &Document, node: NodeId) -> String {
  let mut out = String::new();
  serialize_node(doc, node, &mut out);
  out
}
