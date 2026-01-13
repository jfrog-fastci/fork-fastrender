use crate::dom2::{Document, NodeId, NodeKind};
use super::effective_disabled_dom2;

/// Interaction engine for `dom2::Document`-backed browsing mode.
///
/// This is a lightweight sibling of the legacy [`crate::interaction::InteractionEngine`] that
/// operates directly on the live `dom2` DOM tree. The engine is responsible for:
/// - tracking non-DOM-visible interaction state (e.g. focus), and
/// - performing UA default actions (e.g. `<details>/<summary>` toggling) when user activation is
///   not canceled.
#[derive(Debug, Default, Clone)]
pub struct InteractionEngineDom2 {
  focused: Option<NodeId>,
}

impl InteractionEngineDom2 {
  #[inline]
  pub fn new() -> Self {
    Self::default()
  }

  #[inline]
  pub fn focused(&self) -> Option<NodeId> {
    self.focused
  }

  /// Apply the UA default action for a trusted primary click at `target`.
  ///
  /// This should be called only after the corresponding `"click"` event's default has **not**
  /// been prevented.
  pub fn activate_primary_click(&mut self, dom: &mut Document, target: NodeId) -> bool {
    // Determine the focusable click target first; this is used for focus updates and matches the
    // legacy behaviour where nested interactive content inside `<summary>` (e.g. `<a>`) takes focus.
    let click_focus_target = nearest_focusable_interactive_element(dom, target);

    let mut changed = false;

    // Update focus: if the click did not land on any focusable element, but it happened inside a
    // details summary, focus the summary (button-like).
    if let Some(focus_target) = click_focus_target {
      if self.focused != Some(focus_target) {
        self.focused = Some(focus_target);
        changed = true;
      }
    } else if let Some((summary_id, _)) = nearest_details_summary(dom, target) {
      if !node_or_ancestor_is_inert_hidden_or_disabled(dom, summary_id) {
        if self.focused != Some(summary_id) {
          self.focused = Some(summary_id);
          changed = true;
        }
      }
    }

    // Apply `<details>/<summary>` default toggle behaviour.
    if let Some((summary_id, details_id)) = nearest_details_summary(dom, target) {
      if !node_or_ancestor_is_inert_hidden_or_disabled(dom, summary_id) {
        changed |= toggle_details_open(dom, details_id);
      }
    }

    changed
  }
}

fn is_element_with_tag(dom: &Document, node_id: NodeId, tag: &str) -> bool {
  let node = dom.node(node_id);
  match &node.kind {
    NodeKind::Element { tag_name, .. } => tag_name.eq_ignore_ascii_case(tag),
    _ => false,
  }
}

fn is_summary(dom: &Document, node_id: NodeId) -> bool {
  is_element_with_tag(dom, node_id, "summary")
}

fn is_details(dom: &Document, node_id: NodeId) -> bool {
  is_element_with_tag(dom, node_id, "details")
}

/// Returns `Some(details_id)` if `summary` is the *details summary* for its parent `<details>`.
///
/// A details summary is:
/// - a `<summary>` element
/// - whose parent is a `<details>` element
/// - and which is the *first* `<summary>` element child of that `<details>`.
pub fn details_owner_for_summary(dom: &Document, summary: NodeId) -> Option<NodeId> {
  if !is_summary(dom, summary) {
    return None;
  }

  let details_id = dom.node(summary).parent?;
  if !is_details(dom, details_id) {
    return None;
  }

  // Find the first `<summary>` element child in DOM order (ignore nested summaries).
  let details = dom.node(details_id);
  for &child in &details.children {
    let child_node = dom.node(child);
    if child_node.parent != Some(details_id) {
      continue;
    }
    if is_summary(dom, child) {
      return (child == summary).then_some(details_id);
    }
  }

  None
}

/// Walk up the ancestor chain (including `start`) to find the nearest details summary.
///
/// Returns `(summary_id, details_id)` when found.
fn nearest_details_summary(dom: &Document, mut node_id: NodeId) -> Option<(NodeId, NodeId)> {
  loop {
    if let Some(details_id) = details_owner_for_summary(dom, node_id) {
      return Some((node_id, details_id));
    }
    node_id = dom.node(node_id).parent?;
  }
}

fn toggle_details_open(dom: &mut Document, details: NodeId) -> bool {
  if !is_details(dom, details) {
    return false;
  }
  let is_open = dom.has_attribute(details, "open").unwrap_or(false);
  dom
    .set_bool_attribute(details, "open", !is_open)
    .unwrap_or(false)
}

fn trim_ascii_whitespace(value: &str) -> &str {
  value.trim_matches(|c: char| matches!(c, '\u{0009}' | '\u{000A}' | '\u{000C}' | '\u{000D}' | ' '))
}

fn node_or_ancestor_is_inert_hidden_or_disabled(dom: &Document, node_id: NodeId) -> bool {
  // Delegate to the shared dom2 implementation so we stay consistent with legacy interaction:
  // - `inert` / `<template>` contents behave as disconnected
  // - `hidden` is treated as not interactive
  // - `disabled` follows spec-correct fieldset rules for form controls, while still respecting a
  //   `disabled` attribute on the element itself (common for custom controls).
  effective_disabled_dom2::is_effectively_inert(node_id, dom)
    || effective_disabled_dom2::is_effectively_hidden(node_id, dom)
    || effective_disabled_dom2::is_effectively_disabled(node_id, dom)
}

