use crate::dom2::{Document, NodeId, NodeKind};

fn is_html_element_tag(dom: &Document, node: NodeId, tag: &str) -> bool {
  let NodeKind::Element {
    tag_name,
    namespace,
    ..
  } = &dom.node(node).kind
  else {
    return false;
  };
  dom.is_html_case_insensitive_namespace(namespace) && tag_name.eq_ignore_ascii_case(tag)
}

fn trim_ascii_whitespace(value: &str) -> &str {
  value.trim_matches(|c: char| matches!(c, '\u{0009}' | '\u{000A}' | '\u{000C}' | '\u{000D}' | ' '))
}

fn parse_finite_number(value: &str) -> Option<f64> {
  trim_ascii_whitespace(value)
    .parse::<f64>()
    .ok()
    .filter(|v| v.is_finite())
}

fn is_input_of_type(dom: &Document, input: NodeId, ty: &str) -> bool {
  if !is_html_element_tag(dom, input, "input") {
    return false;
  }
  dom
    .get_attribute(input, "type")
    .ok()
    .flatten()
    .unwrap_or("text")
    .eq_ignore_ascii_case(ty)
}

fn effective_disabled_dom2(dom: &Document, node: NodeId) -> bool {
  super::effective_disabled_dom2::is_effectively_inert(node, dom)
    || super::effective_disabled_dom2::is_effectively_disabled(node, dom)
}

fn tree_root_boundary(dom: &Document, node: NodeId) -> NodeId {
  for ancestor in dom.ancestors(node) {
    if matches!(
      dom.node(ancestor).kind,
      NodeKind::Document { .. } | NodeKind::ShadowRoot { .. } | NodeKind::DocumentFragment
    ) {
      return ancestor;
    }
  }
  node
}

fn is_form_element(dom: &Document, node: NodeId) -> bool {
  is_html_element_tag(dom, node, "form")
}

fn form_owner(dom: &Document, node: NodeId) -> Option<NodeId> {
  let root_boundary = tree_root_boundary(dom, node);

  // Resolve `form="id"` if present and points to a <form> in the same tree-root boundary.
  let form_attr = dom
    .get_attribute(node, "form")
    .ok()
    .flatten()
    .map(trim_ascii_whitespace)
    .filter(|v| !v.is_empty())
    .map(str::to_string);

  if let Some(form_attr) = form_attr.as_deref() {
    if let Some(id) = dom.get_element_by_id_from(root_boundary, form_attr) {
      if is_form_element(dom, id) && tree_root_boundary(dom, id) == root_boundary {
        return Some(id);
      }
    }
  }

  // Nearest ancestor <form>, stopping at the tree-root boundary.
  for ancestor in dom.ancestors(node).skip(1) {
    if ancestor == root_boundary {
      break;
    }
    if is_form_element(dom, ancestor) {
      return Some(ancestor);
    }
  }

  None
}

pub fn toggle_checkbox(dom: &mut Document, input: NodeId) -> bool {
  if !is_input_of_type(dom, input, "checkbox") {
    return false;
  }
  if effective_disabled_dom2(dom, input) {
    return false;
  }

  let was_checked = dom.input_checked(input).ok().unwrap_or(false);
  dom.set_input_checked(input, !was_checked).is_ok()
}

pub fn activate_radio(dom: &mut Document, radio: NodeId) -> bool {
  if !is_input_of_type(dom, radio, "radio") {
    return false;
  }
  if effective_disabled_dom2(dom, radio) {
    return false;
  }

  // HTML radio group membership depends on:
  // - the `name` value (missing treated as the empty string), and
  // - the element's form owner (or tree root if there is no form owner).
  let group_name = dom
    .get_attribute(radio, "name")
    .ok()
    .flatten()
    .unwrap_or("")
    .to_string();

  let active_form = form_owner(dom, radio);
  let active_root = active_form
    .is_none()
    .then(|| tree_root_boundary(dom, radio));

  let mut changed = false;

  if dom.input_checked(radio).ok().is_some_and(|v| !v) {
    if dom.set_input_checked(radio, true).is_err() {
      return false;
    }
    changed = true;
  }

  for idx in 0..dom.nodes_len() {
    let id = NodeId::from_index(idx);
    if id == radio {
      continue;
    }

    if !is_input_of_type(dom, id, "radio") {
      continue;
    }

    let candidate_name = dom.get_attribute(id, "name").ok().flatten().unwrap_or("");
    if candidate_name != group_name {
      continue;
    }

    let owner = form_owner(dom, id);
    if let Some(active_form) = active_form {
      if owner != Some(active_form) {
        continue;
      }
    } else {
      if owner.is_some() {
        continue;
      }
      if tree_root_boundary(dom, id) != active_root.unwrap() { // fastrender-allow-unwrap
        continue;
      }
    }

    if dom.input_checked(id).ok().unwrap_or(false) {
      if dom.set_input_checked(id, false).is_err() {
        continue;
      }
      changed = true;
    }
  }

  changed
}

