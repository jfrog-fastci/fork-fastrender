use crate::dom::DomNode;
use crate::dom::ElementRef;
use crate::interaction::InteractionState;

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
        !href.is_empty()
          && !href
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

fn node_self_is_inert_like(node: &DomNode) -> bool {
  // Template contents are always inert.
  if node.template_contents_are_inert() {
    return true;
  }
  // `hidden` is render- and interaction-suppressing.
  if node.get_attribute_ref("hidden").is_some() {
    return true;
  }
  // Native inert subtree handling.
  if node.get_attribute_ref("inert").is_some() {
    return true;
  }
  // Internal inert propagation for dialogs/popovers.
  if node
    .get_attribute_ref("data-fastr-inert")
    .is_some_and(|v| v.eq_ignore_ascii_case("true"))
  {
    return true;
  }
  false
}

fn is_focusable_element_for_autofocus(node: &DomNode, disabled: bool) -> bool {
  if !node.is_element() {
    return false;
  }
  if disabled {
    return false;
  }
  if node.get_attribute_ref("hidden").is_some() {
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

fn node_is_disabled(node: &DomNode, ancestors: &[&DomNode]) -> bool {
  ElementRef::with_ancestors(node, ancestors).accessibility_disabled()
}

/// Build an [`InteractionState`] reflecting initial autofocus selection, if any.
///
/// This is a best-effort approximation of HTML's autofocus behavior that enables correct `:focus`
/// selector matching (and related paint effects such as caret/selection rendering) for static
/// renders.
///
/// Returns `None` when no eligible autofocus element is present.
pub fn interaction_state_for_autofocus(dom: &DomNode) -> Option<InteractionState> {
  #[derive(Clone, Copy)]
  enum TraversalState {
    Enter,
    Exit,
  }
  struct Frame<'a> {
    node: &'a DomNode,
    parent_id: usize,
    inert: bool,
    state: TraversalState,
  }

  // We need stable pre-order ids matching `crate::dom::enumerate_dom_ids`. Keep a lightweight
  // (parent, is_element) table so we can later produce the `focus_chain` expected by selector
  // matching.
  let mut parent: Vec<usize> = vec![0];
  let mut is_element: Vec<bool> = vec![false];

  let mut focused_id: Option<usize> = None;
  let mut next_id = 1usize;
  let mut stack = vec![Frame {
    node: dom,
    parent_id: 0,
    inert: false,
    state: TraversalState::Enter,
  }];
  let mut ancestors: Vec<&DomNode> = Vec::new();
  while let Some(frame) = stack.pop() {
    match frame.state {
      TraversalState::Enter => {
        let id = next_id;
        next_id = next_id.saturating_add(1);
        parent.push(frame.parent_id);
        let self_is_element = frame.node.is_element();
        is_element.push(self_is_element);

        let self_inert = frame.inert || node_self_is_inert_like(frame.node);

        if focused_id.is_none()
          && self_is_element
          && !self_inert
          && frame.node.get_attribute_ref("autofocus").is_some()
        {
          let disabled = node_is_disabled(frame.node, &ancestors);
          if is_focusable_element_for_autofocus(frame.node, disabled) {
            focused_id = Some(id);
          }
        }

        stack.push(Frame {
          node: frame.node,
          parent_id: 0,
          inert: false,
          state: TraversalState::Exit,
        });
        for child in frame.node.children.iter().rev() {
          stack.push(Frame {
            node: child,
            parent_id: id,
            inert: self_inert,
            state: TraversalState::Enter,
          });
        }
        ancestors.push(frame.node);
      }
      TraversalState::Exit => {
        ancestors.pop();
      }
    }
  }

  let focused_id = focused_id?;

  let mut focus_chain = Vec::new();
  let mut current = focused_id;
  while current != 0 {
    if is_element.get(current).copied().unwrap_or(false) {
      focus_chain.push(current);
    }
    current = parent.get(current).copied().unwrap_or(0);
  }

  Some(InteractionState {
    focused: Some(focused_id),
    // Autofocus is not pointer-driven. Err on the side of matching `:focus-visible` as well,
    // which aligns with typical browser behavior for initially focused text controls.
    focus_visible: true,
    focus_chain,
    ..InteractionState::default()
  })
}

/// Returns the pre-order DOM node id of the first eligible `[autofocus]` element, if any.
///
/// This shares the same best-effort eligibility rules as [`interaction_state_for_autofocus`], but
/// only returns the node id. This is intended for interactive/browser UI integrations that manage
/// their own [`crate::interaction::InteractionEngine`] state but still want spec-ish autofocus
/// target selection.
pub fn autofocus_target_node_id(dom: &DomNode) -> Option<usize> {
  #[derive(Clone, Copy)]
  enum TraversalState {
    Enter,
    Exit,
  }
  struct Frame<'a> {
    node: &'a DomNode,
    inert: bool,
    state: TraversalState,
  }
  let mut next_id = 1usize;
  let mut stack = vec![Frame {
    node: dom,
    inert: false,
    state: TraversalState::Enter,
  }];
  let mut ancestors: Vec<&DomNode> = Vec::new();
  while let Some(frame) = stack.pop() {
    match frame.state {
      TraversalState::Enter => {
        let id = next_id;
        next_id = next_id.saturating_add(1);
        let self_inert = frame.inert || node_self_is_inert_like(frame.node);

        if frame.node.is_element()
          && !self_inert
          && frame.node.get_attribute_ref("autofocus").is_some()
        {
          let disabled = node_is_disabled(frame.node, &ancestors);
          if is_focusable_element_for_autofocus(frame.node, disabled) {
            return Some(id);
          }
        }

        stack.push(Frame {
          node: frame.node,
          inert: false,
          state: TraversalState::Exit,
        });
        for child in frame.node.children.iter().rev() {
          stack.push(Frame {
            node: child,
            inert: self_inert,
            state: TraversalState::Enter,
          });
        }
        ancestors.push(frame.node);
      }
      TraversalState::Exit => {
        ancestors.pop();
      }
    }
  }

  None
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
