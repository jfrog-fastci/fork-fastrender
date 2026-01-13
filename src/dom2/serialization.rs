use super::{Attribute, Document, NodeId, NodeKind};

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

fn is_rawtext_element(doc: &Document, tag_name: &str, namespace: &str) -> bool {
  // Minimal raw text support: preserve JS/CSS text when serializing `<script>`/`<style>`.
  doc.is_html_case_insensitive_namespace(namespace)
    && (tag_name.eq_ignore_ascii_case("script") || tag_name.eq_ignore_ascii_case("style"))
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
      '>' => out.push_str("&gt;"),
      _ => out.push(ch),
    }
  }
}

fn escape_attr_value(out: &mut String, value: &str) {
  for ch in value.chars() {
    match ch {
      '&' => out.push_str("&amp;"),
      '"' => out.push_str("&quot;"),
      '<' => out.push_str("&lt;"),
      _ => out.push(ch),
    }
  }
}

fn serialize_start_tag(
  doc: &Document,
  out: &mut String,
  tag_name: &str,
  namespace: &str,
  attributes: &[Attribute],
) -> bool {
  let is_html = doc.is_html_case_insensitive_namespace(namespace);
  let is_void = is_html && is_void_html_element(tag_name);

  out.push('<');
  if is_html {
    push_lowercase_ascii(out, tag_name);
  } else {
    out.push_str(tag_name);
  }

  // Preserve stored attribute order for deterministic output.
  for attr in attributes {
    out.push(' ');
    let name = attr.qualified_name();
    if is_html {
      push_lowercase_ascii(out, name.as_ref());
    } else {
      out.push_str(name.as_ref());
    }
    out.push_str("=\"");
    escape_attr_value(out, &attr.value);
    out.push('"');
  }

  out.push('>');
  is_void
}

fn serialize_end_tag(doc: &Document, out: &mut String, tag_name: &str, namespace: &str) {
  let is_html = doc.is_html_case_insensitive_namespace(namespace);
  out.push_str("</");
  if is_html {
    push_lowercase_ascii(out, tag_name);
  } else {
    out.push_str(tag_name);
  }
  out.push('>');
}

enum Frame {
  Enter { node: NodeId, parent_rawtext: bool },
  ExitElement { node: NodeId },
}

fn serialize_nodes(
  doc: &Document,
  nodes: impl DoubleEndedIterator<Item = NodeId>,
  parent_rawtext: bool,
  out: &mut String,
) {
  let mut stack: Vec<Frame> = Vec::new();
  for node in nodes.rev() {
    stack.push(Frame::Enter {
      node,
      parent_rawtext,
    });
  }

  while let Some(frame) = stack.pop() {
    match frame {
      Frame::ExitElement { node } => {
        let Some(node) = doc.nodes.get(node.index()) else {
          continue;
        };
        match &node.kind {
          NodeKind::Element {
            tag_name,
            namespace,
            ..
          } => {
            if doc.is_html_case_insensitive_namespace(namespace) && is_void_html_element(tag_name) {
              continue;
            }
            serialize_end_tag(doc, out, tag_name, namespace);
          }
          NodeKind::Slot { namespace, .. } => {
            serialize_end_tag(doc, out, "slot", namespace);
          }
          _ => {}
        }
      }

      Frame::Enter {
        node: node_id,
        parent_rawtext,
      } => {
        let Some(node) = doc.nodes.get(node_id.index()) else {
          continue;
        };

        match &node.kind {
          NodeKind::Text { content } => {
            if parent_rawtext {
              out.push_str(content);
            } else {
              escape_text(out, content);
            }
          }

          NodeKind::Comment { content } => {
            // HTML serialization: comments are emitted verbatim (no escaping).
            out.push_str("<!--");
            out.push_str(content);
            out.push_str("-->");
          }

          NodeKind::Doctype {
            name,
            public_id,
            system_id,
          } => {
            out.push_str("<!DOCTYPE");
            if !name.is_empty() {
              out.push(' ');
              out.push_str(name);
            }
            if !public_id.is_empty() {
              out.push_str(" PUBLIC \"");
              out.push_str(public_id);
              out.push('"');
              if !system_id.is_empty() {
                out.push_str(" \"");
                out.push_str(system_id);
                out.push('"');
              }
            } else if !system_id.is_empty() {
              out.push_str(" SYSTEM \"");
              out.push_str(system_id);
              out.push('"');
            }
            out.push('>');
          }

          NodeKind::Element {
            tag_name,
            namespace,
            prefix: _,
            attributes,
          } => {
            let wrote_void = serialize_start_tag(doc, out, tag_name, namespace, attributes);
            if wrote_void {
              continue;
            }

            stack.push(Frame::ExitElement { node: node_id });

            let rawtext = is_rawtext_element(doc, tag_name, namespace);
            for &child in node.children.iter().rev() {
              let Some(child_node) = doc.nodes.get(child.index()) else {
                continue;
              };
              if child_node.parent != Some(node_id) {
                continue;
              }
              // Shadow roots are stored as element children for renderer traversal, but they are not
              // part of the light DOM tree and must not appear in `innerHTML`/`outerHTML` output.
              if matches!(child_node.kind, NodeKind::ShadowRoot { .. }) {
                continue;
              }
              stack.push(Frame::Enter {
                node: child,
                parent_rawtext: rawtext,
              });
            }
          }

          NodeKind::Slot {
            namespace,
            attributes,
            ..
          } => {
            let wrote_void = serialize_start_tag(doc, out, "slot", namespace, attributes);
            if wrote_void {
              continue;
            }

            stack.push(Frame::ExitElement { node: node_id });

            for &child in node.children.iter().rev() {
              let Some(child_node) = doc.nodes.get(child.index()) else {
                continue;
              };
              if child_node.parent != Some(node_id) {
                continue;
              }
              if matches!(child_node.kind, NodeKind::ShadowRoot { .. }) {
                continue;
              }
              stack.push(Frame::Enter {
                node: child,
                parent_rawtext: false,
              });
            }
          }

          NodeKind::Document { .. } | NodeKind::DocumentFragment | NodeKind::ShadowRoot { .. } => {
            // Serialize container-like nodes as fragments (their children, without wrapper markup).
            for &child in node.children.iter().rev() {
              let Some(child_node) = doc.nodes.get(child.index()) else {
                continue;
              };
              if child_node.parent != Some(node_id) {
                continue;
              }
              stack.push(Frame::Enter {
                node: child,
                parent_rawtext,
              });
            }
          }

          // Other node kinds are currently ignored by our HTML serialization.
          _ => {}
        }
      }
    }
  }
}

pub(super) fn serialize_children(doc: &Document, parent: NodeId) -> String {
  let Some(node) = doc.nodes.get(parent.index()) else {
    return String::new();
  };

  let rawtext = match &node.kind {
    NodeKind::Element {
      tag_name,
      namespace,
      ..
    } => is_rawtext_element(doc, tag_name, namespace),
    NodeKind::Slot { namespace, .. } => is_rawtext_element(doc, "slot", namespace),
    _ => false,
  };

  let mut out = String::new();
  let children = node.children.iter().copied().filter(|&child| {
    let Some(child_node) = doc.nodes.get(child.index()) else {
      return false;
    };
    if child_node.parent != Some(parent) {
      return false;
    }
    !matches!(child_node.kind, NodeKind::ShadowRoot { .. })
  });
  serialize_nodes(doc, children, rawtext, &mut out);
  out
}

pub(super) fn serialize_outer(doc: &Document, node: NodeId) -> String {
  let mut out = String::new();
  serialize_nodes(doc, std::iter::once(node), false, &mut out);
  out
}
