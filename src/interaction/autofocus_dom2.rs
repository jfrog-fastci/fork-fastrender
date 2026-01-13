use crate::dom2::{Document, NodeId, NodeKind};

fn trim_ascii_whitespace(value: &str) -> &str {
  // HTML attribute processing (e.g. tabindex) trims ASCII whitespace only.
  value.trim_matches(|c: char| matches!(c, '\u{0009}' | '\u{000A}' | '\u{000C}' | '\u{000D}' | ' '))
}

fn is_html_namespace(doc: &Document, namespace: &str) -> bool {
  doc.is_html_case_insensitive_namespace(namespace)
}

fn node_namespace<'a>(node: &'a crate::dom2::Node) -> Option<&'a str> {
  match &node.kind {
    NodeKind::Element { namespace, .. } | NodeKind::Slot { namespace, .. } => Some(namespace.as_str()),
    _ => None,
  }
}

fn node_tag_name<'a>(node: &'a crate::dom2::Node) -> Option<&'a str> {
  match &node.kind {
    NodeKind::Element { tag_name, .. } => Some(tag_name.as_str()),
    _ => None,
  }
}

fn node_is_element_like(node: &crate::dom2::Node) -> bool {
  matches!(node.kind, NodeKind::Element { .. } | NodeKind::Slot { .. })
}

fn node_has_attr(doc: &Document, node: &crate::dom2::Node, name: &str) -> bool {
  let Some(namespace) = node_namespace(node) else {
    return false;
  };
  let is_html = is_html_namespace(doc, namespace);
  let attrs = match &node.kind {
    NodeKind::Element { attributes, .. } | NodeKind::Slot { attributes, .. } => attributes.as_slice(),
    _ => return false,
  };
  attrs
    .iter()
    .any(|(k, _)| if is_html { k.eq_ignore_ascii_case(name) } else { k == name })
}

fn node_get_attr<'a>(doc: &Document, node: &'a crate::dom2::Node, name: &str) -> Option<&'a str> {
  let Some(namespace) = node_namespace(node) else {
    return None;
  };
  let is_html = is_html_namespace(doc, namespace);
  let attrs = match &node.kind {
    NodeKind::Element { attributes, .. } | NodeKind::Slot { attributes, .. } => attributes.as_slice(),
    _ => return None,
  };
  attrs
    .iter()
    .find(|(k, _)| if is_html { k.eq_ignore_ascii_case(name) } else { k == name })
    .map(|(_, v)| v.as_str())
}

fn parse_tabindex(doc: &Document, node: &crate::dom2::Node) -> Option<i32> {
  let raw = node_get_attr(doc, node, "tabindex")?;
  let raw = trim_ascii_whitespace(raw);
  if raw.is_empty() {
    return None;
  }
  raw.parse::<i32>().ok()
}

fn is_anchor_with_href(doc: &Document, node: &crate::dom2::Node) -> bool {
  node_tag_name(node).is_some_and(|tag| {
    (tag.eq_ignore_ascii_case("a") || tag.eq_ignore_ascii_case("area"))
      && node_get_attr(doc, node, "href").is_some_and(|href| {
        let href = trim_ascii_whitespace(href);
        !href.is_empty()
          && !href
            .as_bytes()
            .get(.."javascript:".len())
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case(b"javascript:"))
      })
  })
}

fn input_type<'a>(doc: &Document, node: &'a crate::dom2::Node) -> &'a str {
  node_get_attr(doc, node, "type")
    .map(trim_ascii_whitespace)
    .filter(|v| !v.is_empty())
    .unwrap_or("text")
}

fn is_potentially_focusable_element_for_autofocus(doc: &Document, node: &crate::dom2::Node) -> bool {
  if !node_is_element_like(node) {
    return false;
  }

  // `input type=hidden` is never focusable, even when tabindex is set.
  if node_tag_name(node)
    .is_some_and(|tag| tag.eq_ignore_ascii_case("input"))
    && input_type(doc, node).eq_ignore_ascii_case("hidden")
  {
    return false;
  }

  // `tabindex` makes any element focusable, even if it is not reachable via Tab (negative values).
  if parse_tabindex(doc, node).is_some() {
    return true;
  }

  if is_anchor_with_href(doc, node) {
    return true;
  }

  node_tag_name(node).is_some_and(|tag| {
    tag.eq_ignore_ascii_case("input")
      || tag.eq_ignore_ascii_case("textarea")
      || tag.eq_ignore_ascii_case("select")
      || tag.eq_ignore_ascii_case("button")
  })
}

fn node_self_is_inert(doc: &Document, node: &crate::dom2::Node) -> bool {
  // `<template>` inert subtree marker: treat the template element itself as inert for autofocus, to
  // mirror the renderer/`DomNode` autofocus behaviour.
  if node.inert_subtree {
    return true;
  }
  if node_has_attr(doc, node, "inert") {
    return true;
  }
  node_get_attr(doc, node, "data-fastr-inert").is_some_and(|v| v.eq_ignore_ascii_case("true"))
}

fn node_self_is_hidden(doc: &Document, node: &crate::dom2::Node) -> bool {
  if node_has_attr(doc, node, "hidden") {
    return true;
  }
  node_get_attr(doc, node, "data-fastr-hidden").is_some_and(|v| v.eq_ignore_ascii_case("true"))
}

fn is_html_fieldset(doc: &Document, node: &crate::dom2::Node) -> bool {
  match &node.kind {
    NodeKind::Element {
      tag_name,
      namespace,
      ..
    } => is_html_namespace(doc, namespace) && tag_name.eq_ignore_ascii_case("fieldset"),
    _ => false,
  }
}

