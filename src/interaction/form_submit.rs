use crate::dom::{DomNode, DomNodeType, HTML_NAMESPACE, SVG_NAMESPACE};
use crate::resource::web_url::{WebUrlLimits, WebUrlSearchParams};

use url::Url;

use super::resolve_url;
use super::InteractionState;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FormSubmissionMethod {
  Get,
  Post,
}

impl FormSubmissionMethod {
  fn parse(value: &str) -> Self {
    let value = trim_ascii_whitespace(value);
    if value.eq_ignore_ascii_case("post") {
      return Self::Post;
    }
    // Includes empty/invalid values (HTML enumerated attribute default).
    Self::Get
  }

  pub fn as_http_method(self) -> &'static str {
    match self {
      Self::Get => "GET",
      Self::Post => "POST",
    }
  }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FormSubmissionEnctype {
  UrlEncoded,
  MultipartFormData,
  TextPlain,
}

impl FormSubmissionEnctype {
  fn parse(value: &str) -> Self {
    let value = trim_ascii_whitespace(value);
    if value.eq_ignore_ascii_case("multipart/form-data") {
      return Self::MultipartFormData;
    }
    if value.eq_ignore_ascii_case("text/plain") {
      return Self::TextPlain;
    }
    // Includes empty/invalid values (HTML enumerated attribute default).
    Self::UrlEncoded
  }
}

/// Result of an HTML form submission attempt.
///
/// This is intentionally "spec-shaped" rather than UI-shaped: it preserves method + body encoding
/// so the navigation layer can perform a real GET/POST request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FormSubmission {
  /// Target URL after applying action resolution and fragment stripping.
  pub url: String,
  pub method: FormSubmissionMethod,
  /// Request headers implied by the submission (notably `Content-Type` for POST).
  pub headers: Vec<(String, String)>,
  /// Request body for POST submissions.
  pub body: Option<Vec<u8>>,
}

