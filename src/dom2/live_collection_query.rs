use crate::dom::HTML_NAMESPACE;
use selectors::context::QuirksMode;

use super::{Document, Node, NodeId, NodeKind};

// DOM "ASCII whitespace" for tokenization:
// <https://infra.spec.whatwg.org/#ascii-whitespace>
// Note: This intentionally does *not* include U+000B VERTICAL TAB (which Rust treats as ASCII
// whitespace).
#[inline]
fn is_dom_ascii_whitespace(c: char) -> bool {
  matches!(c, '\u{0009}' | '\u{000A}' | '\u{000C}' | '\u{000D}' | '\u{0020}')
}

#[inline]
fn is_html_namespace(namespace: &str) -> bool {
  namespace.is_empty() || namespace == HTML_NAMESPACE
}

fn parse_ordered_set(input: &str) -> Vec<&str> {
  let mut out: Vec<&str> = Vec::new();
  for token in input.split(is_dom_ascii_whitespace) {
    if token.is_empty() {
      continue;
    }
    if out.iter().any(|existing| *existing == token) {
      continue;
    }
    out.push(token);
  }
  out
}

impl Document {
  fn collect_descendants_from<F>(&self, root: NodeId, mut matches: F) -> Vec<NodeId>
  where
    F: FnMut(NodeId, &Node) -> bool,
  {
    let Some(root_node) = self.nodes.get(root.index()) else {
      return Vec::new();
    };
    if root_node.inert_subtree {
      return Vec::new();
    }
    if matches!(&root_node.kind, NodeKind::ShadowRoot { .. }) {
      return Vec::new();
    }

    let mut results: Vec<NodeId> = Vec::new();
    let mut remaining = self.nodes.len() + 1;
    let mut stack: Vec<NodeId> = Vec::new();

    for &child in root_node.children.iter().rev() {
      let Some(child_node) = self.nodes.get(child.index()) else {
        continue;
      };
      if child_node.parent != Some(root) {
        continue;
      }
      if matches!(&child_node.kind, NodeKind::ShadowRoot { .. }) {
        continue;
      }
      stack.push(child);
    }

    while let Some(node_id) = stack.pop() {
      if remaining == 0 {
        break;
      }
      remaining -= 1;

      let Some(node) = self.nodes.get(node_id.index()) else {
        continue;
      };

      if matches(node_id, node) {
        results.push(node_id);
      }

      if node.inert_subtree {
        continue;
      }
      if matches!(&node.kind, NodeKind::ShadowRoot { .. }) {
        continue;
      }

      for &child in node.children.iter().rev() {
        let Some(child_node) = self.nodes.get(child.index()) else {
          continue;
        };
        if child_node.parent != Some(node_id) {
          continue;
        }
        if matches!(&child_node.kind, NodeKind::ShadowRoot { .. }) {
          continue;
        }
        stack.push(child);
      }
    }

    results
  }

  /// Live-collection backend for `getElementsByTagName`, producing a stable `Vec<NodeId>` snapshot.
  ///
  /// Traversal:
  /// - visits only descendants of `root` (not `root` itself),
  /// - skips inert `<template>` contents (`Node::inert_subtree`), and
  /// - does not traverse into `ShadowRoot` subtrees.
  pub fn get_elements_by_tag_name_from(&self, root: NodeId, qualified_name: &str) -> Vec<NodeId> {
    let match_all = qualified_name == "*";
    self.collect_descendants_from(root, |_, node| match &node.kind {
      NodeKind::Element {
        tag_name,
        namespace,
        ..
      } => {
        if match_all {
          return true;
        }
        if is_html_namespace(namespace) {
          tag_name.eq_ignore_ascii_case(qualified_name)
        } else {
          tag_name == qualified_name
        }
      }
      NodeKind::Slot { namespace, .. } => {
        if match_all {
          return true;
        }
        if is_html_namespace(namespace) {
          "slot".eq_ignore_ascii_case(qualified_name)
        } else {
          "slot" == qualified_name
        }
      }
      _ => false,
    })
  }