fn range_bounds(dom: &Document, input: NodeId) -> Option<(f64, f64)> {
  if !is_input_of_type(dom, input, "range") {
    return None;
  }

  let min = dom
    .get_attribute(input, "min")
    .ok()
    .flatten()
    .and_then(parse_finite_number)
    .unwrap_or(0.0);
  let max = dom
    .get_attribute(input, "max")
    .ok()
    .flatten()
    .and_then(parse_finite_number)
    .unwrap_or(100.0);

  let clamped_max = if max < min { min } else { max };
  Some((min, clamped_max))
}

fn sanitize_range_value(dom: &Document, input: NodeId, value: f64) -> Option<f64> {
  let (min, max) = range_bounds(dom, input)?;
  if !value.is_finite() {
    return None;
  }

  let clamped = value.clamp(min, max);

  let step_attr = dom.get_attribute(input, "step").ok().flatten();
  if matches!(
    step_attr,
    Some(step) if trim_ascii_whitespace(step).eq_ignore_ascii_case("any")
  ) {
    return Some(clamped);
  }

  let step = step_attr
    .and_then(parse_finite_number)
    .filter(|step| *step > 0.0)
    .unwrap_or(1.0);

  // The allowed value step base for range inputs is the minimum value (defaulting to zero).
  let step_base = min;
  let steps_to_value = ((clamped - step_base) / step).round();
  let mut aligned = step_base + steps_to_value * step;

  let max_aligned = step_base + ((max - step_base) / step).floor() * step;
  if aligned > max_aligned {
    aligned = max_aligned;
  }
  if aligned < step_base {
    aligned = step_base;
  }

  Some(aligned.clamp(min, max))
}

pub fn set_range_value_from_ratio(dom: &mut Document, input: NodeId, ratio: f32) -> bool {
  if !is_input_of_type(dom, input, "range") {
    return false;
  }
  if effective_disabled_dom2(dom, input) || dom.has_attribute(input, "readonly").unwrap_or(false) {
    return false;
  }
  if !ratio.is_finite() {
    return false;
  }

  let Some((min, max)) = range_bounds(dom, input) else {
    return false;
  };
  let ratio = (ratio as f64).clamp(0.0, 1.0);
  let value = min + (max - min) * ratio;
  let Some(sanitized) = sanitize_range_value(dom, input, value) else {
    return false;
  };

  let value_str = crate::dom::format_number(sanitized);
  if dom.input_value(input).ok().is_some_and(|v| v == value_str) {
    return false;
  }

  dom.set_input_value(input, &value_str).is_ok()
}

