use crate::dom::DomNode;
use crate::dom2;
use crate::interaction::InteractionState;
use crate::interaction::InteractionStateDom2;
use std::ptr;

fn trim_ascii_whitespace(value: &str) -> &str {
  // HTML attribute processing (e.g. tabindex) trims ASCII whitespace only.
  value.trim_matches(|c: char| matches!(c, '\u{0009}' | '\u{000A}' | '\u{000C}' | '\u{000D}' | ' '))
}

fn parse_tabindex(node: &DomNode) -> Option<i32> {
  let raw = node.get_attribute_ref("tabindex")?;
  let raw = trim_ascii_whitespace(raw);
  if raw.is_empty() {
    return None;
  }
  raw.parse::<i32>().ok()
}

fn is_anchor_with_href(node: &DomNode) -> bool {
  node.tag_name().is_some_and(|tag| {
    (tag.eq_ignore_ascii_case("a") || tag.eq_ignore_ascii_case("area"))
      && node.get_attribute_ref("href").is_some_and(|href| {
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

fn input_type(node: &DomNode) -> &str {
  node
    .get_attribute_ref("type")
    .map(trim_ascii_whitespace)
    .filter(|v| !v.is_empty())
    .unwrap_or("text")
}

fn is_potentially_focusable_element_for_autofocus(node: &DomNode) -> bool {
  if !node.is_element() {
    return false;
  }

  // `input type=hidden` is never focusable, even when tabindex is set.
  if node
    .tag_name()
    .is_some_and(|tag| tag.eq_ignore_ascii_case("input"))
    && input_type(node).eq_ignore_ascii_case("hidden")
  {
    return false;
  }

  // `tabindex` makes any element focusable, even if it is not reachable via Tab (negative values).
  if parse_tabindex(node).is_some() {
    return true;
  }

  if is_anchor_with_href(node) {
    return true;
  }

  node.tag_name().is_some_and(|tag| {
    tag.eq_ignore_ascii_case("input")
      || tag.eq_ignore_ascii_case("textarea")
      || tag.eq_ignore_ascii_case("select")
      || tag.eq_ignore_ascii_case("button")
  })
}

struct DomIndex {
  id_to_ptr: Vec<*const DomNode>,
  parent: Vec<usize>,
  is_element: Vec<bool>,
  inert_or_hidden: Vec<bool>,
}

impl DomIndex {
  fn build(root: &DomNode) -> Self {
    let mut id_to_ptr: Vec<*const DomNode> = vec![ptr::null()];
    let mut parent: Vec<usize> = vec![0];
    let mut is_element: Vec<bool> = vec![false];
    let mut inert_or_hidden: Vec<bool> = vec![false];

    // Pre-order traversal, matching `crate::dom::enumerate_dom_ids`.
    // (node, parent_id, inherited_inert_or_hidden)
    let mut stack: Vec<(&DomNode, usize, bool)> = vec![(root, 0, false)];
    while let Some((node, parent_id, inherited_inert_or_hidden)) = stack.pop() {
      let id = id_to_ptr.len();
      id_to_ptr.push(node as *const DomNode);
      parent.push(parent_id);

      let self_is_element = node.is_element();
      is_element.push(self_is_element);

      let self_inert_or_hidden = inherited_inert_or_hidden
        || super::effective_disabled::node_self_is_inert(node)
        || super::effective_disabled::node_self_is_hidden(node);
      inert_or_hidden.push(self_inert_or_hidden);

      for child in node.children.iter().rev() {
        stack.push((child, id, self_inert_or_hidden));
      }
    }

    Self {
      id_to_ptr,
      parent,
      is_element,
      inert_or_hidden,
    }
  }

  fn len(&self) -> usize {
    self.id_to_ptr.len().saturating_sub(1)
  }

  fn node(&self, node_id: usize) -> Option<&DomNode> {
    let ptr = *self.id_to_ptr.get(node_id)?;
    if ptr.is_null() {
      return None;
    }
    // SAFETY: pointers originate from the DOM tree borrowed for the duration of the caller.
    Some(unsafe { &*ptr })
  }
}

impl super::effective_disabled::DomIdLookup for DomIndex {
  fn len(&self) -> usize {
    DomIndex::len(self)
  }

  fn node(&self, node_id: usize) -> Option<&DomNode> {
    DomIndex::node(self, node_id)
  }

  fn parent_id(&self, node_id: usize) -> usize {
    self.parent.get(node_id).copied().unwrap_or(0)
  }
}

fn autofocus_target_in_index(index: &DomIndex) -> Option<usize> {
  let node_len = index.len();
  for node_id in 1..=node_len {
    if index.inert_or_hidden.get(node_id).copied().unwrap_or(true) {
      continue;
    }
    if !index.is_element.get(node_id).copied().unwrap_or(false) {
      continue;
    }
    let Some(node) = index.node(node_id) else {
      continue;
    };
    if node.get_attribute_ref("autofocus").is_none() {
      continue;
    }
    if !is_potentially_focusable_element_for_autofocus(node) {
      continue;
    }
    if super::effective_disabled::is_effectively_disabled(node_id, index) {
      continue;
    }
    return Some(node_id);
  }
  None
}

/// Build an [`InteractionState`] reflecting initial autofocus selection, if any.
///
/// This is a best-effort approximation of HTML's autofocus behavior that enables correct `:focus`
/// selector matching (and related paint effects such as caret/selection rendering) for static
/// renders.
///
/// Returns `None` when no eligible autofocus element is present.
pub fn interaction_state_for_autofocus(dom: &DomNode) -> Option<InteractionState> {
  let index = DomIndex::build(dom);
  let focused_id = autofocus_target_in_index(&index)?;

  let mut focus_chain = Vec::new();
  let mut current = focused_id;
  while current != 0 {
    if index.is_element.get(current).copied().unwrap_or(false) {
      focus_chain.push(current);
    }
    current = index.parent.get(current).copied().unwrap_or(0);
  }

  let mut state = InteractionState::default();
  state.focused = Some(focused_id);
  // Autofocus is not pointer-driven. Err on the side of matching `:focus-visible` as well,
  // which aligns with typical browser behavior for initially focused text controls.
  state.focus_visible = true;
  state.set_focus_chain(focus_chain);
  Some(state)
}

/// Returns the pre-order DOM node id of the first eligible `[autofocus]` element, if any.
///
/// This shares the same best-effort eligibility rules as [`interaction_state_for_autofocus`], but
/// only returns the node id. This is intended for interactive/browser UI integrations that manage
/// their own [`crate::interaction::InteractionEngine`] state but still want spec-ish autofocus
/// target selection.
pub fn autofocus_target_node_id(dom: &DomNode) -> Option<usize> {
  let index = DomIndex::build(dom);
  autofocus_target_in_index(&index)
}

// -----------------------------------------------------------------------------
// dom2 variants (stable NodeId)
// -----------------------------------------------------------------------------

/// Returns the [`dom2::NodeId`] of the first eligible `[autofocus]` element in a live `dom2`
/// document.
///
/// This matches the best-effort eligibility rules of [`autofocus_target_node_id`] but returns a
/// stable `dom2` id that remains meaningful across incremental DOM updates.
pub fn autofocus_target_node_id_dom2(dom: &dom2::Document) -> Option<dom2::NodeId> {
  super::autofocus_dom2::autofocus_target_node_id(dom)
}

/// Build an [`InteractionStateDom2`] reflecting initial autofocus selection, if any.
///
/// This is the `dom2` equivalent of [`interaction_state_for_autofocus`].
pub fn interaction_state_for_autofocus_dom2(dom: &dom2::Document) -> Option<InteractionStateDom2> {
  super::autofocus_dom2::interaction_state_for_autofocus(dom)
}

#[cfg(test)]
mod tests {
  use super::*;

  fn find_node_by_id<'a>(root: &'a DomNode, id: &str) -> &'a DomNode {
    let mut stack: Vec<&DomNode> = vec![root];
    while let Some(node) = stack.pop() {
      if node.get_attribute_ref("id") == Some(id) {
        return node;
      }
      for child in node.children.iter().rev() {
        stack.push(child);
      }
    }
    panic!("missing id={id}");
  }

  #[test]
  fn autofocus_respects_disabled_fieldset_first_legend_exception() {
    let dom = crate::dom::parse_html(
      "<html><body><fieldset disabled>\
         <input id=\"a\" autofocus>\
         <legend><input id=\"b\" autofocus></legend>\
       </fieldset></body></html>",
    )
    .expect("parse");

    let ids = crate::dom::enumerate_dom_ids(&dom);
    let input_a = find_node_by_id(&dom, "a");
    let input_b = find_node_by_id(&dom, "b");
    let id_a = *ids.get(&(input_a as *const DomNode)).expect("id a");
    let id_b = *ids.get(&(input_b as *const DomNode)).expect("id b");
    assert_ne!(id_a, id_b);

    assert_eq!(autofocus_target_node_id(&dom), Some(id_b));
    let state = interaction_state_for_autofocus(&dom).expect("state");
    assert_eq!(state.focused, Some(id_b));
  }

  #[test]
  fn autofocus_does_not_treat_disabled_fieldset_as_inert_for_tabindex_elements() {
    let dom = crate::dom::parse_html(
      "<html><body><fieldset disabled><div id=\"d\" tabindex=\"0\" autofocus></div></fieldset></body></html>",
    )
    .expect("parse");

    let ids = crate::dom::enumerate_dom_ids(&dom);
    let div = find_node_by_id(&dom, "d");
    let div_id = *ids.get(&(div as *const DomNode)).expect("div id");

    assert_eq!(autofocus_target_node_id(&dom), Some(div_id));
    let state = interaction_state_for_autofocus(&dom).expect("state");
    assert_eq!(state.focused, Some(div_id));
  }

  #[test]
  fn autofocus_ignores_controls_disabled_by_fieldset() {
    let dom = crate::dom::parse_html(
      "<html><body><fieldset disabled><input id=\"a\" autofocus></fieldset></body></html>",
    )
    .expect("parse");

    assert_eq!(autofocus_target_node_id(&dom), None);
    assert!(interaction_state_for_autofocus(&dom).is_none());
  }
}
