use crate::dom::DomNode;
use crate::dom::HTML_NAMESPACE;
use crate::dom2;

/// Minimal interface for querying a DOM tree using stable pre-order ids.
///
/// This is intentionally lightweight so interaction subsystems can share spec-correct disabled/inert
/// resolution without having to depend on a particular index implementation.
pub(crate) trait DomIdLookup {
  fn len(&self) -> usize;
  fn node(&self, node_id: usize) -> Option<&DomNode>;
  fn parent_id(&self, node_id: usize) -> usize;
}

impl DomIdLookup for super::dom_index::DomIndex {
  fn len(&self) -> usize {
    super::dom_index::DomIndex::len(self)
  }

  fn node(&self, node_id: usize) -> Option<&DomNode> {
    self.node(node_id)
  }

  fn parent_id(&self, node_id: usize) -> usize {
    self.parent.get(node_id).copied().unwrap_or(0)
  }
}

/// Minimal interface for querying a `dom2` tree using stable [`dom2::NodeId`] handles.
///
/// `dom2` uses dense indices instead of the renderer DOM's 1-based pre-order ids, so this is kept
/// separate from [`DomIdLookup`] to avoid conflating the two schemes.
pub(crate) trait Dom2Lookup {
  fn len(&self) -> usize;
  fn node(&self, node_id: dom2::NodeId) -> Option<&dom2::Node>;
  fn parent_id(&self, node_id: dom2::NodeId) -> Option<dom2::NodeId>;
}

impl Dom2Lookup for dom2::Document {
  fn len(&self) -> usize {
    self.nodes_len()
  }

  fn node(&self, node_id: dom2::NodeId) -> Option<&dom2::Node> {
    self.nodes().get(node_id.index())
  }

  fn parent_id(&self, node_id: dom2::NodeId) -> Option<dom2::NodeId> {
    self.nodes().get(node_id.index()).and_then(|node| node.parent)
  }
}

fn is_html_namespace(node: &DomNode) -> bool {
  matches!(node.namespace(), Some(ns) if ns.is_empty() || ns == HTML_NAMESPACE)
}

fn is_html_namespace_dom2(node: &dom2::Node) -> bool {
  match &node.kind {
    dom2::NodeKind::Element { namespace, .. } | dom2::NodeKind::Slot { namespace, .. } => {
      namespace.is_empty() || namespace == HTML_NAMESPACE
    }
    _ => false,
  }
}

fn is_html_fieldset(node: &DomNode) -> bool {
  is_html_namespace(node)
    && node
      .tag_name()
      .is_some_and(|tag| tag.eq_ignore_ascii_case("fieldset"))
}

fn is_html_fieldset_dom2(node: &dom2::Node) -> bool {
  is_html_namespace_dom2(node)
    && matches!(
      &node.kind,
      dom2::NodeKind::Element { tag_name, .. } if tag_name.eq_ignore_ascii_case("fieldset")
    )
}

fn is_html_legend(node: &DomNode) -> bool {
  is_html_namespace(node)
    && node
      .tag_name()
      .is_some_and(|tag| tag.eq_ignore_ascii_case("legend"))
}

fn is_html_legend_dom2(node: &dom2::Node) -> bool {
  is_html_namespace_dom2(node)
    && matches!(
      &node.kind,
      dom2::NodeKind::Element { tag_name, .. } if tag_name.eq_ignore_ascii_case("legend")
    )
}

fn is_fieldset_disabled_candidate(node: &DomNode) -> bool {
  // HTML: fieldset disabledness only affects "form-associated elements" inside the fieldset. For
  // interaction we only need to support the core form controls we currently implement.
  is_html_namespace(node)
    && node.tag_name().is_some_and(|tag| {
      tag.eq_ignore_ascii_case("input")
        || tag.eq_ignore_ascii_case("select")
        || tag.eq_ignore_ascii_case("textarea")
        || tag.eq_ignore_ascii_case("button")
    })
}

fn is_fieldset_disabled_candidate_dom2(node: &dom2::Node) -> bool {
  // HTML: fieldset disabledness only affects "form-associated elements" inside the fieldset. For
  // interaction we only need to support the core form controls we currently implement.
  is_html_namespace_dom2(node)
    && matches!(
      &node.kind,
      dom2::NodeKind::Element { tag_name, .. }
        if tag_name.eq_ignore_ascii_case("input")
          || tag_name.eq_ignore_ascii_case("select")
          || tag_name.eq_ignore_ascii_case("textarea")
          || tag_name.eq_ignore_ascii_case("button")
    )
}

