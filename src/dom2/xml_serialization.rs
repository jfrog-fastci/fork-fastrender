use crate::dom::HTML_NAMESPACE;
use crate::web::dom::DomException;

use super::{Document, NodeId, NodeKind, NULL_NAMESPACE};

/// Returns the namespace URI for a stored namespace string.
///
/// `dom2` normalizes HTML namespace elements to store `namespace=""`; for XML serialization we need
/// to treat that as the XHTML namespace.
fn namespace_uri_for_storage(namespace: &str) -> Option<&str> {
  if namespace == NULL_NAMESPACE {
    None
  } else if namespace.is_empty() {
    Some(HTML_NAMESPACE)
  } else {
    Some(namespace)
  }
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

fn invalid_state() -> DomException {
  DomException::invalid_state_error("Failed to serialize node")
}

fn validate_comment_data(data: &str) -> Result<(), DomException> {
  // XML: Comment contents must not contain "--" and must not end in "-".
  // https://www.w3.org/TR/xml/#sec-comments
  if data.contains("--") || data.ends_with('-') {
    return Err(invalid_state());
  }
  Ok(())
}

fn validate_processing_instruction(target: &str, data: &str) -> Result<(), DomException> {
  // Minimal validation: the data must not contain "?>".
  // https://www.w3.org/TR/xml/#sec-pi
  let _ = target;
  if data.contains("?>") {
    return Err(invalid_state());
  }
  Ok(())
}

enum Frame<'a> {
  Enter {
    node: NodeId,
    inherited_ns: Option<&'a str>,
  },
  ExitElement {
    node: NodeId,
  },
}

fn is_serializable_child(doc: &Document, parent: NodeId, child: NodeId) -> bool {
  let Some(child_node) = doc.nodes.get(child.index()) else {
    return false;
  };
  if child_node.parent != Some(parent) {
    return false;
  }
  // Shadow roots are stored as element children for renderer traversal, but they are not part of
  // the light DOM tree and must not appear in serialization output.
  !matches!(child_node.kind, NodeKind::ShadowRoot { .. })
}

