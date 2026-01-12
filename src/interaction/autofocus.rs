use crate::dom::DomNode;
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
  // MVP heuristic (mirrors interaction engine): treat `disabled` as pruning focusability for the
  // subtree. This is important for `<fieldset disabled>` without needing special-casing.
  node.get_attribute_ref("disabled").is_some()
}

fn is_focusable_element_for_autofocus(node: &DomNode) -> bool {
  if !node.is_element() {
    return false;
  }
  if node.get_attribute_ref("disabled").is_some() {
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

/// Build an [`InteractionState`] reflecting initial autofocus selection, if any.
///
/// This is a best-effort approximation of HTML's autofocus behavior that enables correct `:focus`
/// selector matching (and related paint effects such as caret/selection rendering) for static
/// renders.
///
/// Returns `None` when no eligible autofocus element is present.
pub fn interaction_state_for_autofocus(dom: &DomNode) -> Option<InteractionState> {
  struct Frame<'a> {
    node: &'a DomNode,
    parent_id: usize,
    inert: bool,
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
  }];
  while let Some(frame) = stack.pop() {
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
      && is_focusable_element_for_autofocus(frame.node)
    {
      focused_id = Some(id);
    }

    for child in frame.node.children.iter().rev() {
      stack.push(Frame {
        node: child,
        parent_id: id,
        inert: self_inert,
      });
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
  struct Frame<'a> {
    node: &'a DomNode,
    inert: bool,
  }
  let mut next_id = 1usize;
  let mut stack = vec![Frame { node: dom, inert: false }];
  while let Some(frame) = stack.pop() {
    let id = next_id;
    next_id = next_id.saturating_add(1);
    let self_inert = frame.inert || node_self_is_inert_like(frame.node);
    if frame.node.is_element()
      && !self_inert
      && frame.node.get_attribute_ref("autofocus").is_some()
      && is_focusable_element_for_autofocus(frame.node)
    {
      return Some(id);
    }

    for child in frame.node.children.iter().rev() {
      stack.push(Frame {
        node: child,
        inert: self_inert,
      });
    }
  }

  None
}