/// Returns the first `<legend>` element child of `fieldset`, if any.
fn fieldset_first_legend_child_ptr(fieldset: &DomNode) -> Option<*const DomNode> {
  debug_assert!(is_html_fieldset(fieldset));
  fieldset.children.iter().find_map(|child| {
    is_html_legend(child).then_some(child as *const DomNode)
  })
}

/// Returns the first `<legend>` element child of `fieldset`, if any.
fn fieldset_first_legend_child_id_dom2(
  fieldset_id: dom2::NodeId,
  dom: &impl Dom2Lookup,
) -> Option<dom2::NodeId> {
  let fieldset = dom.node(fieldset_id)?;
  debug_assert!(is_html_fieldset_dom2(fieldset));
  for &child_id in fieldset.children.iter() {
    let Some(child) = dom.node(child_id) else {
      continue;
    };
    if dom.parent_id(child_id) != Some(fieldset_id) {
      continue;
    }
    if is_html_legend_dom2(child) {
      return Some(child_id);
    }
  }
  None
}

pub(crate) fn node_self_is_inert(node: &DomNode) -> bool {
  // `<template>` contents are always inert. `template_contents_are_inert()` is true for the
  // template element itself; descendants become inert via ancestor checks.
  if node.template_contents_are_inert() {
    return true;
  }
  if node.get_attribute_ref("inert").is_some() {
    return true;
  }
  node
    .get_attribute_ref("data-fastr-inert")
    .is_some_and(|v| v.eq_ignore_ascii_case("true"))
}

fn node_get_attribute_dom2<'a>(node: &'a dom2::Node, name: &str) -> Option<&'a str> {
  match &node.kind {
    dom2::NodeKind::Element { attributes, .. } | dom2::NodeKind::Slot { attributes, .. } => {
      attributes
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(name))
        .map(|(_, v)| v.as_str())
    }
    _ => None,
  }
}

pub(crate) fn node_self_is_inert_dom2(node: &dom2::Node) -> bool {
  // `<template>` contents are always inert. `inert_subtree` is true for the template element itself;
  // descendants become inert via ancestor checks.
  if node.inert_subtree {
    return true;
  }
  if node_get_attribute_dom2(node, "inert").is_some() {
    return true;
  }
  node_get_attribute_dom2(node, "data-fastr-inert")
    .is_some_and(|v| v.eq_ignore_ascii_case("true"))
}

pub(crate) fn node_self_is_hidden(node: &DomNode) -> bool {
  if node.get_attribute_ref("hidden").is_some() {
    return true;
  }
  node
    .get_attribute_ref("data-fastr-hidden")
    .is_some_and(|v| v.eq_ignore_ascii_case("true"))
}

pub(crate) fn node_self_is_hidden_dom2(node: &dom2::Node) -> bool {
  if node_get_attribute_dom2(node, "hidden").is_some() {
    return true;
  }
  node_get_attribute_dom2(node, "data-fastr-hidden")
    .is_some_and(|v| v.eq_ignore_ascii_case("true"))
}

pub(crate) fn is_effectively_inert(node_id: usize, dom: &impl DomIdLookup) -> bool {
  let mut current = node_id;
  while current != 0 {
    let Some(node) = dom.node(current) else {
      return false;
    };
    if node.is_element() && node_self_is_inert(node) {
      return true;
    }
    current = dom.parent_id(current);
  }
  false
}

pub(crate) fn is_effectively_inert_dom2(node_id: dom2::NodeId, dom: &impl Dom2Lookup) -> bool {
  let mut current = Some(node_id);
  // Defensive bound to avoid infinite loops on corrupted trees.
  let mut remaining = dom.len().saturating_add(1);
  while let Some(node_id) = current {
    if remaining == 0 {
      break;
    }
    remaining -= 1;

    let Some(node) = dom.node(node_id) else {
      return false;
    };
    if matches!(
      &node.kind,
      dom2::NodeKind::Element { .. } | dom2::NodeKind::Slot { .. }
    ) && node_self_is_inert_dom2(node)
    {
      return true;
    }
    current = dom.parent_id(node_id);
  }
  false
}

pub(crate) fn is_effectively_hidden(node_id: usize, dom: &impl DomIdLookup) -> bool {
  let mut current = node_id;
  while current != 0 {
    let Some(node) = dom.node(current) else {
      return false;
    };
    if node.is_element() && node_self_is_hidden(node) {
      return true;
    }
    current = dom.parent_id(current);
  }
  false
}

