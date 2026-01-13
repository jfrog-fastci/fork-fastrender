use crate::dom2;
use crate::dom2::{Document, NodeId, NodeKind};

fn node_is_element_like(node: NodeId, dom: &Document) -> bool {
  dom
    .nodes()
    .get(node.index())
    .is_some_and(|n| matches!(n.kind, NodeKind::Element { .. } | NodeKind::Slot { .. }))
}

fn is_html_fieldset(node: NodeId, dom: &Document) -> bool {
  let Some(node) = dom.nodes().get(node.index()) else {
    return false;
  };
  match &node.kind {
    NodeKind::Element {
      tag_name,
      namespace,
      ..
    } => {
      dom.is_html_case_insensitive_namespace(namespace) && tag_name.eq_ignore_ascii_case("fieldset")
    }
    _ => false,
  }
}

fn is_html_legend(node: NodeId, dom: &Document) -> bool {
  let Some(node) = dom.nodes().get(node.index()) else {
    return false;
  };
  match &node.kind {
    NodeKind::Element {
      tag_name,
      namespace,
      ..
    } => {
      dom.is_html_case_insensitive_namespace(namespace) && tag_name.eq_ignore_ascii_case("legend")
    }
    _ => false,
  }
}

fn is_fieldset_disabled_candidate(node: NodeId, dom: &Document) -> bool {
  let Some(node) = dom.nodes().get(node.index()) else {
    return false;
  };
  match &node.kind {
    NodeKind::Element {
      tag_name,
      namespace,
      ..
    } => {
      dom.is_html_case_insensitive_namespace(namespace)
        && (tag_name.eq_ignore_ascii_case("input")
          || tag_name.eq_ignore_ascii_case("select")
          || tag_name.eq_ignore_ascii_case("textarea")
          || tag_name.eq_ignore_ascii_case("button"))
    }
    _ => false,
  }
}

/// Returns the first `<legend>` element child of `fieldset`, if any.
fn fieldset_first_legend_child(fieldset: NodeId, dom: &Document) -> Option<NodeId> {
  debug_assert!(is_html_fieldset(fieldset, dom));
  let node = dom.nodes().get(fieldset.index())?;
  for &child in &node.children {
    let Some(child_node) = dom.nodes().get(child.index()) else {
      continue;
    };
    if child_node.parent != Some(fieldset) {
      continue;
    }
    if is_html_legend(child, dom) {
      return Some(child);
    }
  }
  None
}

fn node_self_is_inert(node: NodeId, dom: &Document) -> bool {
  // `<template>` contents are always inert. In `dom2`, inert template contents are represented via
  // `Node::inert_subtree=true` on the `<template>` element. Legacy interaction logic treated the
  // `<template>` element itself as inert as well, so we do the same here.
  if dom
    .nodes()
    .get(node.index())
    .is_some_and(|n| n.inert_subtree)
  {
    return true;
  }

  // `inert` is a global attribute; treat it as authoritative for interaction even though our DOM
  // currently does not implement the full `HTMLElement.inert` IDL semantics.
  if dom.has_attribute(node, "inert").unwrap_or(false) {
    return true;
  }

  // `data-fastr-*` attributes are internal escape hatches used by our renderer/interaction
  // subsystems to force inert/hidden semantics independent of authored attributes. Keep honoring
  // them for backwards compatibility.
  dom
    .get_attribute(node, "data-fastr-inert")
    .ok()
    .flatten()
    .is_some_and(|v| v.eq_ignore_ascii_case("true"))
}

fn node_self_is_hidden(node: NodeId, dom: &Document) -> bool {
  if dom.has_attribute(node, "hidden").unwrap_or(false) {
    return true;
  }
  dom
    .get_attribute(node, "data-fastr-hidden")
    .ok()
    .flatten()
    .is_some_and(|v| v.eq_ignore_ascii_case("true"))
}

