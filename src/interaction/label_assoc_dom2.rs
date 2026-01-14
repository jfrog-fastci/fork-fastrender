use crate::dom2;

fn trim_ascii_whitespace(value: &str) -> &str {
  // HTML label association should trim *ASCII* whitespace, not all Unicode whitespace. This mirrors
  // the legacy interaction engine behavior.
  value.trim_matches(|c: char| matches!(c, '\u{0009}' | '\u{000A}' | '\u{000C}' | '\u{000D}' | ' '))
}

fn is_element(dom: &dom2::Document, node: dom2::NodeId) -> bool {
  match dom.nodes().get(node.index()).map(|n| &n.kind) {
    Some(dom2::NodeKind::Element { .. } | dom2::NodeKind::Slot { .. }) => true,
    _ => false,
  }
}

fn is_html_element_with_tag(dom: &dom2::Document, node: dom2::NodeId, tag: &str) -> bool {
  let Some(dom2::NodeKind::Element {
    tag_name, namespace, ..
  }) = dom.nodes().get(node.index()).map(|n| &n.kind)
  else {
    return false;
  };
  dom.is_html_case_insensitive_namespace(namespace) && tag_name.eq_ignore_ascii_case(tag)
}

fn tree_root_boundary(dom: &dom2::Document, mut node: dom2::NodeId) -> Option<dom2::NodeId> {
  // Walk up the DOM parent chain to find the nearest Document/ShadowRoot, matching the legacy
  // `tree_root_boundary_id` logic used by `InteractionEngine`.
  //
  // Note: `dom2` stores shadow roots as children of their host element; this traversal therefore
  // naturally finds the shadow root before the host's ancestors.
  let mut remaining = dom.nodes_len().saturating_add(1);
  while remaining > 0 {
    remaining = remaining.saturating_sub(1);
    let node_ref = dom.nodes().get(node.index())?;
    if matches!(
      node_ref.kind,
      dom2::NodeKind::Document { .. } | dom2::NodeKind::ShadowRoot { .. }
    ) {
      return Some(node);
    }
    node = dom.parent_node(node)?;
  }
  None
}

fn input_type(dom: &dom2::Document, node: dom2::NodeId) -> Option<&str> {
  let raw = dom.get_attribute(node, "type").ok().flatten().unwrap_or("");
  let trimmed = trim_ascii_whitespace(raw);
  if trimmed.is_empty() { Some("text") } else { Some(trimmed) }
}

fn is_labelable_form_control(dom: &dom2::Document, node: dom2::NodeId) -> bool {
  if is_html_element_with_tag(dom, node, "input") {
    return input_type(dom, node)
      .is_some_and(|ty| !ty.eq_ignore_ascii_case("hidden"));
  }
  is_html_element_with_tag(dom, node, "textarea")
    || is_html_element_with_tag(dom, node, "select")
    || is_html_element_with_tag(dom, node, "button")
}

fn is_label(dom: &dom2::Document, node: dom2::NodeId) -> bool {
  is_html_element_with_tag(dom, node, "label")
}

/// Resolve the "associated control" for a `<label>` element in a `dom2::Document`.
///
/// This mirrors the legacy `InteractionEngine` behavior:
/// - If `for` is present and non-empty, look up the referenced element **within the label's tree
///   root boundary** (Document or ShadowRoot containing the label), and return it only if it is a
///   labelable form control.
/// - Otherwise, return the first labelable descendant form control, again staying within the same
///   tree root boundary (i.e. do not pierce nested shadow roots).
pub(crate) fn find_label_associated_control_dom2(
  dom: &dom2::Document,
  label: dom2::NodeId,
) -> Option<dom2::NodeId> {
  // Defensive: avoid panics on invalid NodeIds.
  dom.nodes().get(label.index())?;

  if !is_label(dom, label) {
    return None;
  }

  let label_tree_root = tree_root_boundary(dom, label)?;

  if let Some(for_value) = dom
    .get_attribute(label, "for")
    .ok()
    .flatten()
    .map(trim_ascii_whitespace)
    .filter(|v| !v.is_empty())
  {
    let referenced = dom.get_element_by_id_from(label_tree_root, for_value)?;
    return is_labelable_form_control(dom, referenced).then_some(referenced);
  }

  for candidate in dom.subtree_preorder(label) {
    if candidate == label {
      continue;
    }
    if tree_root_boundary(dom, candidate) != Some(label_tree_root) {
      continue;
    }
    if is_labelable_form_control(dom, candidate) {
      return Some(candidate);
    }
  }

  None
}