pub(crate) fn is_effectively_hidden_dom2(node_id: dom2::NodeId, dom: &impl Dom2Lookup) -> bool {
  let mut current = Some(node_id);
  // Defensive bound to avoid infinite loops on corrupted trees.
  let mut remaining = dom.len().saturating_add(1);
  while let Some(node_id) = current {
    if remaining == 0 {
      break;
    }
    remaining -= 1;

    let Some(node) = dom.node(node_id) else {
      return false;
    };
    if matches!(
      &node.kind,
      dom2::NodeKind::Element { .. } | dom2::NodeKind::Slot { .. }
    ) && node_self_is_hidden_dom2(node)
    {
      return true;
    }
    current = dom.parent_id(node_id);
  }
  false
}

pub(crate) fn is_effectively_inert_or_hidden(node_id: usize, dom: &impl DomIdLookup) -> bool {
  let mut current = node_id;
  while current != 0 {
    let Some(node) = dom.node(current) else {
      return false;
    };
    if node.is_element() && (node_self_is_inert(node) || node_self_is_hidden(node)) {
      return true;
    }
    current = dom.parent_id(current);
  }
  false
}

/// Returns true if `node_id` is inside `<template>` contents (including the `<template>` element
/// itself).
pub(crate) fn is_in_template_contents(node_id: usize, dom: &impl DomIdLookup) -> bool {
  let mut current = node_id;
  while current != 0 {
    let Some(node) = dom.node(current) else {
      return false;
    };
    if node.template_contents_are_inert() {
      return true;
    }
    current = dom.parent_id(current);
  }
  false
}

/// Returns true if `node_id` is inside `<template>` contents (including the `<template>` element
/// itself).
pub(crate) fn is_in_template_contents_dom2(node_id: dom2::NodeId, dom: &impl Dom2Lookup) -> bool {
  let mut current = Some(node_id);
  // Defensive bound to avoid infinite loops on corrupted trees.
  let mut remaining = dom.len().saturating_add(1);
  while let Some(node_id) = current {
    if remaining == 0 {
      break;
    }
    remaining -= 1;

    let Some(node) = dom.node(node_id) else {
      return false;
    };
    if node.inert_subtree {
      return true;
    }
    current = dom.parent_id(node_id);
  }
  false
}

/// Spec-correct HTML disabled resolution for interaction:
///
/// - `disabled` on the element itself disables it.
/// - `fieldset[disabled]` disables descendant form controls, except those inside the fieldset's
///   first `<legend>` element child.
pub(crate) fn is_effectively_disabled(node_id: usize, dom: &impl DomIdLookup) -> bool {
  let Some(node) = dom.node(node_id) else {
    return false;
  };
  if !node.is_element() {
    return false;
  }

  // `disabled` is a boolean attribute on form controls, but authors also use it on custom controls;
  // for interaction treat it as authoritative on the element itself.
  if node.get_attribute_ref("disabled").is_some() {
    return true;
  }

  if !is_fieldset_disabled_candidate(node) {
    return false;
  }

  // Walk ancestors; `<fieldset disabled>` is the only ancestor-based disabledness we model (with the
  // first-legend exception).
  let mut ancestors: Vec<*const DomNode> = Vec::new();
  let mut current = node_id;
  while current != 0 {
    let Some(current_node) = dom.node(current) else {
      break;
    };
    ancestors.push(current_node as *const DomNode);

    if is_html_fieldset(current_node) && current_node.get_attribute_ref("disabled").is_some() {
      match fieldset_first_legend_child_ptr(current_node) {
        Some(first_legend_ptr) => {
          // If the first legend is on the ancestor chain, the control is inside that legend and is
          // *not* disabled by this fieldset.
          let in_first_legend = ancestors.iter().any(|&ptr| ptr == first_legend_ptr);
          if !in_first_legend {
            return true;
          }
        }
        None => return true,
      }
    }

    current = dom.parent_id(current);
  }

  false
}