pub(crate) fn is_effectively_inert(node: NodeId, dom: &dom2::Document) -> bool {
  // Keep legacy semantics: invalid node ids are treated as not inert (callers should validate ids).
  if dom.nodes().get(node.index()).is_none() {
    return false;
  }

  // `dom2` represents `<template>` contents as normal descendants, but exposes `is_connected_for_scripting`
  // to model the platform behaviour that scripts/interaction treat template contents as disconnected.
  if !dom.is_connected_for_scripting(node) {
    return true;
  }

  for ancestor in dom.ancestors(node) {
    if node_is_element_like(ancestor, dom) && node_self_is_inert(ancestor, dom) {
      return true;
    }
  }
  false
}

pub(crate) fn is_effectively_hidden(node: NodeId, dom: &dom2::Document) -> bool {
  for ancestor in dom.ancestors(node) {
    if node_is_element_like(ancestor, dom) && node_self_is_hidden(ancestor, dom) {
      return true;
    }
  }
  false
}

pub(crate) fn is_effectively_inert_or_hidden(node: NodeId, dom: &dom2::Document) -> bool {
  is_effectively_inert(node, dom) || is_effectively_hidden(node, dom)
}

/// Returns true if `node` is inside `<template>` contents (including the `<template>` element
/// itself).
pub(crate) fn is_in_template_contents(node: NodeId, dom: &dom2::Document) -> bool {
  if dom.nodes().get(node.index()).is_none() {
    return false;
  }

  dom.ancestors(node).any(|ancestor| {
    dom
      .nodes()
      .get(ancestor.index())
      .is_some_and(|n| n.inert_subtree)
  })
}

/// Spec-correct HTML disabled resolution for interaction:
///
/// - `disabled` on the element itself disables it.
/// - `fieldset[disabled]` disables descendant form controls, except those inside the fieldset's
///   first `<legend>` element child.
pub(crate) fn is_effectively_disabled(node: NodeId, dom: &dom2::Document) -> bool {
  if !node_is_element_like(node, dom) {
    return false;
  }

  // `disabled` is a boolean attribute on form controls, but authors also use it on custom controls;
  // for interaction treat it as authoritative on the element itself.
  if dom.has_attribute(node, "disabled").unwrap_or(false) {
    return true;
  }

  if !is_fieldset_disabled_candidate(node, dom) {
    return false;
  }

  // Walk ancestors; `<fieldset disabled>` is the only ancestor-based disabledness we model (with the
  // first-legend exception).
  let mut ancestors: Vec<NodeId> = Vec::new();
  for current in dom.ancestors(node) {
    ancestors.push(current);

    if is_html_fieldset(current, dom) && dom.has_attribute(current, "disabled").unwrap_or(false) {
      match fieldset_first_legend_child(current, dom) {
        Some(first_legend) => {
          // If the first legend is on the ancestor chain, the control is inside that legend and is
          // *not* disabled by this fieldset.
          if !ancestors.iter().any(|&id| id == first_legend) {
            return true;
          }
        }
        None => return true,
      }
    }
  }

  false
}

#[cfg(test)]
mod tests {
  use super::*;

  fn id(doc: &Document, html_id: &str) -> NodeId {
    doc
      .get_element_by_id(html_id)
      .unwrap_or_else(|| panic!("missing id {html_id}"))
  }

  #[test]
  fn fieldset_disabled_first_legend_exception() {
    let doc = crate::dom2::parse_html(
      r#"<!doctype html>
      <fieldset disabled>
        <legend><input id="a"></legend>
        <input id="b">
      </fieldset>"#,
    )
    .unwrap();

    let a = id(&doc, "a");
    let b = id(&doc, "b");

    assert!(!is_effectively_disabled(a, &doc));
    assert!(is_effectively_disabled(b, &doc));
  }

