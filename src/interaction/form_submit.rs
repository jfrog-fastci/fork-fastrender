use crate::dom::{DomNode, DomNodeType, HTML_NAMESPACE, SVG_NAMESPACE};
use crate::resource::web_url::{WebUrlLimits, WebUrlSearchParams};

use url::Url;

use super::resolve_url;
use super::InteractionState;
#[cfg(feature = "vmjs")]
use super::state::FileSelection;
#[cfg(feature = "vmjs")]
use rustc_hash::FxHashMap;

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

/// Lookup interface for `<input type="file">` selections when building a `dom2` form submission.
///
/// `dom2` intentionally strips authored file input state from markup for security reasons, so hosts
/// (interaction engines, UIs) must provide the user-selected files out-of-band.
#[cfg(feature = "vmjs")]
pub trait Dom2FileInputLookup {
  fn files_for(&self, input: crate::dom2::NodeId) -> Option<&[FileSelection]>;
}

#[cfg(feature = "vmjs")]
impl Dom2FileInputLookup for FxHashMap<crate::dom2::NodeId, Vec<FileSelection>> {
  fn files_for(&self, input: crate::dom2::NodeId) -> Option<&[FileSelection]> {
    self.get(&input).map(|v| v.as_slice())
  }
}

#[cfg(feature = "vmjs")]
impl Dom2FileInputLookup for super::state::FormStateDom2 {
  fn files_for(&self, input: crate::dom2::NodeId) -> Option<&[FileSelection]> {
    self.file_inputs.get(&input).map(|v| v.as_slice())
  }
}

#[cfg(feature = "vmjs")]
impl Dom2FileInputLookup for super::state::InteractionStateDom2 {
  fn files_for(&self, input: crate::dom2::NodeId) -> Option<&[FileSelection]> {
    self
      .form_state
      .file_inputs
      .get(&input)
      .map(|v| v.as_slice())
  }
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
    && (input_type(node).eq_ignore_ascii_case("submit")
      || input_type(node).eq_ignore_ascii_case("image")))
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
          .and_then(|state| state.form_state().files_for(node_id))
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
    let name = submitter
      .get_attribute_ref("name")
      .map(trim_ascii_whitespace)
      .unwrap_or("");

    if is_input(submitter) && input_type(submitter).eq_ignore_ascii_case("image") {
      // `<input type=image>` submits click coordinates.
      //
      // - When the submitter has a non-empty name, browsers submit `name.x` and `name.y`.
      // - When the name is empty/missing, browsers submit `x` and `y`.
      //
      // We only track click coordinates for pointer activation. Keyboard activation (Enter/Space)
      // and other submit paths fall back to (0,0).
      let (x, y) = submitter_image_coords.unwrap_or((0, 0));
      let x_name = if name.is_empty() {
        "x".to_string()
      } else {
        format!("{name}.x")
      };
      let y_name = if name.is_empty() {
        "y".to_string()
      } else {
        format!("{name}.y")
      };
      out.push(FormDataEntry::Text {
        name: x_name,
        value: x.max(0).to_string(),
      });
      out.push(FormDataEntry::Text {
        name: y_name,
        value: y.max(0).to_string(),
      });
    } else if !name.is_empty() {
      let value = submitter.get_attribute_ref("value").unwrap_or("");
      out.push(FormDataEntry::Text {
        name: name.to_string(),
        value: value.to_string(),
      });
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
  // `name=` and `filename=` parameters are serialized inside a single `Content-Disposition` header
  // line. Inputs may contain CR/LF/control characters (e.g. from markup or OS filenames), which
  // would split headers and allow header injection.
  //
  // Sanitize by removing ASCII control characters before escaping and cap the output length to
  // prevent unbounded allocations from malicious inputs.
  const MAX_BYTES: usize = 1024;
  let mut out = String::with_capacity(value.len().min(MAX_BYTES));
  let mut out_len = 0usize;

  for ch in value.chars() {
    // Remove ASCII control characters (including CR/LF and NUL). Keep other Unicode characters.
    if ch.is_ascii_control() {
      continue;
    }

    let needs_escape = ch == '"' || ch == '\\';
    let add_len = ch.len_utf8() + if needs_escape { 1 } else { 0 };
    let next_len = out_len.checked_add(add_len).unwrap_or(usize::MAX);
    if next_len > MAX_BYTES {
      break;
    }

    if needs_escape {
      out.push('\\');
    }
    out.push(ch);
    out_len = next_len;
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
  collect_form_entries(
    &index,
    form_node_id,
    None,
    None,
    interaction_state,
    &mut entries,
  )?;

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

#[cfg(feature = "vmjs")]
mod dom2_support {
use super::*;
use crate::dom2;

// --- dom2 --------------------------------------------------------------------

fn is_html_element_tag_dom2(dom: &dom2::Document, node: dom2::NodeId, tag: &str) -> bool {
  match &dom.node(node).kind {
    dom2::NodeKind::Element {
      tag_name,
      namespace,
      ..
    } => dom.is_html_case_insensitive_namespace(namespace) && tag_name.eq_ignore_ascii_case(tag),
    _ => false,
  }
}

fn is_form_dom2(dom: &dom2::Document, node: dom2::NodeId) -> bool {
  is_html_element_tag_dom2(dom, node, "form")
}

fn is_input_dom2(dom: &dom2::Document, node: dom2::NodeId) -> bool {
  is_html_element_tag_dom2(dom, node, "input")
}

fn is_textarea_dom2(dom: &dom2::Document, node: dom2::NodeId) -> bool {
  is_html_element_tag_dom2(dom, node, "textarea")
}

fn is_select_dom2(dom: &dom2::Document, node: dom2::NodeId) -> bool {
  is_html_element_tag_dom2(dom, node, "select")
}

fn is_button_dom2(dom: &dom2::Document, node: dom2::NodeId) -> bool {
  is_html_element_tag_dom2(dom, node, "button")
}

fn get_attr_dom2<'a>(dom: &'a dom2::Document, node: dom2::NodeId, name: &str) -> Option<&'a str> {
  dom.get_attribute(node, name).ok().flatten()
}

fn has_attr_dom2(dom: &dom2::Document, node: dom2::NodeId, name: &str) -> bool {
  dom.has_attribute(node, name).ok().unwrap_or(false)
}

fn input_type_dom2<'a>(dom: &'a dom2::Document, node: dom2::NodeId) -> &'a str {
  get_attr_dom2(dom, node, "type")
    .map(trim_ascii_whitespace)
    .filter(|v| !v.is_empty())
    .unwrap_or("text")
}