/// dom2 equivalent of [`is_effectively_disabled`], operating on stable [`dom2::NodeId`] handles.
pub(crate) fn is_effectively_disabled_dom2(node_id: dom2::NodeId, dom: &impl Dom2Lookup) -> bool {
  let Some(node) = dom.node(node_id) else {
    return false;
  };
  if !matches!(
    &node.kind,
    dom2::NodeKind::Element { .. } | dom2::NodeKind::Slot { .. }
  ) {
    return false;
  }

  // `disabled` is a boolean attribute on form controls, but authors also use it on custom controls;
  // for interaction treat it as authoritative on the element itself.
  if node_get_attribute_dom2(node, "disabled").is_some() {
    return true;
  }

  if !is_fieldset_disabled_candidate_dom2(node) {
    return false;
  }

  // Walk ancestors; `<fieldset disabled>` is the only ancestor-based disabledness we model (with the
  // first-legend exception).
  let mut ancestors: Vec<dom2::NodeId> = Vec::new();
  let mut current = Some(node_id);
  // Defensive bound to avoid infinite loops on corrupted trees.
  let mut remaining = dom.len().saturating_add(1);
  while let Some(current_id) = current {
    if remaining == 0 {
      break;
    }
    remaining -= 1;

    let Some(current_node) = dom.node(current_id) else {
      break;
    };
    ancestors.push(current_id);

    if is_html_fieldset_dom2(current_node) && node_get_attribute_dom2(current_node, "disabled").is_some() {
      match fieldset_first_legend_child_id_dom2(current_id, dom) {
        Some(first_legend_id) => {
          // If the first legend is on the ancestor chain, the control is inside that legend and is
          // *not* disabled by this fieldset.
          let in_first_legend = ancestors.iter().any(|&id| id == first_legend_id);
          if !in_first_legend {
            return true;
          }
        }
        None => return true,
      }
    }

    current = dom.parent_id(current_id);
  }

  false
}

#[cfg(test)]
mod tests {
  use super::*;

  fn id(dom: &mut DomNode, html_id: &str) -> usize {
    let index = crate::interaction::dom_index::DomIndex::build(dom);
    *index
      .id_by_element_id
      .get(html_id)
      .unwrap_or_else(|| panic!("missing id {html_id}"))
  }

  fn id_dom2(doc: &dom2::Document, html_id: &str) -> dom2::NodeId {
    doc
      .get_element_by_id(html_id)
      .unwrap_or_else(|| panic!("missing id {html_id}"))
  }

  fn id_dom2_anywhere(doc: &dom2::Document, html_id: &str) -> dom2::NodeId {
    for (index, node) in doc.nodes().iter().enumerate() {
      let attrs: &[(String, String)] = match &node.kind {
        dom2::NodeKind::Element { attributes, .. } | dom2::NodeKind::Slot { attributes, .. } => {
          attributes
        }
        _ => continue,
      };
      if attrs
        .iter()
        .any(|(name, value)| name.eq_ignore_ascii_case("id") && value == html_id)
      {
        return dom2::NodeId::from_index(index);
      }
    }
    panic!("missing id {html_id}");
  }

  #[test]
  fn fieldset_disabled_first_legend_exception() {
    let mut dom = crate::dom::parse_html(
      r#"<!doctype html>
      <fieldset disabled>
        <legend><input id="a"></legend>
        <input id="b">
      </fieldset>"#,
    )
    .unwrap();

    let index = crate::interaction::dom_index::DomIndex::build(&mut dom);
    let a = *index.id_by_element_id.get("a").unwrap();
    let b = *index.id_by_element_id.get("b").unwrap();

    assert!(!is_effectively_disabled(a, &index));
    assert!(is_effectively_disabled(b, &index));
  }

  #[test]
  fn fieldset_disabled_first_legend_exception_dom2() {
    let doc = crate::dom2::parse_html(
      r#"<!doctype html>
      <fieldset disabled>
        <legend><input id="a"></legend>
        <input id="b">
      </fieldset>"#,
    )
    .unwrap();
    let a = id_dom2(&doc, "a");
    let b = id_dom2(&doc, "b");

    assert!(!is_effectively_disabled_dom2(a, &doc));
    assert!(is_effectively_disabled_dom2(b, &doc));
  }

  #[test]
  fn fieldset_disabled_second_legend_is_not_exempt() {
    let mut dom = crate::dom::parse_html(
      r#"<!doctype html>
      <fieldset disabled>
        <legend><input id="a"></legend>
        <legend><input id="b"></legend>
      </fieldset>"#,
    )
    .unwrap();

    let index = crate::interaction::dom_index::DomIndex::build(&mut dom);
    let a = *index.id_by_element_id.get("a").unwrap();
    let b = *index.id_by_element_id.get("b").unwrap();

    assert!(!is_effectively_disabled(a, &index));
    assert!(is_effectively_disabled(b, &index));
  }

