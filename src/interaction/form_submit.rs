use crate::dom::{DomNode, DomNodeType, HTML_NAMESPACE, SVG_NAMESPACE};
use crate::resource::web_url::{WebUrlLimits, WebUrlSearchParams};

use url::Url;

fn trim_ascii_whitespace(value: &str) -> &str {
  // HTML URL-ish attributes strip leading/trailing ASCII whitespace (TAB/LF/FF/CR/SPACE) but do not
  // treat all Unicode whitespace as ignorable. Use an explicit trim to avoid incorrectly dropping
  // characters like NBSP (U+00A0).
  value.trim_matches(|c: char| matches!(c, '\u{0009}' | '\u{000A}' | '\u{000C}' | '\u{000D}' | ' '))
}

fn resolve_url(base_url: &str, href: &str) -> Option<String> {
  let href = trim_ascii_whitespace(href);
  if href.is_empty() {
    return None;
  }
  if href
    .as_bytes()
    .get(.."javascript:".len())
    .is_some_and(|prefix| prefix.eq_ignore_ascii_case(b"javascript:"))
  {
    return None;
  }

  if let Ok(base) = Url::parse(base_url) {
    if let Ok(joined) = base.join(href) {
      if joined.scheme().eq_ignore_ascii_case("javascript") {
        return None;
      }
      return Some(joined.to_string());
    }
  }

  let absolute = Url::parse(href).ok()?;
  (!absolute.scheme().eq_ignore_ascii_case("javascript")).then(|| absolute.to_string())
}

struct DomIndex<'a> {
  id_to_node: Vec<*const DomNode>,
  parent: Vec<usize>,
  _root: std::marker::PhantomData<&'a DomNode>,
}

impl<'a> DomIndex<'a> {
  fn new(root: &'a DomNode) -> Self {
    let mut id_to_node: Vec<*const DomNode> = vec![std::ptr::null()];
    let mut parent: Vec<usize> = vec![0];

    // (node_ptr, parent_id)
    let mut stack: Vec<(*const DomNode, usize)> = vec![(root as *const DomNode, 0)];

    while let Some((ptr, parent_id)) = stack.pop() {
      let id = id_to_node.len();
      id_to_node.push(ptr);
      parent.push(parent_id);

      // SAFETY: pointers are built from a live `DomNode` tree.
      let node = unsafe { &*ptr };

      for child in node.children.iter().rev() {
        stack.push((child as *const DomNode, id));
      }
    }

    Self {
      id_to_node,
      parent,
      _root: std::marker::PhantomData,
    }
  }

  fn len(&self) -> usize {
    self.id_to_node.len().saturating_sub(1)
  }

  fn node(&self, node_id: usize) -> Option<&'a DomNode> {
    let ptr = *self.id_to_node.get(node_id)?;
    if ptr.is_null() {
      return None;
    }
    // SAFETY: pointers are built from a live `DomNode` tree.
    Some(unsafe { &*ptr })
  }
}

fn is_form(node: &DomNode) -> bool {
  node
    .tag_name()
    .is_some_and(|tag| tag.eq_ignore_ascii_case("form"))
}

fn is_input(node: &DomNode) -> bool {
  node
    .tag_name()
    .is_some_and(|tag| tag.eq_ignore_ascii_case("input"))
}

fn is_textarea(node: &DomNode) -> bool {
  node
    .tag_name()
    .is_some_and(|tag| tag.eq_ignore_ascii_case("textarea"))
}

fn is_select(node: &DomNode) -> bool {
  node
    .tag_name()
    .is_some_and(|tag| tag.eq_ignore_ascii_case("select"))
}

fn is_button(node: &DomNode) -> bool {
  node
    .tag_name()
    .is_some_and(|tag| tag.eq_ignore_ascii_case("button"))
}

fn input_type(node: &DomNode) -> &str {
  node
    .get_attribute_ref("type")
    .map(trim_ascii_whitespace)
    .filter(|v| !v.is_empty())
    .unwrap_or("text")
}

fn button_type(node: &DomNode) -> &str {
  // HTML <button> defaults to submit.
  node
    .get_attribute_ref("type")
    .map(trim_ascii_whitespace)
    .filter(|v| !v.is_empty())
    .unwrap_or("submit")
}