fn trim_ascii_whitespace(value: &str) -> &str {
  // HTML URL-ish attributes strip leading/trailing ASCII whitespace (TAB/LF/FF/CR/SPACE) but do not
  // treat all Unicode whitespace as ignorable. Use an explicit trim to avoid incorrectly dropping
  // characters like NBSP (U+00A0).
  value.trim_matches(|c: char| matches!(c, '\u{0009}' | '\u{000A}' | '\u{000C}' | '\u{000D}' | ' '))
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

impl super::effective_disabled::DomIdLookup for DomIndex<'_> {
  fn len(&self) -> usize {
    self.len()
  }

  fn node(&self, node_id: usize) -> Option<&DomNode> {
    self.node(node_id)
  }

  fn parent_id(&self, node_id: usize) -> usize {
    self.parent.get(node_id).copied().unwrap_or(0)
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
  (is_input(node)
    && (input_type(node).eq_ignore_ascii_case("submit") || input_type(node).eq_ignore_ascii_case("image")))
    || (is_button(node) && button_type(node).eq_ignore_ascii_case("submit"))
}

fn is_disabled_or_inert(index: &DomIndex<'_>, node_id: usize) -> bool {
  super::effective_disabled::is_effectively_disabled(node_id, index)
    || super::effective_disabled::is_effectively_inert(node_id, index)
}

fn find_ancestor_form(index: &DomIndex<'_>, mut node_id: usize) -> Option<usize> {
  while node_id != 0 {
    let node = index.node(node_id)?;
    if is_form(node) {
      return Some(node_id);
    }
    // Shadow roots are tree root boundaries for form owner resolution; do not walk out into the
    // shadow host tree.
    if matches!(
      node.node_type,
      DomNodeType::ShadowRoot { .. } | DomNodeType::Document { .. }
    ) {
      break;
    }
    node_id = *index.parent.get(node_id).unwrap_or(&0);
  }
  None
}

fn tree_root_boundary_id(index: &DomIndex<'_>, mut node_id: usize) -> Option<usize> {
  while node_id != 0 {
    let node = index.node(node_id)?;
    if matches!(
      node.node_type,
      DomNodeType::Document { .. } | DomNodeType::ShadowRoot { .. }
    ) {
      return Some(node_id);
    }
    node_id = *index.parent.get(node_id).unwrap_or(&0);
  }
  None
}

fn node_or_ancestor_is_template(index: &DomIndex<'_>, node_id: usize) -> bool {
  super::effective_disabled::is_in_template_contents(node_id, index)
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
    return index
      .node(referenced)
      .is_some_and(is_form)
      .then_some(referenced);
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

#[derive(Debug, Clone, PartialEq, Eq)]
enum FormDataEntry {
  Text {
    name: String,
    value: String,
  },
  File {
    name: String,
    filename: String,
    content_type: String,
    bytes: Vec<u8>,
  },
}

fn entry_name(entry: &FormDataEntry) -> &str {
  match entry {
    FormDataEntry::Text { name, .. } => name,
    FormDataEntry::File { name, .. } => name,
  }
}

fn entry_value_for_urlencoded(entry: &FormDataEntry) -> &str {
  match entry {
    FormDataEntry::Text { value, .. } => value,
    // For urlencoded submissions file controls submit the filename (or empty string when no file is
    // selected).
    FormDataEntry::File { filename, .. } => filename,
  }
}

fn collect_form_entries(
  index: &DomIndex<'_>,
  form_node_id: usize,
  submitter_node_id: Option<usize>,
  submitter_image_coords: Option<(i32, i32)>,
  interaction_state: Option<&InteractionState>,
  out: &mut Vec<FormDataEntry>,
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
        out.push(FormDataEntry::Text {
          name: name.to_string(),
          value: value.to_string(),
        });
        continue;
      }

      if ty.eq_ignore_ascii_case("submit") {
        // Only include the activated submitter.
        continue;
      }

      if ty.eq_ignore_ascii_case("button") || ty.eq_ignore_ascii_case("reset") {
        continue;
      }

      if ty.eq_ignore_ascii_case("image") {
        continue;
      }

      if ty.eq_ignore_ascii_case("file") {
        let files = interaction_state
          .and_then(|state| state.form_state.files_for(node_id))
          .map(|files| files.as_slice())
          .unwrap_or(&[]);

        if files.is_empty() {
          // When no files are selected, submit an empty file entry.
          out.push(FormDataEntry::File {
            name: name.to_string(),
            filename: String::new(),
            content_type: "application/octet-stream".to_string(),
            bytes: Vec::new(),
          });
        } else {
          for file in files {
            out.push(FormDataEntry::File {
              name: name.to_string(),
              filename: file.filename.clone(),
              content_type: file.content_type.clone(),
              bytes: file.bytes.clone(),
            });
          }
        }
        continue;
      }

      if ty.eq_ignore_ascii_case("range") {
        let value = crate::dom::input_range_value(node)
          .map(crate::dom::format_number)
          .unwrap_or_else(|| node.get_attribute_ref("value").unwrap_or("").to_string());
        out.push(FormDataEntry::Text {
          name: name.to_string(),
          value,
        });
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
      out.push(FormDataEntry::Text {
        name: name.to_string(),
        value,
      });
      continue;
    }

    if is_textarea(node) {
      let value = crate::dom::textarea_current_value(node);
      out.push(FormDataEntry::Text {
        name: name.to_string(),
        value,
      });
      continue;
    }

    if is_select(node) {
      let multiple = node.get_attribute_ref("multiple").is_some();
      let options = collect_select_options(node);

      if multiple {
        for option in options {
          if option.selected && !option.disabled {
            out.push(FormDataEntry::Text {
              name: name.to_string(),
              value: option.value,
            });
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
            out.push(FormDataEntry::Text {
              name: name.to_string(),
              value: option.value.clone(),
            });
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

  if let Some(submitter_node_id) = submitter_node_id {
    // Include submitter name/value pair if it has a name.
    let submitter = index.node(submitter_node_id)?;
    if is_disabled_or_inert(index, submitter_node_id) {
      return None;
    }
    if let Some(name) = submitter
      .get_attribute_ref("name")
      .map(trim_ascii_whitespace)
      .filter(|name| !name.is_empty())
    {
      if is_input(submitter) && input_type(submitter).eq_ignore_ascii_case("image") {
        // `<input type=image>` submits click coordinates as `name.x`/`name.y`.
        //
        // We only track click coordinates for pointer activation. Keyboard activation (Enter/Space)
        // and other submit paths fall back to (0,0) like browsers.
        let (x, y) = submitter_image_coords.unwrap_or((0, 0));
        out.push(FormDataEntry::Text {
          name: format!("{name}.x"),
          value: x.max(0).to_string(),
        });
        out.push(FormDataEntry::Text {
          name: format!("{name}.y"),
          value: y.max(0).to_string(),
        });
      } else {
        let value = submitter.get_attribute_ref("value").unwrap_or("");
        out.push(FormDataEntry::Text {
          name: name.to_string(),
          value: value.to_string(),
        });
      }
    }
  }

  Some(())
}

fn serialize_urlencoded(entries: &[FormDataEntry]) -> Option<String> {
  let limits = WebUrlLimits::default();
  let params = WebUrlSearchParams::new(&limits);
  for entry in entries {
    append_pair(
      &params,
      entry_name(entry),
      entry_value_for_urlencoded(entry),
    )?;
  }
  params.serialize().ok()
}

fn escape_multipart_value(value: &str) -> String {
  let mut out = String::new();
  for ch in value.chars() {
    if ch == '"' || ch == '\\' {
      out.push('\\');
    }
    out.push(ch);
  }
  out
}

fn serialize_multipart_form_data(entries: &[FormDataEntry]) -> (Vec<u8>, String) {
  // Deterministic boundary for tests/offline fixtures.
  const BOUNDARY: &str = "fastrender-form-boundary";
  let mut body = Vec::new();

  for entry in entries {
    body.extend_from_slice(b"--");
    body.extend_from_slice(BOUNDARY.as_bytes());
    body.extend_from_slice(b"\r\n");

    match entry {
      FormDataEntry::Text { name, value } => {
        let name = escape_multipart_value(name);
        body.extend_from_slice(
          format!("Content-Disposition: form-data; name=\"{name}\"\r\n\r\n").as_bytes(),
        );
        body.extend_from_slice(value.as_bytes());
        body.extend_from_slice(b"\r\n");
      }
      FormDataEntry::File {
        name,
        filename,
        content_type,
        bytes,
      } => {
        let name = escape_multipart_value(name);
        let filename = escape_multipart_value(filename);
        body.extend_from_slice(
          format!("Content-Disposition: form-data; name=\"{name}\"; filename=\"{filename}\"\r\n")
            .as_bytes(),
        );
        body.extend_from_slice(format!("Content-Type: {content_type}\r\n\r\n").as_bytes());
        body.extend_from_slice(bytes);
        body.extend_from_slice(b"\r\n");
      }
    }
  }

  body.extend_from_slice(b"--");
  body.extend_from_slice(BOUNDARY.as_bytes());
  body.extend_from_slice(b"--\r\n");
  (body, BOUNDARY.to_string())
}

fn serialize_text_plain(entries: &[FormDataEntry]) -> Vec<u8> {
  let mut out = String::new();
  for (idx, entry) in entries.iter().enumerate() {
    if idx > 0 {
      out.push_str("\r\n");
    }
    out.push_str(entry_name(entry));
    out.push('=');
    out.push_str(entry_value_for_urlencoded(entry));
  }
  out.into_bytes()
}

fn action_url_for_submission(
  form: &DomNode,
  submitter: &DomNode,
  document_url: &str,
  base_url: &str,
) -> Option<String> {
  let action_attr = submitter
    .get_attribute_ref("formaction")
    .map(trim_ascii_whitespace)
    .filter(|action| !action.is_empty());
  match action_attr {
    Some(action) => resolve_url(base_url, action),
    None => action_url_for_form(form, document_url, base_url),
  }
}

fn action_url_for_form(form: &DomNode, document_url: &str, base_url: &str) -> Option<String> {
  let action_attr = form
    .get_attribute_ref("action")
    .map(trim_ascii_whitespace)
    .filter(|action| !action.is_empty());

  match action_attr {
    Some(action) => resolve_url(base_url, action),
    None => {
      let doc = trim_ascii_whitespace(document_url);
      if !doc.is_empty() {
        Some(doc.to_string())
      } else {
        let base = trim_ascii_whitespace(base_url);
        (!base.is_empty()).then(|| base.to_string())
      }
    }
  }
}

fn method_for_submission(form: &DomNode, submitter: &DomNode) -> FormSubmissionMethod {
  submitter
    .get_attribute_ref("formmethod")
    .map(FormSubmissionMethod::parse)
    .unwrap_or_else(|| method_for_form(form))
}

fn enctype_for_submission(form: &DomNode, submitter: &DomNode) -> FormSubmissionEnctype {
  submitter
    .get_attribute_ref("formenctype")
    .map(FormSubmissionEnctype::parse)
    .unwrap_or_else(|| enctype_for_form(form))
}

fn method_for_form(form: &DomNode) -> FormSubmissionMethod {
  form
    .get_attribute_ref("method")
    .map(FormSubmissionMethod::parse)
    .unwrap_or(FormSubmissionMethod::Get)
}

fn enctype_for_form(form: &DomNode) -> FormSubmissionEnctype {
  form
    .get_attribute_ref("enctype")
    .map(FormSubmissionEnctype::parse)
    .unwrap_or(FormSubmissionEnctype::UrlEncoded)
}

/// Compute an HTML form submission request for the given submit button/input.
///
/// Spec-shaped implementation:
/// - Finds the form owner using the submitter's `form` attribute when present, otherwise the
///   nearest `<form>` ancestor.
/// - Collects successful controls in tree order (including form-associated elements outside the
///   `<form>` subtree).
/// - Supports GET and POST with common `enctype` values.
/// - Applies `formaction`/`formmethod`/`formenctype` overrides on the submitter.
/// - Resolves `action` against the document base URL and strips fragments.
pub fn form_submission(
  dom: &DomNode,
  submitter_node_id: usize,
  submitter_image_coords: Option<(i32, i32)>,
  document_url: &str,
  base_url: &str,
  interaction_state: Option<&InteractionState>,
) -> Option<FormSubmission> {
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

  let method = method_for_submission(form, submitter);
  let enctype = enctype_for_submission(form, submitter);

  let action_url = action_url_for_submission(form, submitter, document_url, base_url)?;
  let mut url = Url::parse(&action_url).ok()?;
  // Form submission discards fragments.
  url.set_fragment(None);

  let mut entries: Vec<FormDataEntry> = Vec::new();
  collect_form_entries(
    &index,
    form_id,
    Some(submitter_node_id),
    submitter_image_coords,
    interaction_state,
    &mut entries,
  )?;

  match method {
    FormSubmissionMethod::Get => {
      // GET submissions set the query to the encoded form data.
      let query = serialize_urlencoded(&entries)?;
      if query.is_empty() {
        url.set_query(None);
      } else {
        url.set_query(Some(&query));
      }

      Some(FormSubmission {
        url: url.to_string(),
        method,
        headers: Vec::new(),
        body: None,
      })
    }
    FormSubmissionMethod::Post => {
      let (body, content_type) = match enctype {
        FormSubmissionEnctype::UrlEncoded => {
          let encoded = serialize_urlencoded(&entries)?;
          (
            encoded.into_bytes(),
            "application/x-www-form-urlencoded".to_string(),
          )
        }
        FormSubmissionEnctype::MultipartFormData => {
          let (body, boundary) = serialize_multipart_form_data(&entries);
          (body, format!("multipart/form-data; boundary={boundary}"))
        }
        FormSubmissionEnctype::TextPlain => {
          (serialize_text_plain(&entries), "text/plain".to_string())
        }
      };

      Some(FormSubmission {
        url: url.to_string(),
        method,
        headers: vec![("Content-Type".to_string(), content_type)],
        body: Some(body),
      })
    }
  }
}

/// Compute an HTML form submission request for the given `<form>` without a submitter.
///
/// This is used for implicit submissions (e.g. pressing Enter in a text input when the form has no
/// submit button). Since there is no submitter, submitter-specific overrides (e.g. `formaction`) do
/// not apply and no submitter name/value pair is included in the form data set.
pub fn form_submission_without_submitter(
  dom: &DomNode,
  form_node_id: usize,
  document_url: &str,
  base_url: &str,
  interaction_state: Option<&InteractionState>,
) -> Option<FormSubmission> {
  let index = DomIndex::new(dom);
  let form = index.node(form_node_id)?;
  if !is_form(form) {
    return None;
  }
  if is_disabled_or_inert(&index, form_node_id) {
    return None;
  }

  let method = method_for_form(form);
  let enctype = enctype_for_form(form);

  let action_url = action_url_for_form(form, document_url, base_url)?;
  let mut url = Url::parse(&action_url).ok()?;
  url.set_fragment(None);

  let mut entries: Vec<FormDataEntry> = Vec::new();
  collect_form_entries(&index, form_node_id, None, None, interaction_state, &mut entries)?;

  match method {
    FormSubmissionMethod::Get => {
      let query = serialize_urlencoded(&entries)?;
      if query.is_empty() {
        url.set_query(None);
      } else {
        url.set_query(Some(&query));
      }

      Some(FormSubmission {
        url: url.to_string(),
        method,
        headers: Vec::new(),
        body: None,
      })
    }
    FormSubmissionMethod::Post => {
      let (body, content_type) = match enctype {
        FormSubmissionEnctype::UrlEncoded => {
          let encoded = serialize_urlencoded(&entries)?;
          (
            encoded.into_bytes(),
            "application/x-www-form-urlencoded".to_string(),
          )
        }
        FormSubmissionEnctype::MultipartFormData => {
          let (body, boundary) = serialize_multipart_form_data(&entries);
          (body, format!("multipart/form-data; boundary={boundary}"))
        }
        FormSubmissionEnctype::TextPlain => {
          (serialize_text_plain(&entries), "text/plain".to_string())
        }
      };

      Some(FormSubmission {
        url: url.to_string(),
        method,
        headers: vec![("Content-Type".to_string(), content_type)],
        body: Some(body),
      })
    }
  }
}

/// Build a GET form submission URL for the given submit button/input.
///
/// This is a convenience wrapper around [`form_submission`] that only returns a URL for GET
/// submissions. POST submissions return `None`.
pub fn form_submission_get_url(
  dom: &DomNode,
  submitter_node_id: usize,
  document_url: &str,
  base_url: &str,
  interaction_state: Option<&InteractionState>,
) -> Option<String> {
  let submission = form_submission(
    dom,
    submitter_node_id,
    None,
    document_url,
    base_url,
    interaction_state,
  )?;
  (submission.method == FormSubmissionMethod::Get).then_some(submission.url)
}
