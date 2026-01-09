use crate::dom::{input_range_value, textarea_current_value, DomNode, DomNodeType};
use std::collections::HashMap;
use std::ptr;
use url::Url;
use url::form_urlencoded;

fn trim_ascii_whitespace(value: &str) -> &str {
  // HTML URL-ish attributes strip leading/trailing ASCII whitespace (TAB/LF/FF/CR/SPACE) but do not
  // treat all Unicode whitespace as ignorable. Use an explicit trim to avoid incorrectly dropping
  // characters like NBSP (U+00A0).
  value.trim_matches(|c: char| matches!(c, '\u{0009}' | '\u{000A}' | '\u{000C}' | '\u{000D}' | ' '))
}

fn resolve_url(base_url: &str, href: &str) -> Option<Url> {
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
      return Some(joined);
    }
  }

  let absolute = Url::parse(href).ok()?;
  (!absolute.scheme().eq_ignore_ascii_case("javascript")).then_some(absolute)
}

fn format_number(mut value: f64) -> String {
  // Match `crate::dom::format_number` behavior (not public).
  if value == -0.0 {
    value = 0.0;
  }
  let mut s = value.to_string();
  if s.contains('.') {
    while s.ends_with('0') {
      s.pop();
    }
    if s.ends_with('.') {
      s.pop();
    }
  }
  s
}

fn node_text_content(node: &DomNode) -> String {
  let mut content = String::new();
  let mut stack: Vec<&DomNode> = vec![node];
  while let Some(current) = stack.pop() {
    if let DomNodeType::Text { content: text } = &current.node_type {
      content.push_str(text);
    }
    for child in current.children.iter().rev() {
      stack.push(child);
    }
  }
  content
}

fn option_value(option: &DomNode) -> String {
  option
    .get_attribute_ref("value")
    .map(str::to_string)
    .unwrap_or_else(|| node_text_content(option))
}

fn input_type(node: &DomNode) -> &str {
  node
    .get_attribute_ref("type")
    .map(trim_ascii_whitespace)
    .filter(|v| !v.is_empty())
    .unwrap_or("text")
}

fn button_type(node: &DomNode) -> &str {
  node
    .get_attribute_ref("type")
    .map(trim_ascii_whitespace)
    .filter(|v| !v.is_empty())
    // HTML default button type is "submit".
    .unwrap_or("submit")
}

fn is_submitter_candidate(node: &DomNode) -> bool {
  match node.tag_name() {
    Some(tag) if tag.eq_ignore_ascii_case("input") => input_type(node).eq_ignore_ascii_case("submit"),
    Some(tag) if tag.eq_ignore_ascii_case("button") => button_type(node).eq_ignore_ascii_case("submit"),
    _ => false,
  }
}

struct DomIndex {
  id_to_ptr: Vec<*const DomNode>,
  parent: Vec<usize>,
  id_by_element_id: HashMap<String, usize>,
}

impl DomIndex {
  fn new(dom: &DomNode) -> Self {
    let mut id_to_ptr: Vec<*const DomNode> = vec![ptr::null()];
    let mut parent: Vec<usize> = vec![0];
    let mut id_by_element_id: HashMap<String, usize> = HashMap::new();

    // Pre-order traversal, matching `dom::enumerate_dom_ids` / cascade node ids.
    let mut stack: Vec<(&DomNode, usize)> = vec![(dom, 0)];
    while let Some((node, parent_id)) = stack.pop() {
      let id = id_to_ptr.len();
      id_to_ptr.push(node as *const DomNode);
      parent.push(parent_id);

      if let Some(html_id) = node.get_attribute_ref("id") {
        id_by_element_id.entry(html_id.to_string()).or_insert(id);
      }

      if node.is_template_element() {
        continue;
      }
      for child in node.children.iter().rev() {
        stack.push((child, id));
      }
    }

    Self {
      id_to_ptr,
      parent,
      id_by_element_id,
    }
  }

  fn node(&self, node_id: usize) -> Option<&DomNode> {
    let ptr = *self.id_to_ptr.get(node_id)?;
    if ptr.is_null() {
      return None;
    }
    // SAFETY: pointers originate from a live `DomNode` borrowed for the duration of the call.
    Some(unsafe { &*ptr })
  }