fn is_html_legend(doc: &Document, node: &crate::dom2::Node) -> bool {
  match &node.kind {
    NodeKind::Element {
      tag_name,
      namespace,
      ..
    } => is_html_namespace(doc, namespace) && tag_name.eq_ignore_ascii_case("legend"),
    _ => false,
  }
}

fn is_fieldset_disabled_candidate(doc: &Document, node: &crate::dom2::Node) -> bool {
  // HTML: fieldset disabledness only affects "form-associated elements" inside the fieldset. For
  // autofocus selection we only need to support the core form controls we currently implement.
  match &node.kind {
    NodeKind::Element {
      tag_name,
      namespace,
      ..
    } => {
      is_html_namespace(doc, namespace)
        && (tag_name.eq_ignore_ascii_case("input")
          || tag_name.eq_ignore_ascii_case("select")
          || tag_name.eq_ignore_ascii_case("textarea")
          || tag_name.eq_ignore_ascii_case("button"))
    }
    _ => false,
  }
}

fn fieldset_first_legend_child(doc: &Document, fieldset_id: NodeId) -> Option<NodeId> {
  let fieldset = doc.nodes().get(fieldset_id.index())?;
  debug_assert!(is_html_fieldset(doc, fieldset));
  for &child in &fieldset.children {
    let Some(child_node) = doc.nodes().get(child.index()) else {
      continue;
    };
    if child_node.parent != Some(fieldset_id) {
      continue;
    }
    if is_html_legend(doc, child_node) {
      return Some(child);
    }
  }
  None
}

fn is_effectively_disabled(doc: &Document, node_id: NodeId) -> bool {
  let Some(node) = doc.nodes().get(node_id.index()) else {
    return false;
  };
  if !node_is_element_like(node) {
    return false;
  }

  // `disabled` is a boolean attribute on form controls, but authors also use it on custom controls;
  // for interaction treat it as authoritative on the element itself.
  if node_has_attr(doc, node, "disabled") {
    return true;
  }

  if !is_fieldset_disabled_candidate(doc, node) {
    return false;
  }

  // Walk ancestors; `<fieldset disabled>` is the only ancestor-based disabledness we model (with the
  // first-legend exception).
  let mut ancestors: Vec<NodeId> = Vec::new();
  let mut remaining = doc.nodes_len().saturating_add(1);
  let mut current = Some(node_id);
  while let Some(id) = current {
    if remaining == 0 {
      break;
    }
    remaining -= 1;

    let Some(current_node) = doc.nodes().get(id.index()) else {
      break;
    };
    ancestors.push(id);

    if is_html_fieldset(doc, current_node) && node_has_attr(doc, current_node, "disabled") {
      match fieldset_first_legend_child(doc, id) {
        Some(first_legend_id) => {
          let in_first_legend = ancestors.iter().any(|&ancestor| ancestor == first_legend_id);
          if !in_first_legend {
            return true;
          }
        }
        None => return true,
      }
    }

    current = current_node.parent;
  }

  false
}

/// Returns the first eligible `[autofocus]` element in DOM-connected tree order, if any.
///
/// This is a best-effort approximation of HTML's autofocus behavior intended for dom2-driven live
/// documents. The returned [`NodeId`] remains stable across DOM mutations that do not remove the
/// targeted node.
pub fn autofocus_target_node_id(dom: &Document) -> Option<NodeId> {
  // (node_id, inherited_inert_or_hidden)
  let mut stack: Vec<(NodeId, bool)> = vec![(dom.root(), false)];

  while let Some((id, inherited_inert_or_hidden)) = stack.pop() {
    let Some(node) = dom.nodes().get(id.index()) else {
      continue;
    };

    let self_inert_or_hidden = inherited_inert_or_hidden
      || (node_is_element_like(node) && (node_self_is_inert(dom, node) || node_self_is_hidden(dom, node)));

    if node_is_element_like(node)
      && !self_inert_or_hidden
      && node_has_attr(dom, node, "autofocus")
      && is_potentially_focusable_element_for_autofocus(dom, node)
      && !is_effectively_disabled(dom, id)
    {
      return Some(id);
    }

    // Inert/hidden subtrees (including template inert contents via `inert_subtree=true`) should not
    // be traversed for autofocus target selection.
    if self_inert_or_hidden || node.inert_subtree {
      continue;
    }

    // Push children in reverse so we traverse left-to-right in document order. Only traverse
    // DOM-connected edges (child parent pointers must match).
    for &child in node.children.iter().rev() {
      let Some(child_node) = dom.nodes().get(child.index()) else {
        continue;
      };
      if child_node.parent == Some(id) {
        stack.push((child, self_inert_or_hidden));
      }
    }
  }

  None
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn autofocus_skips_disabled_controls() {
    let doc = crate::dom2::parse_html(
      "<!doctype html><html><body><input id=\"skip\" autofocus disabled><input id=\"target\" autofocus></body></html>",
    )
    .expect("parse");

    let target = doc.get_element_by_id("target").expect("target id");
    assert_eq!(autofocus_target_node_id(&doc), Some(target));
  }

  #[test]
  fn autofocus_respects_disabled_fieldset_first_legend_exception() {
    let doc = crate::dom2::parse_html(
      "<!doctype html><html><body><fieldset disabled>\
         <input id=\"a\" autofocus>\
         <legend><input id=\"b\" autofocus></legend>\
       </fieldset></body></html>",
    )
    .expect("parse");

    let b = doc.get_element_by_id("b").expect("b id");
    assert_eq!(autofocus_target_node_id(&doc), Some(b));
  }
}