fn parse_tabindex(dom: &Document, node_id: NodeId) -> Option<i32> {
  let raw = dom.get_attribute(node_id, "tabindex").ok().flatten()?;
  let raw = trim_ascii_whitespace(raw);
  if raw.is_empty() {
    return None;
  }
  raw.parse::<i32>().ok()
}

fn is_anchor_with_href(dom: &Document, node_id: NodeId) -> bool {
  let node = dom.node(node_id);
  let tag_name = match &node.kind {
    NodeKind::Element { tag_name, .. } => tag_name.as_str(),
    _ => return false,
  };
  if !(tag_name.eq_ignore_ascii_case("a") || tag_name.eq_ignore_ascii_case("area")) {
    return false;
  }
  let Some(href) = dom.get_attribute(node_id, "href").ok().flatten() else {
    return false;
  };
  let href = trim_ascii_whitespace(href);
  if href.is_empty() {
    return false;
  }
  // The browser UI doesn't execute JS, so `javascript:` URLs aren't meaningful navigation targets.
  if href
    .as_bytes()
    .get(.."javascript:".len())
    .is_some_and(|prefix| prefix.eq_ignore_ascii_case(b"javascript:"))
  {
    return false;
  }
  true
}

fn is_input(dom: &Document, node_id: NodeId) -> bool {
  is_element_with_tag(dom, node_id, "input")
}

fn is_textarea(dom: &Document, node_id: NodeId) -> bool {
  is_element_with_tag(dom, node_id, "textarea")
}

fn is_select(dom: &Document, node_id: NodeId) -> bool {
  is_element_with_tag(dom, node_id, "select")
}

fn is_button(dom: &Document, node_id: NodeId) -> bool {
  is_element_with_tag(dom, node_id, "button")
}

fn input_type(dom: &Document, node_id: NodeId) -> &str {
  dom
    .get_attribute(node_id, "type")
    .ok()
    .flatten()
    .map(trim_ascii_whitespace)
    .filter(|v| !v.is_empty())
    .unwrap_or("text")
}

/// MVP focusable predicate for pointer focus / blur decisions.
///
/// Mirrors the legacy `InteractionEngine` heuristic: native interactive elements + `tabindex`.
fn is_focusable_interactive_element(dom: &Document, node_id: NodeId) -> bool {
  if node_or_ancestor_is_inert_hidden_or_disabled(dom, node_id) {
    return false;
  }

  // HTML tabindex support: any parsed `tabindex` makes the element focusable via pointer and
  // programmatic focus, even when `tabindex < 0`.
  if parse_tabindex(dom, node_id).is_some() {
    // `input type=hidden` is never focusable, even if tabindex is set.
    if is_input(dom, node_id) && input_type(dom, node_id).eq_ignore_ascii_case("hidden") {
      return false;
    }
    return true;
  }

  if is_anchor_with_href(dom, node_id) {
    return true;
  }

  if details_owner_for_summary(dom, node_id).is_some() {
    return true;
  }

  if is_input(dom, node_id) {
    return !input_type(dom, node_id).eq_ignore_ascii_case("hidden");
  }

  is_textarea(dom, node_id) || is_select(dom, node_id) || is_button(dom, node_id)
}

fn nearest_focusable_interactive_element(dom: &Document, mut node_id: NodeId) -> Option<NodeId> {
  loop {
    if is_focusable_interactive_element(dom, node_id) {
      return Some(node_id);
    }
    node_id = dom.node(node_id).parent?;
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn clicking_first_summary_toggles_details_open() {
    let mut dom = crate::dom2::parse_html(
      "<!doctype html>\
       <details id=d>\
         <summary id=s1>One</summary>\
         <summary id=s2>Two</summary>\
         <div>Body</div>\
       </details>",
    )
    .unwrap();
    let details = dom.get_element_by_id("d").unwrap();
    let summary = dom.get_element_by_id("s1").unwrap();

    assert!(!dom.has_attribute(details, "open").unwrap());

    let mut engine = InteractionEngineDom2::new();
    assert!(engine.activate_primary_click(&mut dom, summary));

    assert!(dom.has_attribute(details, "open").unwrap());
  }

  #[test]
  fn clicking_non_first_summary_does_not_toggle_details() {
    let mut dom = crate::dom2::parse_html(
      "<!doctype html>\
       <details id=d>\
         <summary id=s1>One</summary>\
         <summary id=s2>Two</summary>\
         <div>Body</div>\
       </details>",
    )
    .unwrap();
    let details = dom.get_element_by_id("d").unwrap();
    let summary = dom.get_element_by_id("s2").unwrap();

    assert!(!dom.has_attribute(details, "open").unwrap());

    let mut engine = InteractionEngineDom2::new();
    // Non-first summaries are not the "details summary", so they should not toggle.
    assert!(!engine.activate_primary_click(&mut dom, summary));

    assert!(!dom.has_attribute(details, "open").unwrap());
  }
}