fn is_submit_control(node: &DomNode) -> bool {
  (is_input(node) && input_type(node).eq_ignore_ascii_case("submit"))
    || (is_button(node) && button_type(node).eq_ignore_ascii_case("submit"))
}

fn node_is_inert_like(node: &DomNode) -> bool {
  if node.get_attribute_ref("inert").is_some() {
    return true;
  }
  node
    .get_attribute_ref("data-fastr-inert")
    .is_some_and(|v| v.eq_ignore_ascii_case("true"))
}

fn is_disabled_or_inert(index: &DomIndex<'_>, mut node_id: usize) -> bool {
  while node_id != 0 {
    let Some(node) = index.node(node_id) else {
      return false;
    };

    if node.get_attribute_ref("disabled").is_some() {
      return true;
    }
    if node_is_inert_like(node) {
      return true;
    }
    if node.is_template_element() {
      return true;
    }

    node_id = *index.parent.get(node_id).unwrap_or(&0);
  }

  false
}

fn find_ancestor_form(index: &DomIndex<'_>, mut node_id: usize) -> Option<usize> {
  while node_id != 0 {
    let node = index.node(node_id)?;
    if is_form(node) {
      return Some(node_id);
    }
    // Shadow roots are tree root boundaries for form owner resolution; do not walk out into the
    // shadow host tree.
    if matches!(node.node_type, DomNodeType::ShadowRoot { .. } | DomNodeType::Document { .. }) {
      break;
    }
    node_id = *index.parent.get(node_id).unwrap_or(&0);
  }
  None
}

fn tree_root_boundary_id(index: &DomIndex<'_>, mut node_id: usize) -> Option<usize> {
  while node_id != 0 {
    let node = index.node(node_id)?;
    if matches!(node.node_type, DomNodeType::Document { .. } | DomNodeType::ShadowRoot { .. }) {
      return Some(node_id);
    }
    node_id = *index.parent.get(node_id).unwrap_or(&0);
  }
  None
}

fn node_or_ancestor_is_template(index: &DomIndex<'_>, mut node_id: usize) -> bool {
  while node_id != 0 {
    let Some(node) = index.node(node_id) else {
      return false;
    };
    if node.is_template_element() {
      return true;
    }
    node_id = *index.parent.get(node_id).unwrap_or(&0);
  }
  false
}

fn find_element_by_id_attr_in_tree(
  index: &DomIndex<'_>,
  tree_root_id: usize,
  html_id: &str,
) -> Option<usize> {
  for node_id in 1..index.id_to_node.len() {
    let Some(node) = index.node(node_id) else {
      continue;
    };
    if !node.is_element() {
      continue;
    }
    if node_or_ancestor_is_template(index, node_id) {
      continue;
    }
    if node.get_attribute_ref("id") != Some(html_id) {
      continue;
    }
    if tree_root_boundary_id(index, node_id) == Some(tree_root_id) {
      return Some(node_id);
    }
  }
  None
}

fn resolve_form_owner(index: &DomIndex<'_>, control_node_id: usize) -> Option<usize> {
  let control = index.node(control_node_id)?;

  if let Some(form_attr) = control
    .get_attribute_ref("form")
    .map(trim_ascii_whitespace)
    .filter(|v| !v.is_empty())
  {
    let tree_root = tree_root_boundary_id(index, control_node_id)?;
    let referenced = find_element_by_id_attr_in_tree(index, tree_root, form_attr)?;
    return index.node(referenced).is_some_and(is_form).then_some(referenced);
  }

  find_ancestor_form(index, control_node_id)
}

fn collect_descendant_text_content(node: &DomNode) -> String {
  let mut text = String::new();
  let mut stack: Vec<&DomNode> = Vec::new();
  stack.push(node);

  while let Some(node) = stack.pop() {
    match &node.node_type {
      DomNodeType::Text { content } => text.push_str(content),
      DomNodeType::Element {
        tag_name,
        namespace,
        ..
      } => {
        if tag_name.eq_ignore_ascii_case("script")
          && (namespace.is_empty() || namespace == HTML_NAMESPACE || namespace == SVG_NAMESPACE)
        {
          continue;
        }
      }
      _ => {}
    }

    for child in node.children.iter().rev() {
      stack.push(child);
    }
  }

  text
}