fn button_type_dom2<'a>(dom: &'a dom2::Document, node: dom2::NodeId) -> &'a str {
  // HTML <button> defaults to submit.
  get_attr_dom2(dom, node, "type")
    .map(trim_ascii_whitespace)
    .filter(|v| !v.is_empty())
    .unwrap_or("submit")
}

fn is_submit_control_dom2(dom: &dom2::Document, node: dom2::NodeId) -> bool {
  (is_input_dom2(dom, node)
    && (input_type_dom2(dom, node).eq_ignore_ascii_case("submit")
      || input_type_dom2(dom, node).eq_ignore_ascii_case("image")))
    || (is_button_dom2(dom, node) && button_type_dom2(dom, node).eq_ignore_ascii_case("submit"))
}

fn is_disabled_or_inert_dom2(dom: &dom2::Document, node: dom2::NodeId) -> bool {
  super::super::effective_disabled_dom2::is_effectively_disabled(node, dom)
    || super::super::effective_disabled_dom2::is_effectively_inert(node, dom)
}

fn tree_root_boundary_dom2(dom: &dom2::Document, node: dom2::NodeId) -> Option<dom2::NodeId> {
  let mut current = Some(node);
  let mut remaining = dom.nodes_len().saturating_add(1);
  while let Some(id) = current {
    if remaining == 0 {
      break;
    }
    remaining -= 1;

    match &dom.node(id).kind {
      dom2::NodeKind::Document { .. } | dom2::NodeKind::ShadowRoot { .. } => return Some(id),
      _ => {}
    }

    current = dom.parent_node(id);
  }
  None
}