  #[test]
  fn fieldset_disabled_second_legend_is_not_exempt_dom2() {
    let doc = crate::dom2::parse_html(
      r#"<!doctype html>
      <fieldset disabled>
        <legend><input id="a"></legend>
        <legend><input id="b"></legend>
      </fieldset>"#,
    )
    .unwrap();
    let a = id_dom2(&doc, "a");
    let b = id_dom2(&doc, "b");

    assert!(!is_effectively_disabled_dom2(a, &doc));
    assert!(is_effectively_disabled_dom2(b, &doc));
  }

  #[test]
  fn nested_fieldsets_respect_first_legend_exception() {
    let mut dom = crate::dom::parse_html(
      r#"<!doctype html>
      <fieldset disabled>
        <legend>
          <fieldset disabled>
            <legend><input id="inner_ok"></legend>
            <input id="inner_disabled">
          </fieldset>
        </legend>
        <input id="outer_disabled">
      </fieldset>"#,
    )
    .unwrap();
 
    let index = crate::interaction::dom_index::DomIndex::build(&mut dom);
 
    let inner_ok = *index.id_by_element_id.get("inner_ok").unwrap();
    let inner_disabled = *index.id_by_element_id.get("inner_disabled").unwrap();
    let outer_disabled = *index.id_by_element_id.get("outer_disabled").unwrap();

    assert!(!is_effectively_disabled(inner_ok, &index));
    assert!(is_effectively_disabled(inner_disabled, &index));
    assert!(is_effectively_disabled(outer_disabled, &index));
  }

  #[test]
  fn nested_fieldsets_respect_first_legend_exception_dom2() {
    let doc = crate::dom2::parse_html(
      r#"<!doctype html>
      <fieldset disabled>
        <legend>
          <fieldset disabled>
            <legend><input id="inner_ok"></legend>
            <input id="inner_disabled">
          </fieldset>
        </legend>
        <input id="outer_disabled">
      </fieldset>"#,
    )
    .unwrap();
    let inner_ok = id_dom2(&doc, "inner_ok");
    let inner_disabled = id_dom2(&doc, "inner_disabled");
    let outer_disabled = id_dom2(&doc, "outer_disabled");

    assert!(!is_effectively_disabled_dom2(inner_ok, &doc));
    assert!(is_effectively_disabled_dom2(inner_disabled, &doc));
    assert!(is_effectively_disabled_dom2(outer_disabled, &doc));
  }

  #[test]
  fn fieldset_disabled_does_not_disable_non_control_descendants() {
    let mut dom = crate::dom::parse_html(
      r#"<!doctype html>
      <fieldset disabled>
        <legend>Legend</legend>
        <div id="x" tabindex="0">x</div>
      </fieldset>"#,
    )
    .unwrap();

    let x_id = id(&mut dom, "x");
    let index = crate::interaction::dom_index::DomIndex::build(&mut dom);
    assert!(
      !is_effectively_disabled(x_id, &index),
      "fieldset[disabled] must not disable non-form-control descendants"
    );
  }

  #[test]
  fn fieldset_disabled_does_not_disable_non_control_descendants_dom2() {
    let doc = crate::dom2::parse_html(
      r#"<!doctype html>
      <fieldset disabled>
        <legend>Legend</legend>
        <div id="x" tabindex="0">x</div>
      </fieldset>"#,
    )
    .unwrap();
    let x_id = id_dom2(&doc, "x");
    assert!(
      !is_effectively_disabled_dom2(x_id, &doc),
      "fieldset[disabled] must not disable non-form-control descendants"
    );
  }

  #[test]
  fn inert_propagates_dom2() {
    let doc = crate::dom2::parse_html(
      r#"<!doctype html>
      <div inert>
        <button id="a">a</button>
      </div>
      <button id="b">b</button>"#,
    )
    .unwrap();
    let a = id_dom2(&doc, "a");
    let b = id_dom2(&doc, "b");

    assert!(is_effectively_inert_dom2(a, &doc));
    assert!(!is_effectively_inert_dom2(b, &doc));
  }

  #[test]
  fn template_contents_are_inert_dom2() {
    let doc = crate::dom2::parse_html(
      r#"<!doctype html>
      <template>
        <button id="t">t</button>
      </template>"#,
    )
    .unwrap();
    let t = id_dom2_anywhere(&doc, "t");
    assert!(
      is_effectively_inert_dom2(t, &doc),
      "template contents must be treated as inert"
    );
    assert!(
      is_in_template_contents_dom2(t, &doc),
      "expected is_in_template_contents_dom2 to return true for template descendants"
    );
  }
}