fn option_value(node: &DomNode) -> String {
  if let Some(value) = node.get_attribute_ref("value") {
    return value.to_string();
  }
  crate::dom::strip_and_collapse_ascii_whitespace(&collect_descendant_text_content(node))
}

#[derive(Debug, Clone)]
struct SelectOption {
  value: String,
  selected: bool,
  disabled: bool,
}

fn collect_select_options(select: &DomNode) -> Vec<SelectOption> {
  let mut options = Vec::new();
  // (node, optgroup_disabled)
  let mut stack: Vec<(&DomNode, bool)> = Vec::new();
  for child in select.children.iter().rev() {
    stack.push((child, false));
  }

  while let Some((node, optgroup_disabled)) = stack.pop() {
    if node.is_template_element() {
      continue;
    }

    if let Some(tag) = node.tag_name() {
      if tag.eq_ignore_ascii_case("option") {
        let disabled = optgroup_disabled || node.get_attribute_ref("disabled").is_some();
        options.push(SelectOption {
          value: option_value(node),
          selected: node.get_attribute_ref("selected").is_some(),
          disabled,
        });
        continue;
      }

      if tag.eq_ignore_ascii_case("optgroup") {
        let disabled = optgroup_disabled || node.get_attribute_ref("disabled").is_some();
        for child in node.children.iter().rev() {
          stack.push((child, disabled));
        }
        continue;
      }
    }

    for child in node.children.iter().rev() {
      stack.push((child, optgroup_disabled));
    }
  }

  options
}

fn append_pair(params: &WebUrlSearchParams, name: &str, value: &str) -> Option<()> {
  params.append(name, value).ok()?;
  Some(())
}

fn append_form_controls(
  index: &DomIndex<'_>,
  form_node_id: usize,
  submitter_node_id: usize,
  params: &WebUrlSearchParams,
) -> Option<()> {
  // Spec-ish: successful controls are collected in tree order (document order), including form-
  // associated elements outside the `<form>` subtree.
  for node_id in 1..index.id_to_node.len() {
    let Some(node) = index.node(node_id) else {
      continue;
    };

    if !node.is_element() {
      continue;
    }

    // Skip disabled controls and inert subtrees.
    if is_disabled_or_inert(index, node_id) {
      continue;
    }

    if !(is_input(node) || is_textarea(node) || is_select(node)) {
      continue;
    }
    if resolve_form_owner(index, node_id) != Some(form_node_id) {
      continue;
    }

    let Some(name) = node
      .get_attribute_ref("name")
      .map(trim_ascii_whitespace)
      .filter(|name| !name.is_empty())
    else {
      continue;
    };

    if is_input(node) {
      let ty = input_type(node);

      if ty.eq_ignore_ascii_case("checkbox") || ty.eq_ignore_ascii_case("radio") {
        if node.get_attribute_ref("checked").is_none() {
          continue;
        }
        let value = node.get_attribute_ref("value").unwrap_or("on");
        append_pair(params, name, value)?;
        continue;
      }

      if ty.eq_ignore_ascii_case("submit") {
        // Only include the activated submitter.
        continue;
      }

      if ty.eq_ignore_ascii_case("button") || ty.eq_ignore_ascii_case("reset") {
        continue;
      }

      if ty.eq_ignore_ascii_case("file") || ty.eq_ignore_ascii_case("image") {
        continue;
      }

      if ty.eq_ignore_ascii_case("range") {
        let value = crate::dom::input_range_value(node)
          .map(crate::dom::format_number)
          .unwrap_or_else(|| node.get_attribute_ref("value").unwrap_or("").to_string());
        append_pair(params, name, &value)?;
        continue;
      }

      let value = if ty.eq_ignore_ascii_case("color") {
        crate::dom::input_color_value_string(node).unwrap_or_default()
      } else if ty.eq_ignore_ascii_case("number") {
        crate::dom::input_number_value_string(node).unwrap_or_default()
      } else if ty.eq_ignore_ascii_case("date") {
        crate::dom::input_date_value_string(node).unwrap_or_default()
      } else if ty.eq_ignore_ascii_case("time") {
        crate::dom::input_time_value_string(node).unwrap_or_default()
      } else if ty.eq_ignore_ascii_case("datetime-local") {
        crate::dom::input_datetime_local_value_string(node).unwrap_or_default()
      } else if ty.eq_ignore_ascii_case("month") {
        crate::dom::input_month_value_string(node).unwrap_or_default()
      } else if ty.eq_ignore_ascii_case("week") {
        crate::dom::input_week_value_string(node).unwrap_or_default()
      } else {
        crate::dom::input_text_like_value_string(node)
          .unwrap_or_else(|| node.get_attribute_ref("value").unwrap_or("").to_string())
      };
      append_pair(params, name, &value)?;
      continue;
    }

    if is_textarea(node) {
      let value = crate::dom::textarea_current_value(node);
      append_pair(params, name, &value)?;
      continue;
    }

    if is_select(node) {
      let multiple = node.get_attribute_ref("multiple").is_some();
      let options = collect_select_options(node);

      if multiple {
        for option in options {
          if option.selected && !option.disabled {
            append_pair(params, name, &option.value)?;
          }
        }
      } else {
        let mut chosen: Option<usize> = None;
        for (idx, option) in options.iter().enumerate() {
          if option.selected && !option.disabled {
            chosen = Some(idx);
          }
        }

        if chosen.is_none() {
          for (idx, option) in options.iter().enumerate() {
            if !option.disabled {
              chosen = Some(idx);
              break;
            }
          }
        }

        if let Some(chosen) = chosen {
          if let Some(option) = options.get(chosen) {
            append_pair(params, name, &option.value)?;
          }
        }
      }

      continue;
    }

    if is_button(node) {
      // Buttons do not contribute to form data unless they are the submitter.
      continue;
    }
  }

  // Include submitter name/value pair if it has a name.
  let submitter = index.node(submitter_node_id)?;
  if is_disabled_or_inert(index, submitter_node_id) {
    return None;
  }
  if let Some(name) = submitter
    .get_attribute_ref("name")
    .filter(|name| !name.is_empty())
  {
    let value = submitter.get_attribute_ref("value").unwrap_or("");
    append_pair(params, name, value)?;
  }

  Some(())
}