pub fn activate_select_option(
  dom: &mut Document,
  select: NodeId,
  option: NodeId,
  toggle_multi: bool,
) -> bool {
  if effective_disabled_dom2(dom, select) {
    return false;
  }

  if !is_html_element_tag(dom, select, "select") {
    return false;
  }
  let select_multiple = dom.has_attribute(select, "multiple").unwrap_or(false);

  if !is_html_element_tag(dom, option, "option") {
    return false;
  }
  if dom.has_attribute(option, "disabled").unwrap_or(false) {
    return false;
  }

  // Verify `option` is a descendant of `select` and that no disabled `<optgroup>` exists between
  // them.
  let mut found_select = false;
  for ancestor in dom.ancestors(option).skip(1) {
    if ancestor == select {
      found_select = true;
      break;
    }
    if is_html_element_tag(dom, ancestor, "optgroup")
      && dom.has_attribute(ancestor, "disabled").unwrap_or(false)
    {
      return false;
    }
  }
  if !found_select {
    return false;
  }

  let option_selected = dom.option_selected(option).ok().unwrap_or(false);

  if select_multiple && toggle_multi {
    return dom.set_option_selected(option, !option_selected).is_ok();
  }

  // Replacement selection (single-select and non-toggle multiple-select).
  if !select_multiple && option_selected {
    // Spec-ish: activating an already-selected option in single-select is a no-op.
    return false;
  }

  if !select_multiple {
    // Single-select: `set_option_selected` enforces mutual exclusion within the <select>.
    return dom.set_option_selected(option, true).is_ok();
  }

  // Multiple-select replacement selection: clear selectedness from other options and ensure the
  // target is selected.
  let options = dom.select_options(select);
  let mut changed = false;
  for other in options {
    if other == option {
      continue;
    }
    if dom.option_selected(other).ok().unwrap_or(false) {
      if dom.set_option_selected(other, false).is_ok() {
        changed = true;
      }
    }
  }

  if !option_selected {
    if dom.set_option_selected(option, true).is_err() {
      return false;
    }
    changed = true;
  }

  changed
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn checkbox_toggles_checkedness_without_mutating_content_attribute() {
    let mut dom = crate::dom2::parse_html(r#"<input id="c" type="checkbox">"#).unwrap();
    let c = dom.get_element_by_id("c").unwrap();

    assert!(!dom.input_checked(c).unwrap());
    assert!(!dom.has_attribute(c, "checked").unwrap());

    assert!(toggle_checkbox(&mut dom, c));
    assert!(dom.input_checked(c).unwrap());
    assert!(
      !dom.has_attribute(c, "checked").unwrap(),
      "checkedness should be internal state, not the `checked` content attribute"
    );

    assert!(toggle_checkbox(&mut dom, c));
    assert!(!dom.input_checked(c).unwrap());
    assert!(!dom.has_attribute(c, "checked").unwrap());
  }

  #[test]
  fn radio_activation_unchecks_others_in_same_form_owner() {
    let mut dom = crate::dom2::parse_html(
      r#"<!doctype html>
      <form id="f1">
        <input id="r1" type="radio" name="g" checked>
        <input id="r2" type="radio" name="g">
      </form>
      <form id="f2">
        <input id="r3" type="radio" name="g" checked>
        <input id="r4" type="radio" name="g">
      </form>"#,
    )
    .unwrap();

    let r1 = dom.get_element_by_id("r1").unwrap();
    let r2 = dom.get_element_by_id("r2").unwrap();
    let r3 = dom.get_element_by_id("r3").unwrap();
    let r4 = dom.get_element_by_id("r4").unwrap();

    assert!(dom.input_checked(r1).unwrap());
    assert!(!dom.input_checked(r2).unwrap());
    assert!(dom.input_checked(r3).unwrap());
    assert!(!dom.input_checked(r4).unwrap());

    assert!(activate_radio(&mut dom, r2));
    assert!(!dom.input_checked(r1).unwrap());
    assert!(dom.input_checked(r2).unwrap());
    assert!(dom.input_checked(r3).unwrap());
    assert!(!dom.input_checked(r4).unwrap());

    // Content attributes should remain authored.
    assert!(dom.has_attribute(r1, "checked").unwrap());
    assert!(!dom.has_attribute(r2, "checked").unwrap());
    assert!(dom.has_attribute(r3, "checked").unwrap());
    assert!(!dom.has_attribute(r4, "checked").unwrap());
  }

  #[test]
  fn select_activation_respects_multiple_and_does_not_mutate_selected_attribute() {
    let mut dom = crate::dom2::parse_html(
      r#"<!doctype html>
      <select id="s">
        <option id="o1" selected>One</option>
        <option id="o2">Two</option>
      </select>
      <select id="m" multiple>
        <option id="m1" selected>One</option>
        <option id="m2">Two</option>
      </select>"#,
    )
    .unwrap();

    let s = dom.get_element_by_id("s").unwrap();
    let o1 = dom.get_element_by_id("o1").unwrap();
    let o2 = dom.get_element_by_id("o2").unwrap();

    let m = dom.get_element_by_id("m").unwrap();
    let m1 = dom.get_element_by_id("m1").unwrap();
    let m2 = dom.get_element_by_id("m2").unwrap();

    assert!(dom.option_selected(o1).unwrap());
    assert!(!dom.option_selected(o2).unwrap());
    assert!(dom.has_attribute(o1, "selected").unwrap());
    assert!(!dom.has_attribute(o2, "selected").unwrap());

    assert!(activate_select_option(&mut dom, s, o2, false));
    assert!(!dom.option_selected(o1).unwrap());
    assert!(dom.option_selected(o2).unwrap());
    assert!(
      dom.has_attribute(o1, "selected").unwrap(),
      "content attribute should remain authored"
    );
    assert!(!dom.has_attribute(o2, "selected").unwrap());

    // Multiple-select toggle.
    assert!(dom.option_selected(m1).unwrap());
    assert!(!dom.option_selected(m2).unwrap());
    assert!(activate_select_option(&mut dom, m, m2, true));
    assert!(dom.option_selected(m1).unwrap());
    assert!(dom.option_selected(m2).unwrap());
    assert!(activate_select_option(&mut dom, m, m2, true));
    assert!(dom.option_selected(m1).unwrap());
    assert!(!dom.option_selected(m2).unwrap());
  }

  #[test]
  fn range_ratio_clamps_and_aligns_to_step_without_mutating_value_attribute() {
    let mut dom =
      crate::dom2::parse_html(r#"<input id="r" type="range" min="0" max="10" step="3">"#).unwrap();
    let r = dom.get_element_by_id("r").unwrap();

    assert_eq!(dom.get_attribute(r, "value").unwrap(), None);

    // Ratio is clamped to [0, 1] and aligned to the nearest allowed step.
    assert!(set_range_value_from_ratio(&mut dom, r, 0.5));
    assert_eq!(dom.input_value(r).unwrap(), "6");
    assert_eq!(dom.get_attribute(r, "value").unwrap(), None);

    assert!(set_range_value_from_ratio(&mut dom, r, 1.0));
    assert_eq!(dom.input_value(r).unwrap(), "9");
    assert_eq!(dom.get_attribute(r, "value").unwrap(), None);

    assert!(set_range_value_from_ratio(&mut dom, r, -1.0));
    assert_eq!(dom.input_value(r).unwrap(), "0");
    assert_eq!(dom.get_attribute(r, "value").unwrap(), None);
  }
}