  /// Live-collection backend for `getElementsByTagNameNS`, producing a stable `Vec<NodeId>`
  /// snapshot.
  ///
  /// Traversal semantics match [`Document::get_elements_by_tag_name_from`].
  pub fn get_elements_by_tag_name_ns_from(
    &self,
    root: NodeId,
    namespace: Option<&str>,
    local_name: &str,
  ) -> Vec<NodeId> {
    let namespace_is_wildcard = namespace == Some("*");
    let local_is_wildcard = local_name == "*";

    self.collect_descendants_from(root, |_, node| {
      let (tag_name, node_ns) = match &node.kind {
        NodeKind::Element {
          tag_name, namespace, ..
        } => (tag_name.as_str(), namespace.as_str()),
        NodeKind::Slot { namespace, .. } => ("slot", namespace.as_str()),
        _ => return false,
      };

      let namespace_ok = if namespace_is_wildcard {
        true
      } else {
        let Some(query_ns) = namespace else {
          // `null` namespace (WebIDL `DOMString?`) does not match HTML namespace elements in our
          // DOM representation.
          return false;
        };

        if is_html_namespace(query_ns) {
          is_html_namespace(node_ns)
        } else {
          node_ns == query_ns
        }
      };

      if !namespace_ok {
        return false;
      }

      local_is_wildcard || tag_name == local_name
    })
  }

  /// Live-collection backend for `getElementsByClassName`, producing a stable `Vec<NodeId>`
  /// snapshot.
  pub fn get_elements_by_class_name_from(&self, root: NodeId, class_names: &str) -> Vec<NodeId> {
    let classes = parse_ordered_set(class_names);
    if classes.is_empty() {
      return Vec::new();
    }

    let quirks_mode = match &self.node(self.root()).kind {
      NodeKind::Document { quirks_mode } => *quirks_mode,
      _ => QuirksMode::NoQuirks,
    };
    let case_insensitive = quirks_mode == QuirksMode::Quirks;

    self.collect_descendants_from(root, |_, node| {
      let (namespace, attributes) = match &node.kind {
        NodeKind::Element {
          namespace,
          attributes,
          ..
        }
        | NodeKind::Slot {
          namespace,
          attributes,
          ..
        } => (namespace.as_str(), attributes.as_slice()),
        _ => return false,
      };

      let is_html = is_html_namespace(namespace);
      let Some(class_value) = attributes.iter().find_map(|(name, value)| {
        let name_ok = if is_html {
          name.eq_ignore_ascii_case("class")
        } else {
          name == "class"
        };
        name_ok.then_some(value.as_str())
      }) else {
        return false;
      };

      for &requested in &classes {
        let mut found = false;
        for token in class_value.split(is_dom_ascii_whitespace) {
          if token.is_empty() {
            continue;
          }
          if case_insensitive {
            if token.eq_ignore_ascii_case(requested) {
              found = true;
              break;
            }
          } else if token == requested {
            found = true;
            break;
          }
        }
        if !found {
          return false;
        }
      }

      true
    })
  }

  /// Live-collection backend for `getElementsByName`, producing a stable `Vec<NodeId>` snapshot.
  pub fn get_elements_by_name_from(&self, root: NodeId, name: &str) -> Vec<NodeId> {
    self.collect_descendants_from(root, |_, node| {
      let (namespace, attributes) = match &node.kind {
        NodeKind::Element {
          namespace,
          attributes,
          ..
        }
        | NodeKind::Slot {
          namespace,
          attributes,
          ..
        } => (namespace.as_str(), attributes.as_slice()),
        _ => return false,
      };

      let is_html = is_html_namespace(namespace);
      attributes.iter().any(|(attr_name, value)| {
        let name_ok = if is_html {
          attr_name.eq_ignore_ascii_case("name")
        } else {
          attr_name == "name"
        };
        name_ok && value == name
      })
    })
  }
}