fn find_ancestor_form_dom2(dom: &dom2::Document, node: dom2::NodeId) -> Option<dom2::NodeId> {
  let mut current = Some(node);
  let mut remaining = dom.nodes_len().saturating_add(1);
  while let Some(id) = current {
    if remaining == 0 {
      break;
    }
    remaining -= 1;

    if is_form_dom2(dom, id) {
      return Some(id);
    }

    // Shadow roots are tree root boundaries for form owner resolution; do not walk out into the
    // shadow host tree.
    if matches!(
      dom.node(id).kind,
      dom2::NodeKind::ShadowRoot { .. } | dom2::NodeKind::Document { .. }
    ) {
      break;
    }

    current = dom.parent_node(id);
  }
  None
}

fn resolve_form_owner_dom2(dom: &dom2::Document, control: dom2::NodeId) -> Option<dom2::NodeId> {
  let form_attr = get_attr_dom2(dom, control, "form")
    .map(trim_ascii_whitespace)
    .filter(|v| !v.is_empty());
  if let Some(form_id) = form_attr {
    let root = tree_root_boundary_dom2(dom, control)?;
    let referenced = dom.get_element_by_id_from(root, form_id)?;
    return is_form_dom2(dom, referenced).then_some(referenced);
  }

  find_ancestor_form_dom2(dom, control)
}