  fn is_ancestor_or_self(&self, ancestor: usize, mut node_id: usize) -> bool {
    while node_id != 0 {
      if node_id == ancestor {
        return true;
      }
      node_id = self.parent.get(node_id).copied().unwrap_or(0);
    }
    false
  }

  fn find_form_by_element_id(&self, html_id: &str) -> Option<usize> {
    let node_id = *self.id_by_element_id.get(html_id)?;
    self
      .node(node_id)
      .and_then(|node| node.tag_name())
      .is_some_and(|t| t.eq_ignore_ascii_case("form"))
      .then_some(node_id)
  }

  fn find_form_ancestor(&self, start_node_id: usize) -> Option<usize> {
    let mut current = start_node_id;
    while current != 0 {
      let node = self.node(current)?;
      if node
        .tag_name()
        .is_some_and(|tag| tag.eq_ignore_ascii_case("form"))
      {
        return Some(current);
      }
      current = self.parent.get(current).copied().unwrap_or(0);
    }
    None
  }
}

fn collect_select_submission_values(select: &DomNode) -> Vec<String> {
  let multiple = select.get_attribute_ref("multiple").is_some();

  let mut options: Vec<&DomNode> = Vec::new();
  let mut stack: Vec<&DomNode> = select.children.iter().rev().collect();
  while let Some(node) = stack.pop() {
    if node
      .tag_name()
      .is_some_and(|tag| tag.eq_ignore_ascii_case("option"))
    {
      options.push(node);
      // Options cannot contain nested options, but keep walking their children anyway.
    }
    if node.is_template_element() {
      continue;
    }
    for child in node.children.iter().rev() {
      stack.push(child);
    }
  }

  if multiple {
    return options
      .into_iter()
      .filter(|option| option.get_attribute_ref("disabled").is_none())
      .filter(|option| option.get_attribute_ref("selected").is_some())
      .map(option_value)
      .collect();
  }

  let selected = options
    .iter()
    .copied()
    .filter(|option| option.get_attribute_ref("disabled").is_none())
    .find(|option| option.get_attribute_ref("selected").is_some())
    .or_else(|| {
      options
        .iter()
        .copied()
        .find(|option| option.get_attribute_ref("disabled").is_none())
    });
  vec![selected.map(option_value).unwrap_or_default()]
}

fn collect_control_pairs(
  node: &DomNode,
  is_submitter: bool,
  out: &mut Vec<(String, String)>,
) {
  if node.get_attribute_ref("disabled").is_some() {
    return;
  }

  let Some(tag) = node.tag_name() else {
    return;
  };

  if tag.eq_ignore_ascii_case("input") {
    let ty = input_type(node);
    if ty.eq_ignore_ascii_case("checkbox") || ty.eq_ignore_ascii_case("radio") {
      if node.get_attribute_ref("checked").is_none() {
        return;
      }
      let Some(name) = node.get_attribute_ref("name").filter(|v| !v.is_empty()) else {
        return;
      };
      let value = node
        .get_attribute_ref("value")
        .map(str::to_string)
        .unwrap_or_else(|| "on".to_string());
      out.push((name.to_string(), value));
      return;
    }

    if ty.eq_ignore_ascii_case("range") {
      let Some(name) = node.get_attribute_ref("name").filter(|v| !v.is_empty()) else {
        return;
      };
      let Some(value) = input_range_value(node) else {
        return;
      };
      out.push((name.to_string(), format_number(value)));
      return;
    }

    if ty.eq_ignore_ascii_case("submit")
      || ty.eq_ignore_ascii_case("reset")
      || ty.eq_ignore_ascii_case("button")
      || ty.eq_ignore_ascii_case("image")
      || ty.eq_ignore_ascii_case("file")
      || ty.eq_ignore_ascii_case("hidden")
    {
      if ty.eq_ignore_ascii_case("submit") && is_submitter {
        let Some(name) = node.get_attribute_ref("name").filter(|v| !v.is_empty()) else {
          return;
        };
        let value = node.get_attribute_ref("value").unwrap_or("").to_string();
        out.push((name.to_string(), value));
      }
      return;
    }

    // Text-like.
    let Some(name) = node.get_attribute_ref("name").filter(|v| !v.is_empty()) else {
      return;
    };
    let value = node.get_attribute_ref("value").unwrap_or("").to_string();
    out.push((name.to_string(), value));
    return;
  }

  if tag.eq_ignore_ascii_case("textarea") {
    let Some(name) = node.get_attribute_ref("name").filter(|v| !v.is_empty()) else {
      return;
    };
    out.push((name.to_string(), textarea_current_value(node)));
    return;
  }

  if tag.eq_ignore_ascii_case("select") {
    let Some(name) = node.get_attribute_ref("name").filter(|v| !v.is_empty()) else {
      return;
    };
    let values = collect_select_submission_values(node);
    if node.get_attribute_ref("multiple").is_some() {
      for value in values {
        out.push((name.to_string(), value));
      }
    } else {
      let value = values.into_iter().next().unwrap_or_default();
      out.push((name.to_string(), value));
    }
    return;
  }

  if tag.eq_ignore_ascii_case("button") {
    if !button_type(node).eq_ignore_ascii_case("submit") {
      return;
    }
    if !is_submitter {
      return;
    }
    let Some(name) = node.get_attribute_ref("name").filter(|v| !v.is_empty()) else {
      return;
    };
    let value = node.get_attribute_ref("value").unwrap_or("").to_string();
    out.push((name.to_string(), value));
  }
}