/// Build a GET form submission URL for the given submit button/input.
///
/// MVP implementation:
/// - Finds the form owner using the submitter's `form` attribute when present, otherwise the
///   nearest `<form>` ancestor.
/// - Only supports method=GET.
/// - Serializes successful controls in tree order.
pub fn form_submission_get_url(
  dom: &DomNode,
  submitter_node_id: usize,
  document_url: &str,
  base_url: &str,
) -> Option<String> {
  let index = DomIndex::new(dom);

  let submitter = index.node(submitter_node_id)?;
  if !is_submit_control(submitter) {
    return None;
  }
  if is_disabled_or_inert(&index, submitter_node_id) {
    return None;
  }

  let form_id = resolve_form_owner(&index, submitter_node_id)?;
  let form = index.node(form_id)?;

  let method = trim_ascii_whitespace(form.get_attribute_ref("method").unwrap_or("get"));
  let method = if method.is_empty() { "get" } else { method };
  if !method.eq_ignore_ascii_case("get") {
    return None;
  }

  let action_url = match form
    .get_attribute_ref("action")
    .map(trim_ascii_whitespace)
    .filter(|action| !action.is_empty())
  {
    Some(action) => resolve_url(base_url, action)?,
    None => {
      let doc = trim_ascii_whitespace(document_url);
      if doc.is_empty() {
        base_url.to_string()
      } else {
        doc.to_string()
      }
    }
  };

  let mut url = Url::parse(&action_url).ok()?;
  // Form submission discards fragments.
  url.set_fragment(None);

  let limits = WebUrlLimits::default();
  let params = WebUrlSearchParams::new(&limits);
  append_form_controls(&index, form_id, submitter_node_id, &params)?;
  let query = params.serialize().ok()?;

  if query.is_empty() {
    url.set_query(None);
  } else {
    url.set_query(Some(&query));
  }

  Some(url.to_string())
}
