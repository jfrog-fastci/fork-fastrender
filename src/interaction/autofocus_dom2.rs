use crate::dom2::{Document, NodeId, NodeKind, NULL_NAMESPACE};
use crate::interaction::InteractionStateDom2;

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
    .any(|attr| attr.qualified_name_matches(name, is_html))
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
    .find(|attr| attr.qualified_name_matches(name, is_html))
    .map(|attr| attr.value.as_str())
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
        // Match browser behavior: an explicit `href` attribute is a link target even when it is
        // empty/whitespace-only (`<a href="">`).
        !href
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

/// Returns the `dom2` [`NodeId`] of the first eligible `[autofocus]` element, if any.
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
      || (node_is_element_like(node)
        && (super::effective_disabled_dom2::node_self_is_inert(id, dom)
          || super::effective_disabled_dom2::node_self_is_hidden(id, dom)));

    if node_is_element_like(node)
      && !self_inert_or_hidden
      && node_has_attr(dom, node, "autofocus")
      && is_potentially_focusable_element_for_autofocus(dom, node)
      && !super::effective_disabled_dom2::is_effectively_disabled(id, dom)
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

/// Build an [`InteractionStateDom2`] reflecting initial autofocus selection, if any.
///
/// This enables correct `:focus` selector matching (and related paint effects such as
/// caret/selection rendering) for initial/static renders backed by a live `dom2::Document`.
///
/// Returns `None` when no eligible autofocus element is present.
pub fn interaction_state_for_autofocus(doc: &Document) -> Option<InteractionStateDom2> {
  let focused = autofocus_target_node_id(doc)?;

  let mut focus_chain: Vec<NodeId> = Vec::new();
  for ancestor in doc.ancestors(focused) {
    let Some(node) = doc.nodes().get(ancestor.index()) else {
      continue;
    };
    if node_is_element_like(node) {
      focus_chain.push(ancestor);
    }
  }

  Some(InteractionStateDom2 {
    focused: Some(focused),
    // Autofocus is not pointer-driven. Err on the side of matching `:focus-visible` as well,
    // which aligns with typical browser behavior for initially focused text controls.
    focus_visible: true,
    focus_chain,
    ..InteractionStateDom2::default()
  })
}

#[cfg(test)]
mod tests {
  use super::*;
  use selectors::context::QuirksMode;

  #[test]
  fn autofocus_matches_attribute_name_case_insensitively_in_html_namespace() {
    let mut doc = crate::dom2::parse_html("<html><body><input id=\"target\"></body></html>")
      .expect("parse");

    let target = doc.get_element_by_id("target").expect("target id");
    doc
      .set_bool_attribute(target, "AutoFocus", true)
      .expect("set AutoFocus");

    assert_eq!(autofocus_target_node_id(&doc), Some(target));
  }

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
  fn autofocus_treats_empty_href_as_focusable_link() {
    let doc = crate::dom2::parse_html(
      "<html><body><a id=\"a\" href=\"\" autofocus></a></body></html>",
    )
    .expect("parse");

    let a = doc.get_element_by_id("a").expect("a id");
    assert_eq!(autofocus_target_node_id(&doc), Some(a));
    let state = interaction_state_for_autofocus(&doc).expect("state");
    assert_eq!(state.focused, Some(a));
  }

  #[test]
  fn autofocus_respects_disabled_fieldset_first_legend_exception() {
    let doc = crate::dom2::parse_html(
      "<html><body><fieldset disabled>\
         <input id=\"a\" autofocus>\
         <legend><input id=\"b\" autofocus></legend>\
       </fieldset></body></html>",
    )
    .expect("parse");

    let input_a = doc.get_element_by_id("a").expect("a id");
    let input_b = doc.get_element_by_id("b").expect("b id");
    assert_ne!(input_a, input_b);

    assert_eq!(autofocus_target_node_id(&doc), Some(input_b));
    let state = interaction_state_for_autofocus(&doc).expect("state");
    assert_eq!(state.focused, Some(input_b));
  }

  #[test]
  fn autofocus_does_not_treat_disabled_fieldset_as_inert_for_tabindex_elements() {
    let doc = crate::dom2::parse_html(
      "<html><body><fieldset disabled><div id=\"d\" tabindex=\"0\" autofocus></div></fieldset></body></html>",
    )
    .expect("parse");

    let div_id = doc.get_element_by_id("d").expect("d id");

    assert_eq!(autofocus_target_node_id(&doc), Some(div_id));
    let state = interaction_state_for_autofocus(&doc).expect("state");
    assert_eq!(state.focused, Some(div_id));
  }

  #[test]
  fn autofocus_ignores_controls_disabled_by_fieldset() {
    let doc = crate::dom2::parse_html(
      "<html><body><fieldset disabled><input id=\"a\" autofocus></fieldset></body></html>",
    )
    .expect("parse");

    assert_eq!(autofocus_target_node_id(&doc), None);
    assert!(interaction_state_for_autofocus(&doc).is_none());
  }

  #[test]
  fn node_attr_helpers_match_html_case_insensitively() {
    let mut doc = Document::new(QuirksMode::NoQuirks);
    let el = doc.create_element("div", "");
    doc.append_child(doc.root(), el).unwrap();
    doc.set_attribute(el, "DATA-TEST", "ok").unwrap();

    let node = doc.node(el);
    assert!(node_has_attr(&doc, node, "data-test"));
    assert!(node_has_attr(&doc, node, "DATA-TEST"));
    assert_eq!(node_get_attr(&doc, node, "data-test"), Some("ok"));
    assert_eq!(node_get_attr(&doc, node, "Data-Test"), Some("ok"));
  }

  #[test]
  fn node_attr_helpers_match_xml_case_sensitively() {
    let mut doc = Document::new_xml();
    let el = doc.create_element("div", "");
    doc.append_child(doc.root(), el).unwrap();
    doc.set_attribute(el, "DATA-TEST", "ok").unwrap();

    let node = doc.node(el);
    assert!(node_has_attr(&doc, node, "DATA-TEST"));
    assert!(!node_has_attr(&doc, node, "data-test"));
    assert!(!node_has_attr(&doc, node, "Data-Test"));
    assert_eq!(node_get_attr(&doc, node, "DATA-TEST"), Some("ok"));
    assert_eq!(node_get_attr(&doc, node, "data-test"), None);
    assert_eq!(node_get_attr(&doc, node, "Data-Test"), None);
  }
}