fn collect_descendant_text_content_dom2(dom: &dom2::Document, root: dom2::NodeId) -> String {
  let mut text = String::new();
  let mut stack: Vec<dom2::NodeId> = vec![root];
  let mut remaining = dom.nodes_len().saturating_add(1);

  while let Some(node_id) = stack.pop() {
    if remaining == 0 {
      break;
    }
    remaining -= 1;

    match &dom.node(node_id).kind {
      dom2::NodeKind::Text { content } => text.push_str(content),
      dom2::NodeKind::Element {
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

    let node = dom.node(node_id);
    for &child in node.children.iter().rev() {
      if dom
        .nodes()
        .get(child.index())
        .is_some_and(|child_node| child_node.parent == Some(node_id))
      {
        stack.push(child);
      }
    }
  }

  text
}

fn option_value_dom2(dom: &dom2::Document, option: dom2::NodeId) -> String {
  if let Some(value) = get_attr_dom2(dom, option, "value") {
    return value.to_string();
  }
  crate::dom::strip_and_collapse_ascii_whitespace(&collect_descendant_text_content_dom2(
    dom, option,
  ))
}

#[derive(Debug, Clone)]
struct SelectOptionDom2 {
  value: String,
  selected: bool,
  disabled: bool,
}

fn is_optgroup_dom2(dom: &dom2::Document, node: dom2::NodeId) -> bool {
  is_html_element_tag_dom2(dom, node, "optgroup")
}

fn is_option_dom2(dom: &dom2::Document, node: dom2::NodeId) -> bool {
  is_html_element_tag_dom2(dom, node, "option")
}

fn option_disabled_dom2(dom: &dom2::Document, option: dom2::NodeId, select: dom2::NodeId) -> bool {
  if has_attr_dom2(dom, option, "disabled") {
    return true;
  }
  let mut current = dom.parent_node(option);
  let mut remaining = dom.nodes_len().saturating_add(1);
  while let Some(id) = current {
    if remaining == 0 {
      break;
    }
    remaining -= 1;
    if id == select {
      break;
    }
    if is_optgroup_dom2(dom, id) && has_attr_dom2(dom, id, "disabled") {
      return true;
    }
    current = dom.parent_node(id);
  }
  false
}

fn collect_select_options_dom2(
  dom: &dom2::Document,
  select: dom2::NodeId,
) -> Vec<SelectOptionDom2> {
  let mut out = Vec::new();
  for option in dom.select_options(select) {
    if !is_option_dom2(dom, option) {
      continue;
    }
    // Skip inert `<template>` contents (and other inert subtrees) inside the select, matching the
    // renderer DOM's template-contents semantics.
    if super::super::effective_disabled_dom2::is_effectively_inert(option, dom) {
      continue;
    }
    out.push(SelectOptionDom2 {
      value: option_value_dom2(dom, option),
      selected: dom.option_selected(option).ok().unwrap_or(false),
      disabled: option_disabled_dom2(dom, option, select),
    });
  }
  out
}

fn action_url_for_form_dom2(
  dom: &dom2::Document,
  form: dom2::NodeId,
  document_url: &str,
  base_url: &str,
) -> Option<String> {
  let action_attr = get_attr_dom2(dom, form, "action")
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

fn action_url_for_submission_dom2(
  dom: &dom2::Document,
  form: dom2::NodeId,
  submitter: dom2::NodeId,
  document_url: &str,
  base_url: &str,
) -> Option<String> {
  let action_attr = get_attr_dom2(dom, submitter, "formaction")
    .map(trim_ascii_whitespace)
    .filter(|action| !action.is_empty());
  match action_attr {
    Some(action) => resolve_url(base_url, action),
    None => action_url_for_form_dom2(dom, form, document_url, base_url),
  }
}

fn method_for_form_dom2(dom: &dom2::Document, form: dom2::NodeId) -> FormSubmissionMethod {
  get_attr_dom2(dom, form, "method")
    .map(FormSubmissionMethod::parse)
    .unwrap_or(FormSubmissionMethod::Get)
}

fn enctype_for_form_dom2(dom: &dom2::Document, form: dom2::NodeId) -> FormSubmissionEnctype {
  get_attr_dom2(dom, form, "enctype")
    .map(FormSubmissionEnctype::parse)
    .unwrap_or(FormSubmissionEnctype::UrlEncoded)
}

fn method_for_submission_dom2(
  dom: &dom2::Document,
  form: dom2::NodeId,
  submitter: dom2::NodeId,
) -> FormSubmissionMethod {
  get_attr_dom2(dom, submitter, "formmethod")
    .map(FormSubmissionMethod::parse)
    .unwrap_or_else(|| method_for_form_dom2(dom, form))
}

fn enctype_for_submission_dom2(
  dom: &dom2::Document,
  form: dom2::NodeId,
  submitter: dom2::NodeId,
) -> FormSubmissionEnctype {
  get_attr_dom2(dom, submitter, "formenctype")
    .map(FormSubmissionEnctype::parse)
    .unwrap_or_else(|| enctype_for_form_dom2(dom, form))
}

fn collect_form_entries_dom2(
  dom: &dom2::Document,
  form_node_id: dom2::NodeId,
  submitter_node_id: Option<dom2::NodeId>,
  submitter_image_coords: Option<(i32, i32)>,
  file_inputs: Option<&dyn Dom2FileInputLookup>,
  out: &mut Vec<FormDataEntry>,
) -> Option<()> {
  // Spec-ish: successful controls are collected in tree order (document order), including form-
  // associated elements outside the `<form>` subtree.
  for node_id in dom.dom_connected_preorder() {
    // Skip non-elements (including Document and ShadowRoot nodes).
    match &dom.node(node_id).kind {
      dom2::NodeKind::Element { .. } => {}
      _ => continue,
    }

    // Skip disabled controls and inert subtrees.
    if is_disabled_or_inert_dom2(dom, node_id) {
      continue;
    }

    if !(is_input_dom2(dom, node_id)
      || is_textarea_dom2(dom, node_id)
      || is_select_dom2(dom, node_id))
    {
      continue;
    }

    if resolve_form_owner_dom2(dom, node_id) != Some(form_node_id) {
      continue;
    }

    let Some(name) = get_attr_dom2(dom, node_id, "name")
      .map(trim_ascii_whitespace)
      .filter(|name| !name.is_empty())
    else {
      continue;
    };

    if is_input_dom2(dom, node_id) {
      let ty = input_type_dom2(dom, node_id);

      if ty.eq_ignore_ascii_case("checkbox") || ty.eq_ignore_ascii_case("radio") {
        let checked = dom.input_checked(node_id).ok().unwrap_or(false);
        if !checked {
          continue;
        }

        // Checkbox/radio value is sourced from the live form control state. The `dom2` input state
        // initializes checkable controls to the HTML default value "on" when no `value` content
        // attribute is present.
        let value = dom.input_value(node_id).ok().unwrap_or("");

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
        let files = file_inputs
          .and_then(|store| store.files_for(node_id))
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

      // For all other input types, use the live `dom2` input state value.
      let value = dom.input_value(node_id).ok().unwrap_or("").to_string();
      out.push(FormDataEntry::Text {
        name: name.to_string(),
        value,
      });
      continue;
    }

    if is_textarea_dom2(dom, node_id) {
      let value = dom.textarea_value(node_id).ok()?;
      out.push(FormDataEntry::Text {
        name: name.to_string(),
        value,
      });
      continue;
    }

    if is_select_dom2(dom, node_id) {
      let multiple = has_attr_dom2(dom, node_id, "multiple");
      let options = collect_select_options_dom2(dom, node_id);

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
  }

  if let Some(submitter_node_id) = submitter_node_id {
    // Include submitter name/value pair if it has a name.
    if is_disabled_or_inert_dom2(dom, submitter_node_id) {
      return None;
    }

    let name = get_attr_dom2(dom, submitter_node_id, "name")
      .map(trim_ascii_whitespace)
      .unwrap_or("");

    if is_input_dom2(dom, submitter_node_id)
      && input_type_dom2(dom, submitter_node_id).eq_ignore_ascii_case("image")
    {
      let (x, y) = submitter_image_coords.unwrap_or((0, 0));
      let x_name = if name.is_empty() {
        "x".to_string()
      } else {
        format!("{name}.x")
      };
      let y_name = if name.is_empty() {
        "y".to_string()
      } else {
        format!("{name}.y")
      };
      out.push(FormDataEntry::Text {
        name: x_name,
        value: x.max(0).to_string(),
      });
      out.push(FormDataEntry::Text {
        name: y_name,
        value: y.max(0).to_string(),
      });
    } else if !name.is_empty() {
      let value = get_attr_dom2(dom, submitter_node_id, "value").unwrap_or("");
      out.push(FormDataEntry::Text {
        name: name.to_string(),
        value: value.to_string(),
      });
    }
  }

  Some(())
}

/// Compute an HTML form submission request using a live `dom2` document.
///
/// `form_node_id` must identify the `<form>` element being submitted. When `submitter_node_id` is
/// provided, submitter-specific overrides (`formaction`, `formmethod`, `formenctype`) are applied and
/// the submitter name/value pair (or image coordinates) is included in the form data set.
pub fn form_submission_dom2(
  dom: &dom2::Document,
  form_node_id: dom2::NodeId,
  submitter_node_id: Option<dom2::NodeId>,
  submitter_image_coords: Option<(i32, i32)>,
  document_url: &str,
  base_url: &str,
  file_inputs: Option<&dyn Dom2FileInputLookup>,
) -> Option<FormSubmission> {
  if !is_form_dom2(dom, form_node_id) {
    return None;
  }
  if is_disabled_or_inert_dom2(dom, form_node_id) {
    return None;
  }

  let (method, enctype, action_url) = match submitter_node_id {
    Some(submitter) => {
      if !is_submit_control_dom2(dom, submitter) {
        return None;
      }
      if is_disabled_or_inert_dom2(dom, submitter) {
        return None;
      }
      if resolve_form_owner_dom2(dom, submitter) != Some(form_node_id) {
        return None;
      }
      (
        method_for_submission_dom2(dom, form_node_id, submitter),
        enctype_for_submission_dom2(dom, form_node_id, submitter),
        action_url_for_submission_dom2(dom, form_node_id, submitter, document_url, base_url)?,
      )
    }
    None => (
      method_for_form_dom2(dom, form_node_id),
      enctype_for_form_dom2(dom, form_node_id),
      action_url_for_form_dom2(dom, form_node_id, document_url, base_url)?,
    ),
  };

  let mut url = Url::parse(&action_url).ok()?;
  // Form submission discards fragments.
  url.set_fragment(None);

  let mut entries: Vec<FormDataEntry> = Vec::new();
  collect_form_entries_dom2(
    dom,
    form_node_id,
    submitter_node_id,
    submitter_image_coords,
    file_inputs,
    &mut entries,
  )?;

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

/// Compute an HTML form submission request for a submit button/input, using a live `dom2` document.
///
/// This mirrors the legacy [`form_submission`] API shape (submitter-first) so interaction engines
/// can compute a submission given only the activated submitter element.
pub fn form_submission_from_submitter_dom2(
  dom: &dom2::Document,
  submitter_node_id: dom2::NodeId,
  submitter_image_coords: Option<(i32, i32)>,
  document_url: &str,
  base_url: &str,
  file_inputs: Option<&dyn Dom2FileInputLookup>,
) -> Option<FormSubmission> {
  let form_id = resolve_form_owner_dom2(dom, submitter_node_id)?;
  form_submission_dom2(
    dom,
    form_id,
    Some(submitter_node_id),
    submitter_image_coords,
    document_url,
    base_url,
    file_inputs,
  )
}

/// Compute an HTML form submission request for the given `<form>` without a submitter, using `dom2`.
pub fn form_submission_without_submitter_dom2(
  dom: &dom2::Document,
  form_node_id: dom2::NodeId,
  document_url: &str,
  base_url: &str,
  file_inputs: Option<&dyn Dom2FileInputLookup>,
) -> Option<FormSubmission> {
  form_submission_dom2(
    dom,
    form_node_id,
    /* submitter */ None,
    /* coords */ None,
    document_url,
    base_url,
    file_inputs,
  )
}

/// Build a GET form submission URL for the given submitter, using `dom2`.
pub fn form_submission_get_url_dom2(
  dom: &dom2::Document,
  form_node_id: dom2::NodeId,
  submitter_node_id: Option<dom2::NodeId>,
  document_url: &str,
  base_url: &str,
  file_inputs: Option<&dyn Dom2FileInputLookup>,
) -> Option<String> {
  let submission = form_submission_dom2(
    dom,
    form_node_id,
    submitter_node_id,
    None,
    document_url,
    base_url,
    file_inputs,
  )?;
  (submission.method == FormSubmissionMethod::Get).then_some(submission.url)
}

/// Convenience wrapper around [`form_submission_from_submitter_dom2`] for GET-only submissions.
pub fn form_submission_get_url_from_submitter_dom2(
  dom: &dom2::Document,
  submitter_node_id: dom2::NodeId,
  document_url: &str,
  base_url: &str,
  file_inputs: Option<&dyn Dom2FileInputLookup>,
) -> Option<String> {
  let submission = form_submission_from_submitter_dom2(
    dom,
    submitter_node_id,
    None,
    document_url,
    base_url,
    file_inputs,
  )?;
  (submission.method == FormSubmissionMethod::Get).then_some(submission.url)
}

#[cfg(test)]
mod dom2_tests {
  use super::*;
  use selectors::context::QuirksMode;

  #[test]
  fn dom2_form_submission_respects_disabled_fieldset_checkedness_and_textarea_value() {
    let html = concat!(
      "<!doctype html>",
      "<html><body>",
      "<form id=\"f\" action=\"https://example.com/submit\">",
      "  <fieldset disabled>",
      "    <legend><input id=\"a\" name=\"a\" value=\"1\"></legend>",
      "    <input id=\"b\" name=\"b\" value=\"2\">",
      "  </fieldset>",
      "  <input id=\"c\" type=\"checkbox\" name=\"c\" value=\"yes\">",
      "  <input id=\"c2\" type=\"checkbox\" name=\"d\">",
      "  <input id=\"r1\" type=\"radio\" name=\"r\" value=\"1\">",
      "  <input id=\"r2\" type=\"radio\" name=\"r\" value=\"2\">",
      "  <textarea id=\"t\" name=\"t\">default</textarea>",
      "</form>",
      "</body></html>",
    );

    let mut doc = crate::dom2::parse_html(html).expect("parse dom2");
    let form = doc.get_element_by_id("f").expect("form");
    let checkbox = doc.get_element_by_id("c").expect("checkbox");
    let checkbox_default = doc.get_element_by_id("c2").expect("checkbox");
    let radio2 = doc.get_element_by_id("r2").expect("radio");
    let textarea = doc.get_element_by_id("t").expect("textarea");

    // Simulate user interaction: checkedness/value are stored in dom2 internal state.
    doc.set_input_checked(checkbox, true).unwrap();
    doc.set_input_checked(checkbox_default, true).unwrap();
    doc.set_input_checked(radio2, true).unwrap();
    doc.set_textarea_value(textarea, "edited").unwrap();

    let submission = form_submission_without_submitter_dom2(
      &doc,
      form,
      "https://example.com/page",
      "https://example.com/page",
      None,
    )
    .expect("submission");

    assert_eq!(
      submission.url,
      "https://example.com/submit?a=1&c=yes&d=on&r=2&t=edited",
      "expected: (1) first-legend input included and fieldset-disabled input excluded, (2) checkbox/radio use dom2 checkedness state, (3) textarea uses dom2 current value"
    );
  }

  #[test]
  fn dom2_form_submission_ignores_options_in_inert_template_subtrees() {
    let mut doc = crate::dom2::Document::new(QuirksMode::NoQuirks);
    let root = doc.root();
    let body = doc.create_element("body", "");
    doc.append_child(root, body).unwrap();

    let form = doc.create_element("form", "");
    doc
      .set_attribute(form, "action", "https://example.com/submit")
      .unwrap();
    doc.append_child(body, form).unwrap();

    let select = doc.create_element("select", "");
    doc.set_attribute(select, "name", "s").unwrap();
    doc.append_child(form, select).unwrap();

    let option_live = doc.create_element("option", "");
    doc.set_attribute(option_live, "value", "a").unwrap();
    doc
      .set_bool_attribute(option_live, "selected", true)
      .unwrap();
    doc.append_child(select, option_live).unwrap();

    // Insert an inert `<template>` subtree inside the `<select>` via DOM APIs. Its `<option>`
    // descendants should not contribute to the select's option list or to form submission.
    let template = doc.create_element("template", "");
    doc.append_child(select, template).unwrap();

    let option_inert = doc.create_element("option", "");
    doc.set_attribute(option_inert, "value", "b").unwrap();
    doc
      .set_bool_attribute(option_inert, "selected", true)
      .unwrap();
    doc.append_child(template, option_inert).unwrap();

    let submission = form_submission_without_submitter_dom2(
      &doc,
      form,
      "https://example.com/page",
      "https://example.com/page",
      None,
    )
    .expect("submission");

    assert_eq!(
      submission.url, "https://example.com/submit?s=a",
      "options inside inert template subtrees must not contribute to successful controls"
    );
  }
}

} // mod dom2_support

#[cfg(feature = "vmjs")]
pub use dom2_support::{
  form_submission_dom2, form_submission_from_submitter_dom2, form_submission_get_url_dom2,
  form_submission_get_url_from_submitter_dom2, form_submission_without_submitter_dom2,
};

#[cfg(test)]
mod multipart_tests {
  use super::*;

  #[test]
  fn multipart_filename_crlf_injection_is_sanitized() {
    let entries = vec![FormDataEntry::File {
      name: "file".to_string(),
      filename: "evil\"\r\nX: y".to_string(),
      content_type: "text/plain".to_string(),
      bytes: b"hello".to_vec(),
    }];

    let (body, _boundary) = serialize_multipart_form_data(&entries);
    let body_str = String::from_utf8(body).expect("multipart body must be valid UTF-8 for test");

    // The injected header line must not appear in the body.
    assert!(
      !body_str.contains("\r\nX: y"),
      "multipart body contains injected header line: {body_str:?}"
    );

    // The quote should still be escaped in the serialized header parameter value.
    assert!(
      body_str.contains("filename=\"evil\\\"X: y\""),
      "expected escaped filename not found: {body_str:?}"
    );
  }

  #[test]
  fn multipart_value_strips_controls_and_escapes_quotes() {
    let input = "a\u{0000}\u{001f}b\"c\\d\r\ne";
    let escaped = escape_multipart_value(input);
    assert_eq!(escaped, "ab\\\"c\\\\de");

    assert!(
      !escaped.chars().any(|c| c.is_ascii_control()),
      "escaped value must not contain ASCII control characters: {escaped:?}"
    );
  }
}