fn collect_element_chain(dom: &dom2::Document, start: dom2::NodeId) -> Vec<dom2::NodeId> {
  let mut chain = Vec::new();
  let mut current = Some(start);
  let mut remaining = dom.nodes_len().saturating_add(1);
  while let Some(node_id) = current {
    if remaining == 0 {
      break;
    }
    remaining = remaining.saturating_sub(1);
    if is_element(dom, node_id) {
      chain.push(node_id);
    }
    current = dom.parent_node(node_id);
  }
  chain
}

/// Collect the element ancestor chain for `start`, unioned with the element chain of any label
/// associated controls.
///
/// HTML defines that when a label matches `:hover`/`:active`, its associated control should also
/// match. The interaction engine approximates this by unioning ancestor chains.
pub(crate) fn collect_element_chain_with_label_associated_controls_dom2(
  dom: &dom2::Document,
  start: dom2::NodeId,
) -> Vec<dom2::NodeId> {
  let mut chain = collect_element_chain(dom, start);
  let baseline = chain.clone();
  for id in baseline {
    if is_label(dom, id) {
      if let Some(control) = find_label_associated_control_dom2(dom, id) {
        for control_id in collect_element_chain(dom, control) {
          if !chain.contains(&control_id) {
            chain.push(control_id);
          }
        }
      }
    }
  }
  chain
}

#[cfg(test)]
mod tests {
  use super::*;

  fn find_node_by_id_attribute_any_tree(doc: &dom2::Document, id: &str) -> Option<dom2::NodeId> {
    doc.nodes().iter().enumerate().find_map(|(idx, node)| {
      let (
        dom2::NodeKind::Element {
          namespace,
          attributes,
          ..
        }
        | dom2::NodeKind::Slot {
          namespace,
          attributes,
          ..
        }
      ) = &node.kind
      else {
        return None;
      };
      let is_html = doc.is_html_case_insensitive_namespace(namespace);
      attributes
        .iter()
        .any(|attr| attr.qualified_name_matches("id", is_html) && attr.value == id)
        .then_some(dom2::NodeId::from_index(idx))
    })
  }

  #[test]
  fn label_click_activates_associated_checkbox() {
    let mut doc = dom2::parse_html(
      "<!doctype html><html><body><label id=lbl for=cb>Label</label><input id=cb type=checkbox></body></html>",
    )
    .expect("parse dom2");

    let label = doc.get_element_by_id("lbl").expect("label");
    let cb = doc.get_element_by_id("cb").expect("checkbox input");

    assert_eq!(
      find_label_associated_control_dom2(&doc, label),
      Some(cb),
      "label[for] should resolve to the referenced checkbox"
    );

    // Simulate "activation" by toggling checkedness on the resolved control.
    let before = doc.input_checked(cb).expect("input_checked");
    let resolved = find_label_associated_control_dom2(&doc, label).expect("associated control");
    doc
      .set_input_checked(resolved, !before)
      .expect("set_input_checked");
    assert!(
      doc.input_checked(cb).expect("input_checked"),
      "label activation should toggle the associated checkbox"
    );
  }

  #[test]
  fn label_for_ignores_non_form_control_target() {
    let doc = dom2::parse_html(
      "<!doctype html><html><body><label id=lbl for=x>Label</label><a id=x href=/foo>Link</a></body></html>",
    )
    .expect("parse dom2");

    let label = doc.get_element_by_id("lbl").expect("label");
    assert_eq!(
      find_label_associated_control_dom2(&doc, label),
      None,
      "label[for] should only resolve to labelable form controls"
    );
  }

  #[test]
  fn label_for_does_not_cross_shadow_root_boundary() {
    let doc = dom2::parse_html(concat!(
      "<!doctype html>",
      "<html><body>",
      "<input id=cb type=checkbox>",
      "<div id=host>",
      "<template shadowroot=open>",
      "<label id=lbl for=cb>Label</label>",
      "</template>",
      "</div>",
      "</body></html>",
    ))
    .expect("parse dom2");

    let label =
      find_node_by_id_attribute_any_tree(&doc, "lbl").expect("label in shadow root");

    assert_eq!(
      doc.get_element_by_id("lbl"),
      None,
      "Document.getElementById must not pierce shadow roots"
    );

    assert_eq!(
      find_label_associated_control_dom2(&doc, label),
      None,
      "label `for` must not match an element outside the label's tree root (shadow root boundary)"
    );
  }
}