fn serialize_node(doc: &Document, node: NodeId) -> Result<String, DomException> {
  let mut out = String::new();
  let mut stack: Vec<Frame<'_>> = Vec::new();
  stack.push(Frame::Enter {
    node,
    inherited_ns: None,
  });

  while let Some(frame) = stack.pop() {
    match frame {
      Frame::ExitElement { node: node_id } => {
        let Some(node) = doc.nodes.get(node_id.index()) else {
          continue;
        };

        match &node.kind {
          NodeKind::Element {
            tag_name,
            namespace,
            prefix,
            ..
          } => {
            let is_html = doc.is_html_case_insensitive_namespace(namespace);
            out.push_str("</");
            if let Some(prefix) = prefix.as_deref() {
              if is_html {
                push_lowercase_ascii(&mut out, prefix);
              } else {
                out.push_str(prefix);
              }
              out.push(':');
            }
            if is_html {
              push_lowercase_ascii(&mut out, tag_name);
            } else {
              out.push_str(tag_name);
            }
            out.push('>');
          }
          NodeKind::Slot { namespace, .. } => {
            let is_html = doc.is_html_case_insensitive_namespace(namespace);
            out.push_str("</");
            if is_html {
              out.push_str("slot");
            } else {
              out.push_str("slot");
            }
            out.push('>');
          }
          _ => {}
        }
      }

      Frame::Enter {
        node: node_id,
        inherited_ns,
      } => {
        let Some(node) = doc.nodes.get(node_id.index()) else {
          continue;
        };

        match &node.kind {
          NodeKind::Text { content } => {
            escape_text(&mut out, content);
          }

          NodeKind::Comment { content } => {
            validate_comment_data(content)?;
            out.push_str("<!--");
            out.push_str(content);
            out.push_str("-->");
          }

          NodeKind::ProcessingInstruction { target, data } => {
            validate_processing_instruction(target, data)?;
            out.push_str("<?");
            out.push_str(target);
            if !data.is_empty() {
              out.push(' ');
              out.push_str(data);
            }
            out.push_str("?>");
          }

          NodeKind::Doctype {
            name,
            public_id,
            system_id,
          } => {
            // Minimal doctype serialization sufficient for debugging; HTML parser does not currently
            // produce doctype nodes in dom2.
            out.push_str("<!DOCTYPE ");
            out.push_str(name);
            if !public_id.is_empty() {
              out.push_str(" PUBLIC \"");
              escape_attr_value(&mut out, public_id);
              out.push('"');
              if !system_id.is_empty() {
                out.push(' ');
                out.push('"');
                escape_attr_value(&mut out, system_id);
                out.push('"');
              }
            } else if !system_id.is_empty() {
              out.push_str(" SYSTEM \"");
              escape_attr_value(&mut out, system_id);
              out.push('"');
            }
            out.push('>');
          }

          NodeKind::Element {
            tag_name,
            namespace,
            prefix,
            attributes,
            ..
          } => {
            let is_html = doc.is_html_case_insensitive_namespace(namespace);
            let ns_uri = namespace_uri_for_storage(namespace.as_str());
            let expected_xmlns_value = ns_uri.unwrap_or("");
            let prefix_str = prefix.as_deref();

            // Validate any explicit `xmlns` attribute matches the element's actual namespace.
            let explicit_xmlns = attributes.iter().find_map(|attr| {
              let name = attr.qualified_name();
              if is_html {
                name.eq_ignore_ascii_case("xmlns").then_some(attr.value.as_str())
              } else {
                (name.as_ref() == "xmlns").then_some(attr.value.as_str())
              }
            });
            if let Some(explicit) = explicit_xmlns {
              if explicit != expected_xmlns_value {
                return Err(invalid_state());
              }
            }

            // Validate any explicit `xmlns:prefix` matches the element's namespace.
            let prefix_xmlns_name = prefix_str.map(|p| format!("xmlns:{p}"));
            let explicit_prefix_xmlns = prefix_xmlns_name.as_deref().and_then(|expected| {
              attributes.iter().find_map(|attr| {
                let name = attr.qualified_name();
                if is_html {
                  name.eq_ignore_ascii_case(expected).then_some(attr.value.as_str())
                } else {
                  (name.as_ref() == expected).then_some(attr.value.as_str())
                }
              })
            });
            if let Some(explicit) = explicit_prefix_xmlns {
              let ns_uri_for_prefix = ns_uri.ok_or_else(invalid_state)?;
              if explicit != ns_uri_for_prefix {
                return Err(invalid_state());
              }
            }

            let needs_xmlns = inherited_ns != ns_uri;
            let inject_xmlns = needs_xmlns && explicit_xmlns.is_none();
            let inject_prefix_xmlns = prefix_str.is_some() && explicit_prefix_xmlns.is_none();

            // Determine whether we will serialize any children before deciding whether to emit
            // `<tag/>` or `<tag>..</tag>`.
            let mut has_child = false;
            for &child in node.children.iter() {
              if is_serializable_child(doc, node_id, child) {
                has_child = true;
                break;
              }
            }

            out.push('<');
            if let Some(prefix) = prefix_str {
              if is_html {
                push_lowercase_ascii(&mut out, prefix);
              } else {
                out.push_str(prefix);
              }
              out.push(':');
            }
            if is_html {
              push_lowercase_ascii(&mut out, tag_name);
            } else {
              out.push_str(tag_name);
            }

            if inject_xmlns {
              out.push_str(" xmlns=\"");
              escape_attr_value(&mut out, expected_xmlns_value);
              out.push('"');
            }
            if inject_prefix_xmlns {
              let Some(prefix) = prefix_str else {
                return Err(invalid_state());
              };
              let ns_uri_for_prefix = ns_uri.ok_or_else(invalid_state)?;
              out.push_str(" xmlns:");
              if is_html {
                push_lowercase_ascii(&mut out, prefix);
              } else {
                out.push_str(prefix);
              }
              out.push_str("=\"");
              escape_attr_value(&mut out, ns_uri_for_prefix);
              out.push('"');
            }

            // Preserve stored attribute order for deterministic output.
            for attr in attributes {
              let name = attr.qualified_name();
              out.push(' ');
              if is_html {
                push_lowercase_ascii(&mut out, name.as_ref());
              } else {
                out.push_str(name.as_ref());
              }
              out.push_str("=\"");
              escape_attr_value(&mut out, &attr.value);
              out.push('"');
            }

            if !has_child {
              out.push_str("/>");
              continue;
            }

            out.push('>');
            stack.push(Frame::ExitElement { node: node_id });

            for &child in node.children.iter().rev() {
              if !is_serializable_child(doc, node_id, child) {
                continue;
              }
              stack.push(Frame::Enter {
                node: child,
                inherited_ns: ns_uri,
              });
            }
          }

          NodeKind::Slot {
            namespace,
            attributes,
            ..
          } => {
            let is_html = doc.is_html_case_insensitive_namespace(namespace);
            let ns_uri = namespace_uri_for_storage(namespace.as_str());
            let expected_xmlns_value = ns_uri.unwrap_or("");

            let explicit_xmlns = attributes.iter().find_map(|attr| {
              let name = attr.qualified_name();
              if is_html {
                name.eq_ignore_ascii_case("xmlns").then_some(attr.value.as_str())
              } else {
                (name.as_ref() == "xmlns").then_some(attr.value.as_str())
              }
            });
            if let Some(explicit) = explicit_xmlns {
              if explicit != expected_xmlns_value {
                return Err(invalid_state());
              }
            }

            let needs_xmlns = inherited_ns != ns_uri;
            let inject_xmlns = needs_xmlns && explicit_xmlns.is_none();

            let mut has_child = false;
            for &child in node.children.iter() {
              if is_serializable_child(doc, node_id, child) {
                has_child = true;
                break;
              }
            }

            out.push_str("<slot");
            if inject_xmlns {
              out.push_str(" xmlns=\"");
              escape_attr_value(&mut out, expected_xmlns_value);
              out.push('"');
            }

            for attr in attributes {
              let name = attr.qualified_name();
              out.push(' ');
              if is_html {
                push_lowercase_ascii(&mut out, name.as_ref());
              } else {
                out.push_str(name.as_ref());
              }
              out.push_str("=\"");
              escape_attr_value(&mut out, &attr.value);
              out.push('"');
            }

            if !has_child {
              out.push_str("/>");
              continue;
            }

            out.push('>');
            stack.push(Frame::ExitElement { node: node_id });

            for &child in node.children.iter().rev() {
              if !is_serializable_child(doc, node_id, child) {
                continue;
              }
              stack.push(Frame::Enter {
                node: child,
                inherited_ns: ns_uri,
              });
            }
          }

          NodeKind::Document { .. } | NodeKind::DocumentFragment | NodeKind::ShadowRoot { .. } => {
            for &child in node.children.iter().rev() {
              if !is_serializable_child(doc, node_id, child) {
                continue;
              }
              stack.push(Frame::Enter { node: child, inherited_ns });
            }
          }
        }
      }
    }
  }

  Ok(out)
}

impl Document {
  /// Serialize a node (and its descendants) using DOM's XML serialization algorithm.
  ///
  /// This is the backing implementation for `XMLSerializer.prototype.serializeToString`.
  pub fn xml_serialize(&self, node: NodeId) -> Result<String, DomException> {
    if node.index() >= self.nodes.len() {
      return Err(invalid_state());
    }
    serialize_node(self, node)
  }
}