/// Build a GET form submission URL for the given submit button/input.
///
/// MVP implementation:
/// - Finds the form owner using the submitter's `form` attribute when present, otherwise the
///   nearest `<form>` ancestor.
/// - Only supports method=GET.
pub fn form_submission_get_url(
  dom: &DomNode,
  submitter_node_id: usize,
  document_url: &str,
  base_url: &str,
) -> Option<String> {
  let index = DomIndex::new(dom);
  let submitter = index.node(submitter_node_id)?;
  if !is_submitter_candidate(submitter) {
    return None;
  }
  if submitter.get_attribute_ref("disabled").is_some() {
    return None;
  }

  let explicit_form_id = submitter
    .get_attribute_ref("form")
    .map(trim_ascii_whitespace)
    .filter(|v| !v.is_empty())
    .and_then(|html_id| index.find_form_by_element_id(html_id));
  let form_node_id = explicit_form_id.or_else(|| index.find_form_ancestor(submitter_node_id))?;
  let form = index.node(form_node_id)?;

  let method = form
    .get_attribute_ref("method")
    .map(trim_ascii_whitespace)
    .unwrap_or("get");
  let method = if method.is_empty() { "get" } else { method };
  if !method.eq_ignore_ascii_case("get") {
    return None;
  }

  let action_attr = form
    .get_attribute_ref("action")
    .map(trim_ascii_whitespace)
    .filter(|v| !v.is_empty());
  let action_raw = action_attr.unwrap_or(document_url);
  let mut action_url = resolve_url(base_url, action_raw)?;
  action_url.set_fragment(None);

  let mut pairs: Vec<(String, String)> = Vec::new();

  // Collect successful controls in tree order.
  let submitter_ptr = index.id_to_ptr.get(submitter_node_id).copied().unwrap_or(ptr::null());
  let mut stack: Vec<&DomNode> = form.children.iter().rev().collect();
  while let Some(node) = stack.pop() {
    let is_submitter = !submitter_ptr.is_null() && ptr::eq(node, submitter_ptr);
    collect_control_pairs(node, is_submitter, &mut pairs);

    if node.is_template_element() {
      continue;
    }
    for child in node.children.iter().rev() {
      stack.push(child);
    }
  }

  // If the submitter is associated via `form=...` and is not a descendant, include it explicitly.
  if explicit_form_id.is_some() && !index.is_ancestor_or_self(form_node_id, submitter_node_id) {
    collect_control_pairs(submitter, true, &mut pairs);
  }

  let mut serializer = form_urlencoded::Serializer::new(String::new());
  for (name, value) in pairs {
    serializer.append_pair(&name, &value);
  }
  let query = serializer.finish();
  if query.is_empty() {
    action_url.set_query(None);
  } else {
    action_url.set_query(Some(&query));
  }

  Some(action_url.to_string())
}