  #[test]
  fn fieldset_disabled_second_legend_is_not_exempt() {
    let doc = crate::dom2::parse_html(
      r#"<!doctype html>
      <fieldset disabled>
        <legend><input id="a"></legend>
        <legend><input id="b"></legend>
      </fieldset>"#,
    )
    .unwrap();

    let a = id(&doc, "a");
    let b = id(&doc, "b");

    assert!(!is_effectively_disabled(a, &doc));
    assert!(is_effectively_disabled(b, &doc));
  }

  #[test]
  fn nested_fieldsets_respect_first_legend_exception() {
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

    let inner_ok = id(&doc, "inner_ok");
    let inner_disabled = id(&doc, "inner_disabled");
    let outer_disabled = id(&doc, "outer_disabled");

    assert!(!is_effectively_disabled(inner_ok, &doc));
    assert!(is_effectively_disabled(inner_disabled, &doc));
    assert!(is_effectively_disabled(outer_disabled, &doc));
  }

  #[test]
  fn fieldset_disabled_does_not_disable_non_control_descendants() {
    let doc = crate::dom2::parse_html(
      r#"<!doctype html>
      <fieldset disabled>
        <legend>Legend</legend>
        <div id="x" tabindex="0">x</div>
      </fieldset>"#,
    )
    .unwrap();

    let x_id = id(&doc, "x");
    assert!(
      !is_effectively_disabled(x_id, &doc),
      "fieldset[disabled] must not disable non-form-control descendants"
    );
  }

  fn find_descendant_by_id_including_inert(
    doc: &Document,
    root: NodeId,
    html_id: &str,
  ) -> Option<NodeId> {
    let mut remaining = doc.nodes_len() + 1;
    let mut stack: Vec<NodeId> = vec![root];
    while let Some(node) = stack.pop() {
      if remaining == 0 {
        break;
      }
      remaining -= 1;

      if node_is_element_like(node, doc) {
        if doc.id(node).ok().flatten().is_some_and(|v| v == html_id) {
          return Some(node);
        }
      }

      let Some(node_ref) = doc.nodes().get(node.index()) else {
        continue;
      };
      for &child in node_ref.children.iter().rev() {
        let Some(child_ref) = doc.nodes().get(child.index()) else {
          continue;
        };
        if child_ref.parent != Some(node) {
          continue;
        }
        stack.push(child);
      }
    }
    None
  }

  #[test]
  fn template_contents_are_inert_and_disconnected_for_interaction() {
    let doc = crate::dom2::parse_html(
      r#"<!doctype html>
      <template id="t">
        <button id="inside"></button>
      </template>"#,
    )
    .unwrap();

    let template = id(&doc, "t");
    let inside = find_descendant_by_id_including_inert(&doc, template, "inside")
      .expect("expected to find #inside within <template>");

    assert!(is_effectively_inert(template, &doc));
    assert!(is_effectively_inert(inside, &doc));
    assert!(is_in_template_contents(template, &doc));
    assert!(is_in_template_contents(inside, &doc));
  }

  #[test]
  fn inert_and_hidden_attributes_apply_to_descendants() {
    let doc = crate::dom2::parse_html(
      r#"<!doctype html>
      <div inert><button id="inert_btn"></button></div>
      <div hidden><button id="hidden_btn"></button></div>"#,
    )
    .unwrap();

    let inert_btn = id(&doc, "inert_btn");
    let hidden_btn = id(&doc, "hidden_btn");

    assert!(is_effectively_inert(inert_btn, &doc));
    assert!(is_effectively_hidden(hidden_btn, &doc));
    assert!(is_effectively_inert_or_hidden(inert_btn, &doc));
    assert!(is_effectively_inert_or_hidden(hidden_btn, &doc));
  }

  #[test]
  fn invalid_node_ids_are_not_treated_as_inert() {
    let doc = crate::dom2::parse_html("<!doctype html><div></div>").unwrap();
    let invalid = NodeId::from_index(doc.nodes_len() + 10);
    assert!(!is_effectively_inert(invalid, &doc));
  }
}
