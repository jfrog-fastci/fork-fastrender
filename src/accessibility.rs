use crate::dom::{forms_validation, DomNode, DomNodeType, ElementRef, HTML_NAMESPACE};
use crate::error::{Error, RenderStage, Result};
use crate::interaction::InteractionState;
use crate::render_control;
use crate::style::cascade::StyledNode;
use crate::style::computed::Visibility;
use crate::style::display::Display;
use serde::Serialize;
use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::ptr;

// AccessKit integration (used by the windowed browser UI) lives in a separate submodule so the core
// renderer can compile without pulling in the optional `accesskit` dependency.
#[cfg(feature = "browser_ui")]
pub mod accesskit_tree;
#[cfg(feature = "browser_ui")]
pub mod accesskit_mapping;

#[cfg(feature = "a11y_accesskit")]
pub mod accesskit_bridge;
#[cfg(feature = "a11y_accesskit")]
pub mod accesskit_ids;

fn is_html_ascii_whitespace(c: char) -> bool {
  matches!(c, '\u{0009}' | '\u{000A}' | '\u{000C}' | '\u{000D}' | ' ')
}

fn trim_ascii_whitespace(value: &str) -> &str {
  value.trim_matches(is_html_ascii_whitespace)
}

fn split_ascii_whitespace(value: &str) -> impl Iterator<Item = &str> {
  value
    .split(is_html_ascii_whitespace)
    .filter(|part| !part.is_empty())
}

/// Checked state for toggleable controls.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum CheckState {
  True,
  False,
  Mixed,
}

/// Pressed state for buttons/switches.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum PressedState {
  True,
  False,
  Mixed,
}

/// Current state for landmarks and navigation items.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum AriaCurrent {
  Page,
  Step,
  Location,
  Date,
  Time,
  True,
}

/// Accessibility-related states for a node.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct AccessibilityState {
  pub focusable: bool,
  #[serde(skip_serializing_if = "is_false")]
  pub focused: bool,
  #[serde(skip_serializing_if = "is_false")]
  pub focus_visible: bool,
  pub disabled: bool,
  pub required: bool,
  pub invalid: bool,
  pub visited: bool,
  #[serde(skip_serializing_if = "is_false")]
  pub busy: bool,
  pub readonly: bool,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub has_popup: Option<String>,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub multiline: Option<bool>,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub checked: Option<CheckState>,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub selected: Option<bool>,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub pressed: Option<PressedState>,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub expanded: Option<bool>,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub current: Option<AriaCurrent>,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub modal: Option<bool>,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub live: Option<String>,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub atomic: Option<bool>,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub relevant: Option<String>,
}

impl Default for AccessibilityState {
  fn default() -> Self {
    Self {
      focusable: false,
      focused: false,
      focus_visible: false,
      disabled: false,
      required: false,
      invalid: false,
      visited: false,
      busy: false,
      readonly: false,
      has_popup: None,
      multiline: None,
      checked: None,
      selected: None,
      pressed: None,
      expanded: None,
      current: None,
      modal: None,
      live: None,
      atomic: None,
      relevant: None,
    }
  }
}

/// Common ARIA relationships exported for downstream tooling.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct AccessibilityRelations {
  #[serde(skip_serializing_if = "Vec::is_empty")]
  pub controls: Vec<String>,
  #[serde(skip_serializing_if = "Vec::is_empty")]
  pub owns: Vec<String>,
  #[serde(skip_serializing_if = "Vec::is_empty")]
  pub labelled_by: Vec<String>,
  #[serde(skip_serializing_if = "Vec::is_empty")]
  pub described_by: Vec<String>,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub active_descendant: Option<String>,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub details: Option<String>,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub error_message: Option<String>,
}

/// Debug-only accessibility metadata that is not part of the stable JSON schema.
///
/// This is currently used for exposing selection/caret state in accessibility tests.
#[cfg(any(debug_assertions, feature = "a11y_debug"))]
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct AccessibilityDebugInfo {
  #[serde(skip_serializing_if = "Option::is_none")]
  pub text_selection: Option<AccessibilityTextSelection>,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub document_selection: Option<AccessibilityDocumentSelection>,
  #[serde(skip_serializing_if = "is_false")]
  pub document_has_selection: bool,
}

#[cfg(any(debug_assertions, feature = "a11y_debug"))]
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct AccessibilityTextSelection {
  /// Caret position in character indices.
  pub caret: usize,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub selection_start: Option<usize>,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub selection_end: Option<usize>,
}

#[cfg(any(debug_assertions, feature = "a11y_debug"))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct AccessibilityDocumentSelectionPoint {
  pub node_id: usize,
  pub char_offset: usize,
}

#[cfg(any(debug_assertions, feature = "a11y_debug"))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct AccessibilityDocumentSelectionRange {
  pub start: AccessibilityDocumentSelectionPoint,
  pub end: AccessibilityDocumentSelectionPoint,
}

#[cfg(any(debug_assertions, feature = "a11y_debug"))]
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum AccessibilityDocumentSelection {
  All,
  Ranges {
    #[serde(skip_serializing_if = "Vec::is_empty")]
    ranges: Vec<AccessibilityDocumentSelectionRange>,
    primary: usize,
    anchor: AccessibilityDocumentSelectionPoint,
    focus: AccessibilityDocumentSelectionPoint,
  },
}

#[cfg(any(debug_assertions, feature = "a11y_debug"))]
fn debug_document_selection(
  selection: &crate::interaction::state::DocumentSelectionState,
) -> AccessibilityDocumentSelection {
  use crate::interaction::state::DocumentSelectionState;

  match selection {
    DocumentSelectionState::All => AccessibilityDocumentSelection::All,
    DocumentSelectionState::Ranges(ranges) => {
      let mut ranges = ranges.clone();
      ranges.normalize();

      AccessibilityDocumentSelection::Ranges {
        ranges: ranges
          .ranges
          .iter()
          .copied()
          .map(|range| {
            let range = range.normalized();
            AccessibilityDocumentSelectionRange {
              start: AccessibilityDocumentSelectionPoint {
                node_id: range.start.node_id,
                char_offset: range.start.char_offset,
              },
              end: AccessibilityDocumentSelectionPoint {
                node_id: range.end.node_id,
                char_offset: range.end.char_offset,
              },
            }
          })
          .collect(),
        primary: ranges.primary,
        anchor: AccessibilityDocumentSelectionPoint {
          node_id: ranges.anchor.node_id,
          char_offset: ranges.anchor.char_offset,
        },
        focus: AccessibilityDocumentSelectionPoint {
          node_id: ranges.focus.node_id,
          char_offset: ranges.focus.char_offset,
        },
      }
    }
  }
}

/// A node in the exported accessibility tree.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct AccessibilityNode {
  /// 1-indexed pre-order identifier for the originating styled/DOM node.
  ///
  /// This is used for mapping exported accessibility nodes back to rendered elements (e.g. for
  /// AccessKit integration), but must not affect the stable JSON schema used by snapshot tests.
  #[serde(skip)]
  pub node_id: usize,
  pub role: String,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub role_description: Option<String>,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub name: Option<String>,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub description: Option<String>,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub value: Option<String>,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub level: Option<u32>,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub html_tag: Option<String>,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub id: Option<String>,
  /// Stable DOM node identifier that can be used by downstream tooling (e.g. AccessKit action
  /// routing) to map accessibility nodes back to the originating DOM nodes.
  ///
  /// This intentionally does not appear in the JSON snapshot schema.
  #[serde(skip)]
  pub dom_node_id: usize,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub relations: Option<AccessibilityRelations>,
  pub states: AccessibilityState,
  pub children: Vec<AccessibilityNode>,
  #[cfg(any(debug_assertions, feature = "a11y_debug"))]
  #[serde(skip_serializing_if = "Option::is_none")]
  pub debug: Option<AccessibilityDebugInfo>,
}

// Task 15: AccessKit integration is only built when the desktop browser UI stack is enabled.
#[cfg(feature = "browser_ui")]
pub mod accesskit;

/// Build an accessibility tree from a styled DOM.
pub fn build_accessibility_tree(
  root: &StyledNode,
  interaction_state: Option<&InteractionState>,
) -> Result<AccessibilityNode> {
  let mut lookup = HashMap::new();
  build_styled_lookup(root, &mut lookup)?;
  let mut hidden = HashMap::new();
  let mut aria_hidden = HashMap::new();
  let mut node_scope = HashMap::new();
  let mut ids_by_scope: HashMap<usize, HashMap<String, usize>> = HashMap::new();
  compute_hidden_and_scoped_ids(
    root,
    &mut hidden,
    &mut aria_hidden,
    &mut node_scope,
    &mut ids_by_scope,
  )?;

  let labels = collect_labels(root, &node_scope, &ids_by_scope, &lookup)?;

  let (aria_owned_children, aria_owned_by) =
    compute_aria_owns(root, &lookup, &hidden, &node_scope, &ids_by_scope)?;

  let needs_validation_dom = lookup.values().any(|node| {
    node
      .node
      .tag_name()
      .is_some_and(|tag| tag.eq_ignore_ascii_case("input"))
      && node
        .node
        .get_attribute_ref("type")
        .is_some_and(|t| t.eq_ignore_ascii_case("radio"))
      && node
        .node
       .get_attribute_ref("name")
       .is_some_and(|name| !name.is_empty())
  });
  let has_form_overrides = interaction_state.is_some_and(|state| state.form_state().has_overrides());
  let needs_validation_dom = needs_validation_dom || has_form_overrides;
  let validation_dom = needs_validation_dom.then(|| ValidationDomIndex::build(root, interaction_state));

  let mut dom_ptr_lookup: HashMap<*const DomNode, &StyledNode> = HashMap::new();
  for styled in lookup.values() {
    let styled = *styled;
    dom_ptr_lookup.insert(&styled.node as *const DomNode, styled);
  }

  let ctx = BuildContext {
    hidden,
    aria_hidden,
    node_scope,
    ids_by_scope,
    labels,
    lookup,
    dom_ptr_lookup,
    validation_dom,
    interaction_state,
    aria_owned_children,
    aria_owned_by,
    deadline_counter: Cell::new(0),
    deadline_error: RefCell::new(None),
  };

  // Expose a more browser-like root document node by deriving its accessible name from the first
  // HTML `<title>` element, even though `<head>/<title>` are typically display:none.
  //
  // Use referenced-mode traversal so visually hidden nodes still contribute, while respecting
  // `aria-hidden`/`inert` so titles hidden from assistive technology are ignored.
  let document_title = {
    let mut stack: Vec<&StyledNode> = vec![root];
    let mut title_node: Option<&StyledNode> = None;

    while let Some(node) = stack.pop() {
      if ctx.deadline_tripped() {
        break;
      }
      ctx.deadline_step(RenderStage::BoxTree);
      if ctx.deadline_tripped() {
        break;
      }

      // If a subtree is hidden from assistive technology via `aria-hidden`/`inert`, it cannot
      // contribute a document title.
      if ctx.is_accessibility_hidden(node) {
        continue;
      }

      // Shadow roots are not part of the document tree, and `<template>` contents are inert.
      if matches!(node.node.node_type, DomNodeType::ShadowRoot { .. })
        || node.node.template_contents_are_inert()
      {
        continue;
      }

      if is_html_element(&node.node)
        && node
          .node
          .tag_name()
          .is_some_and(|tag| tag.eq_ignore_ascii_case("title"))
      {
        title_node = Some(node);
        break;
      }

      for child in node.children.iter().rev() {
        stack.push(child);
      }
    }

    title_node.and_then(|node| {
      let mut visited = HashSet::new();
      let title = ctx.text_content(node, &mut visited, TextAlternativeMode::Referenced);
      let title = normalize_whitespace(&title);
      (!title.is_empty()).then_some(title)
    })
  };

  let mut children = Vec::new();
  for child in ctx.tree_children(root) {
    children.extend(build_nodes(child, &ctx));
    if ctx.deadline_tripped() {
      break;
    }
  }

  if let Some(err) = ctx.deadline_error.borrow_mut().take() {
    return Err(Error::Render(err));
  }

  Ok(AccessibilityNode {
    node_id: root.node_id,
    role: "document".to_string(),
    role_description: None,
    name: document_title,
    description: None,
    value: None,
    level: None,
    html_tag: Some("document".to_string()),
    id: None,
    dom_node_id: root.node_id,
    relations: None,
    states: AccessibilityState::default(),
    children,
    #[cfg(any(debug_assertions, feature = "a11y_debug"))]
    debug: interaction_state.and_then(|state| {
      state.document_selection.as_ref().map(|selection| {
        AccessibilityDebugInfo {
          text_selection: None,
          document_selection: Some(debug_document_selection(selection)),
          document_has_selection: selection.has_highlight(),
        }
      })
    }),
  })
}

/// Serialize the accessibility tree to JSON for snapshot tests.
pub fn accessibility_tree_json(root: &StyledNode) -> serde_json::Value {
  match build_accessibility_tree(root, None) {
    Ok(tree) => serde_json::to_value(tree).unwrap_or(serde_json::Value::Null),
    Err(_) => serde_json::Value::Null,
  }
}

fn build_styled_lookup<'a>(
  root: &'a StyledNode,
  out: &mut HashMap<usize, &'a StyledNode>,
) -> Result<()> {
  let mut stack: Vec<&'a StyledNode> = vec![root];
  let mut counter = 0usize;
  while let Some(node) = stack.pop() {
    render_control::check_active_periodic(&mut counter, 1024, RenderStage::BoxTree)
      .map_err(Error::Render)?;
    out.insert(node.node_id, node);
    for child in node.children.iter().rev() {
      stack.push(child);
    }
  }
  Ok(())
}

fn composed_children<'a>(
  styled: &'a StyledNode,
  lookup: &HashMap<usize, &'a StyledNode>,
) -> Vec<&'a StyledNode> {
  // `<template>` contents are inert and must not appear in the composed/accessibility tree even if
  // author CSS overrides `template { display: block }`.
  if styled.node.template_contents_are_inert() {
    return Vec::new();
  }

  if let Some(shadow_root) = styled
    .children
    .iter()
    .find(|c| matches!(c.node.node_type, DomNodeType::ShadowRoot { .. }))
  {
    return vec![shadow_root];
  }

  if matches!(styled.node.node_type, DomNodeType::Slot { .. })
    && !styled.slotted_node_ids.is_empty()
  {
    let mut resolved: Vec<&'a StyledNode> = Vec::new();
    for id in &styled.slotted_node_ids {
      if let Some(node) = lookup.get(id) {
        resolved.push(*node);
      }
    }
    return resolved;
  }

  styled.children.iter().collect()
}

#[derive(Debug)]
struct ValidationDomIndex {
  /// Root of a fully-populated `DomNode` tree (unlike `StyledNode.node`, which is shallow).
  ///
  /// This is used for constraint-validation checks that require tree traversal, such as radio-group
  /// requiredness.
  root: Box<DomNode>,
  /// 1-indexed pre-order node id -> node pointer.
  node_by_id: Vec<*const DomNode>,
  /// 1-indexed pre-order node id -> parent node id (0 for root).
  parent_by_id: Vec<usize>,
}

impl ValidationDomIndex {
  fn build(root: &StyledNode, interaction_state: Option<&InteractionState>) -> Self {
    let mut root = Box::new(clone_dom_subtree(root));
    if let Some(state) = interaction_state.filter(|state| state.form_state().has_overrides()) {
      apply_form_state_overrides(&mut root, state);
    }
    let mut node_by_id: Vec<*const DomNode> = Vec::new();
    let mut parent_by_id: Vec<usize> = Vec::new();
    node_by_id.push(std::ptr::null());
    parent_by_id.push(0);

    let mut stack: Vec<(*const DomNode, usize)> = Vec::new();
    stack.push((&*root as *const DomNode, 0));

    while let Some((ptr, parent)) = stack.pop() {
      let node_id = node_by_id.len();
      node_by_id.push(ptr);
      parent_by_id.push(parent);

      // Safety: pointers are derived from `root` and `root` is owned by `self`.
      let node = unsafe { &*ptr };
      for child in node.children.iter().rev() {
        stack.push((child as *const DomNode, node_id));
      }
    }

    Self {
      root,
      node_by_id,
      parent_by_id,
    }
  }

  fn with_element_ref<R>(&self, node_id: usize, f: impl FnOnce(ElementRef<'_>) -> R) -> Option<R> {
    let ptr = *self.node_by_id.get(node_id)?;
    if ptr.is_null() {
      return None;
    }

    let mut ancestors: Vec<&DomNode> = Vec::new();
    let mut current = *self.parent_by_id.get(node_id).unwrap_or(&0);
    while current != 0 {
      let ptr = *self.node_by_id.get(current).unwrap_or(&std::ptr::null());
      if ptr.is_null() {
        break;
      }
      // Safety: pointers are derived from `root` and remain valid for the duration of the call.
      ancestors.push(unsafe { &*ptr });
      current = *self.parent_by_id.get(current).unwrap_or(&0);
    }
    ancestors.reverse();

    // Safety: pointers are derived from `root` and remain valid for the duration of the call.
    let node = unsafe { &*ptr };
    Some(f(ElementRef::with_ancestors(node, ancestors.as_slice())))
  }
}

struct BuildContext<'a, 'state> {
  hidden: HashMap<usize, bool>,
  aria_hidden: HashMap<usize, bool>,
  node_scope: HashMap<usize, usize>,
  ids_by_scope: HashMap<usize, HashMap<String, usize>>,
  labels: HashMap<usize, Vec<usize>>,
  lookup: HashMap<usize, &'a StyledNode>,
  /// Fast lookup from `DomNode` pointers (as stored in `StyledNode.node`) to their owning
  /// `StyledNode`. This enables resolving relationships (e.g. landmark scoping) when only a DOM
  /// ancestor reference is available.
  dom_ptr_lookup: HashMap<*const DomNode, &'a StyledNode>,
  validation_dom: Option<ValidationDomIndex>,
  interaction_state: Option<&'state InteractionState>,
  aria_owned_children: HashMap<usize, Vec<usize>>,
  aria_owned_by: HashMap<usize, usize>,
  deadline_counter: Cell<usize>,
  deadline_error: RefCell<Option<crate::error::RenderError>>,
}

#[derive(Clone, Copy)]
enum TextAlternativeMode {
  /// Only include nodes that are visible in the rendered output.
  Visible,
  /// Include nodes that are hidden by CSS/HTML but not explicitly aria-hidden.
  Referenced,
}

#[derive(Clone, Copy)]
enum TextAltCollectKind {
  Children,
  Join,
}

enum TextAltEngineStart<'a> {
  Node {
    node: &'a StyledNode,
    mode: TextAlternativeMode,
    allow_name_from_content: Option<bool>,
  },
  Collect {
    nodes: Vec<&'a StyledNode>,
    kind: TextAltCollectKind,
    mode: TextAlternativeMode,
    allow_name_from_content: Option<bool>,
  },
}

impl<'a, 'state> BuildContext<'a, 'state> {
  fn is_hidden(&self, node: &StyledNode) -> bool {
    *self.hidden.get(&node.node_id).unwrap_or(&false)
  }

  /// Whether the node or any ancestor is hidden from assistive technology via
  /// `aria-hidden` or `inert`.
  fn is_accessibility_hidden(&self, node: &StyledNode) -> bool {
    *self.aria_hidden.get(&node.node_id).unwrap_or(&false)
  }

  fn deadline_tripped(&self) -> bool {
    self.deadline_error.borrow().is_some()
  }

  fn deadline_step(&self, stage: RenderStage) {
    if self.deadline_tripped() {
      return;
    }

    let mut counter = self.deadline_counter.get();
    counter = counter.wrapping_add(1);
    self.deadline_counter.set(counter);

    // Amortize expensive deadline checks while still making cancellation/timeouts effective for
    // large accessibility traversals.
    if counter % 1024 == 0 {
      if let Err(err) = render_control::check_active(stage) {
        *self.deadline_error.borrow_mut() = Some(err);
      }
    }
  }

  fn text_content(
    &self,
    node: &'a StyledNode,
    visited: &mut HashSet<usize>,
    mode: TextAlternativeMode,
  ) -> String {
    if self.deadline_tripped() {
      return String::new();
    }

    if !visited.insert(node.node_id) {
      return String::new();
    }

    if self.is_hidden_for_mode(node, mode) {
      return String::new();
    }

    match &node.node.node_type {
      DomNodeType::Text { content } => normalize_whitespace(content),
      DomNodeType::Document { .. } | DomNodeType::ShadowRoot { .. } => {
        self.subtree_text(node, visited, mode)
      }
      DomNodeType::Element { .. } | DomNodeType::Slot { .. } => {
        if node
          .node
          .tag_name()
          .is_some_and(|t| t.eq_ignore_ascii_case("script") || t.eq_ignore_ascii_case("style"))
        {
          return String::new();
        }

        self.subtree_text(node, visited, mode)
      }
    }
  }

  fn is_hidden_for_mode(&self, node: &StyledNode, mode: TextAlternativeMode) -> bool {
    match mode {
      TextAlternativeMode::Visible => self.is_hidden(node),
      // Referenced nodes (aria-labelledby/aria-describedby and HTML label associations) include
      // text from nodes that are visually hidden, but ignore nodes explicitly hidden from
      // assistive technology (`aria-hidden`/`inert`).
      TextAlternativeMode::Referenced => self.is_accessibility_hidden(node),
    }
  }

  fn node_for_id_scoped(&self, referrer_node_id: usize, id: &str) -> Option<&'a StyledNode> {
    let scope_id = self.node_scope.get(&referrer_node_id)?;
    let scoped_ids = self.ids_by_scope.get(scope_id)?;
    let node_id = scoped_ids.get(id)?;
    self.lookup.get(node_id).copied()
  }

  fn node_by_id(&self, id: usize) -> Option<&'a StyledNode> {
    self.lookup.get(&id).copied()
  }

  fn styled_for_dom_node(&self, node: &DomNode) -> Option<&'a StyledNode> {
    self.dom_ptr_lookup.get(&(node as *const DomNode)).copied()
  }

  fn composed_children(&self, node: &'a StyledNode) -> Vec<&'a StyledNode> {
    composed_children(node, &self.lookup)
  }

  /// Returns the children used for building the exported accessibility tree.
  ///
  /// This is the composed/flattened tree with `aria-owns` reparenting applied:
  /// - nodes that are targets of another element's `aria-owns` are removed from their original
  ///   location
  /// - owned targets are injected as children of the owning node in author-specified token order
  fn tree_children(&self, node: &'a StyledNode) -> Vec<&'a StyledNode> {
    let mut out: Vec<&'a StyledNode> = Vec::new();

    for child in self.composed_children(node) {
      if self.aria_owned_by.contains_key(&child.node_id) {
        continue;
      }
      out.push(child);
    }

    if let Some(owned) = self.aria_owned_children.get(&node.node_id) {
      for owned_id in owned {
        if let Some(target) = self.node_by_id(*owned_id) {
          if !self.is_hidden(target) {
            out.push(target);
          }
        }
      }
    }

    out
  }

  fn run_text_alternative_engine(
    &self,
    start: TextAltEngineStart<'a>,
    visited: &mut HashSet<usize>,
  ) -> Option<String> {
    #[derive(Clone, Copy)]
    enum NodeStep {
      Start,
      Element(ElementStep),
      AwaitCollect(AwaitCollect),
      AwaitNode(AwaitNode),
    }

    #[derive(Clone, Copy)]
    enum ElementStep {
      AriaLabelledBy,
      AriaLabel,
      Presentational,
      LabelAssociation,
      Placeholder,
      NativeName,
      RoleSpecific,
      NameFromContent,
      Alt,
      Fallback,
      Title,
      Done,
    }

    #[derive(Clone, Copy)]
    enum AwaitCollectKind {
      DocumentChildren,
      AriaLabelledBy,
      LabelAssociation,
      RoleSpecificButtonText,
      RoleSpecificOptionText,
      RoleSpecificFieldsetLegendText,
      RoleSpecificCaptionText,
      RoleSpecificFigcaptionText,
      RoleSpecificHeadingText,
      NameFromContentText,
      FallbackRoleOptionText,
    }

    #[derive(Clone, Copy)]
    struct AwaitCollect {
      kind: AwaitCollectKind,
      resume: Option<ElementStep>,
    }

    #[derive(Clone, Copy)]
    enum AwaitNodeKind {
      NativeName,
    }

    #[derive(Clone, Copy)]
    struct AwaitNode {
      kind: AwaitNodeKind,
      resume: ElementStep,
    }

    struct NodeFrame<'a> {
      node: &'a StyledNode,
      mode: TextAlternativeMode,
      allow_name_from_content: Option<bool>,
      tag: Option<String>,
      role: Option<String>,
      presentational: bool,
      step: NodeStep,
    }

    struct CollectFrame<'a> {
      kind: TextAltCollectKind,
      nodes: Vec<&'a StyledNode>,
      index: usize,
      mode: TextAlternativeMode,
      allow_name_from_content: Option<bool>,
      out: String,
      suppress_space: bool,
      awaiting_child: bool,
    }

    enum Frame<'a> {
      Node(NodeFrame<'a>),
      Collect(CollectFrame<'a>),
    }

    fn node_frame<'a>(
      node: &'a StyledNode,
      mode: TextAlternativeMode,
      allow_name_from_content: Option<bool>,
    ) -> Frame<'a> {
      Frame::Node(NodeFrame {
        node,
        mode,
        allow_name_from_content,
        tag: None,
        role: None,
        presentational: false,
        step: NodeStep::Start,
      })
    }

    fn collect_frame<'a>(
      nodes: Vec<&'a StyledNode>,
      kind: TextAltCollectKind,
      mode: TextAlternativeMode,
      allow_name_from_content: Option<bool>,
    ) -> Frame<'a> {
      Frame::Collect(CollectFrame {
        kind,
        nodes,
        index: 0,
        mode,
        allow_name_from_content,
        out: String::new(),
        suppress_space: false,
        awaiting_child: false,
      })
    }

    let mut stack: Vec<Frame<'a>> = Vec::new();
    match start {
      TextAltEngineStart::Node {
        node,
        mode,
        allow_name_from_content,
      } => stack.push(node_frame(node, mode, allow_name_from_content)),
      TextAltEngineStart::Collect {
        nodes,
        kind,
        mode,
        allow_name_from_content,
      } => stack.push(collect_frame(nodes, kind, mode, allow_name_from_content)),
    }

    // Outer `Option` indicates whether a value is pending. Inner `Option` matches the return type
    // of `text_alternative` (Some(text) or None).
    let mut pending: Option<Option<String>> = None;

    while let Some(frame) = stack.pop() {
      if self.deadline_tripped() {
        return Some(String::new());
      }
      self.deadline_step(RenderStage::BoxTree);
      if self.deadline_tripped() {
        return Some(String::new());
      }

      match frame {
        Frame::Collect(mut frame) => {
          if frame.awaiting_child {
            let child_value = pending.take().unwrap_or(None);
            if let Some(text) = child_value {
              if !text.is_empty() {
                if !frame.out.is_empty() && !frame.suppress_space {
                  frame.out.push(' ');
                }
                frame.suppress_space = false;
                frame.out.push_str(&text);
              }
            }
            frame.awaiting_child = false;
            stack.push(Frame::Collect(frame));
            continue;
          }

          if frame.index >= frame.nodes.len() {
            let normalized = normalize_whitespace(&frame.out);
            pending = Some(Some(normalized));
            continue;
          }

          let child = frame.nodes[frame.index];
          frame.index += 1;

          if matches!(frame.kind, TextAltCollectKind::Children)
            && child
              .node
              .tag_name()
              .is_some_and(|tag| tag.eq_ignore_ascii_case("wbr"))
          {
            frame.suppress_space = true;
            stack.push(Frame::Collect(frame));
            continue;
          }

          frame.awaiting_child = true;
          let child_frame = node_frame(child, frame.mode, frame.allow_name_from_content);
          stack.push(Frame::Collect(frame));
          stack.push(child_frame);
        }
        Frame::Node(mut frame) => match frame.step {
          NodeStep::Start => {
            if !visited.insert(frame.node.node_id) {
              pending = Some(Some(String::new()));
              continue;
            }

            if self.is_hidden_for_mode(frame.node, frame.mode) {
              pending = Some(Some(String::new()));
              continue;
            }

            match &frame.node.node.node_type {
              DomNodeType::Text { content } => {
                pending = Some(Some(normalize_whitespace(content)));
              }
              DomNodeType::Document { .. } | DomNodeType::ShadowRoot { .. } => {
                let children = self.composed_children(frame.node);
                let mode = frame.mode;
                frame.step = NodeStep::AwaitCollect(AwaitCollect {
                  kind: AwaitCollectKind::DocumentChildren,
                  resume: None,
                });
                stack.push(Frame::Node(frame));
                stack.push(collect_frame(
                  children,
                  TextAltCollectKind::Children,
                  mode,
                  None,
                ));
              }
              DomNodeType::Element { .. } | DomNodeType::Slot { .. } => {
                frame.tag = frame.node.node.tag_name().map(|t| t.to_ascii_lowercase());
                let (role, presentational, _) = compute_role(frame.node, &[], None, Some(self));
                frame.role = role;
                frame.presentational = presentational;

                // Script/style never contribute to the text alternative.
                if frame.tag.as_deref().is_some_and(|t| {
                  t.eq_ignore_ascii_case("script") || t.eq_ignore_ascii_case("style")
                }) {
                  pending = Some(Some(String::new()));
                  continue;
                }

                frame.step = NodeStep::Element(ElementStep::AriaLabelledBy);
                stack.push(Frame::Node(frame));
              }
            }
          }
          NodeStep::Element(step) => {
            let node = frame.node;
            let tag = frame.tag.as_deref();
            let role = frame.role.as_deref();

            match step {
              ElementStep::AriaLabelledBy => {
                if let Some(labelledby) = node.node.get_attribute_ref("aria-labelledby") {
                  let mut targets: Vec<&'a StyledNode> = Vec::new();
                  let mut seen_tokens: HashSet<&str> = HashSet::new();
                  for id in split_ascii_whitespace(labelledby) {
                    if !seen_tokens.insert(id) {
                      continue;
                    }
                    if let Some(target) = self.node_for_id_scoped(node.node_id, id) {
                      targets.push(target);
                    }
                  }

                  frame.step = NodeStep::AwaitCollect(AwaitCollect {
                    kind: AwaitCollectKind::AriaLabelledBy,
                    resume: None,
                  });
                  stack.push(Frame::Node(frame));
                  stack.push(collect_frame(
                    targets,
                    TextAltCollectKind::Join,
                    TextAlternativeMode::Referenced,
                    Some(true),
                  ));
                  continue;
                }

                frame.step = NodeStep::Element(ElementStep::AriaLabel);
                stack.push(Frame::Node(frame));
              }
              ElementStep::AriaLabel => {
                if let Some(label) = node.node.get_attribute_ref("aria-label") {
                  pending = Some(Some(normalize_whitespace(label)));
                } else {
                  frame.step = NodeStep::Element(ElementStep::Presentational);
                  stack.push(Frame::Node(frame));
                }
              }
              ElementStep::Presentational => {
                // The text alternative engine recomputes presentational state without the DOM
                // ancestor chain, so it cannot always see conditions that cause `role="none"` /
                // `role="presentation"` to be honored (e.g. form controls disabled via ancestor
                // `<fieldset disabled>`).
                //
                // `compute_name` passes `allow_name_from_content = Some(false)` when the caller has
                // already determined the presentational role is honored. Use that signal to
                // suppress all fallback name sources (placeholder/title, etc.) so the element cannot
                // leak a name and get exposed as a generic node.
                //
                // Note: `allow_name_from_content = Some(false)` is also used for other callers
                // (e.g. determining whether `section`/`form` should expose implicit landmark roles),
                // so only apply this suppression when the element actually declares a
                // presentational role token *and* the token could be honored (i.e., it's not
                // immediately disallowed by global ARIA attributes or focusability).
                let has_presentational_role_attr = node
                  .node
                  .get_attribute_ref("role")
                  .is_some_and(|raw| {
                    raw.split_ascii_whitespace().any(|token| {
                      token.eq_ignore_ascii_case("none")
                        || token.eq_ignore_ascii_case("presentation")
                    })
                  });
                let presentational_globally_allowed = !has_global_aria_attributes(&node.node);
                let locally_focusable = focusable_for_presentational_role(&node.node, &[]);
                // Elements that can be disabled via ancestor `<fieldset disabled>` can appear
                // focusable when ancestors are unavailable (e.g. `tabindex="0"`), even though the
                // browser will treat them as disabled and thus allow the presentational role.
                let focusability_may_depend_on_ancestors = supports_disabled(&node.node);

                if frame.allow_name_from_content == Some(false)
                  && has_presentational_role_attr
                  && presentational_globally_allowed
                  && (!locally_focusable || focusability_may_depend_on_ancestors)
                {
                  pending = Some(None);
                } else {
                  frame.step = NodeStep::Element(ElementStep::LabelAssociation);
                  stack.push(Frame::Node(frame));
                }
              }
              ElementStep::LabelAssociation => {
                if is_labelable(&node.node) {
                  if let Some(label_ids) = self.labels.get(&node.node_id) {
                    let mut label_nodes: Vec<&'a StyledNode> = Vec::new();
                    for label_id in label_ids {
                      if let Some(label_node) = self.node_by_id(*label_id) {
                        label_nodes.push(label_node);
                      }
                    }

                    if !label_nodes.is_empty() {
                      frame.step = NodeStep::AwaitCollect(AwaitCollect {
                        kind: AwaitCollectKind::LabelAssociation,
                        resume: Some(ElementStep::Placeholder),
                      });
                      stack.push(Frame::Node(frame));
                      stack.push(collect_frame(
                        label_nodes,
                        TextAltCollectKind::Join,
                        TextAlternativeMode::Referenced,
                        None,
                      ));
                      continue;
                    }
                  }
                }

                frame.step = NodeStep::Element(ElementStep::Placeholder);
                stack.push(Frame::Node(frame));
              }
              ElementStep::Placeholder => {
                if let Some(placeholder) = placeholder_as_name(node, self) {
                  pending = Some(Some(placeholder));
                } else {
                  frame.step = NodeStep::Element(ElementStep::NativeName);
                  stack.push(Frame::Node(frame));
                }
              }
              ElementStep::NativeName => {
                let child = match tag {
                  Some("fieldset") => first_child_with_tag(node, self, "legend", false, frame.mode),
                  Some("figure") => {
                    first_child_with_tag(node, self, "figcaption", true, frame.mode)
                  }
                  Some("table") => first_child_with_tag(node, self, "caption", true, frame.mode),
                  _ => None,
                };

                if let Some(child) = child {
                  let mode = frame.mode;
                  frame.step = NodeStep::AwaitNode(AwaitNode {
                    kind: AwaitNodeKind::NativeName,
                    resume: ElementStep::RoleSpecific,
                  });
                  stack.push(Frame::Node(frame));
                  stack.push(node_frame(child, mode, None));
                  continue;
                }

                frame.step = NodeStep::Element(ElementStep::RoleSpecific);
                stack.push(Frame::Node(frame));
              }
              ElementStep::RoleSpecific => {
                if !frame.presentational {
                  match tag {
                    Some("img") => {
                      if let Some(alt) = node.node.get_attribute_ref("alt") {
                        let norm = normalize_whitespace(alt);
                        if !norm.is_empty() {
                          pending = Some(Some(norm));
                          continue;
                        }
                      }
                    }
                    Some("input") => {
                      let input_type = node
                        .node
                        .get_attribute_ref("type")
                        .map(|t| t.to_ascii_lowercase())
                        .unwrap_or_else(|| "text".to_string());

                      if matches!(input_type.as_str(), "button" | "submit" | "reset") {
                        let label = node
                          .node
                          .get_attribute_ref("value")
                          .map(normalize_whitespace)
                          .filter(|value| !value.is_empty())
                          .or_else(|| default_button_label(&input_type).map(|s| s.to_string()));
                        if let Some(label) = label {
                          if !label.is_empty() {
                            pending = Some(Some(label));
                            continue;
                          }
                        }
                      }

                      if input_type == "image" {
                        let label = node
                          .node
                          .get_attribute_ref("alt")
                          .map(normalize_whitespace)
                          .or_else(|| {
                            node
                              .node
                              .get_attribute_ref("value")
                              .map(normalize_whitespace)
                          });
                        if let Some(label) = label {
                          if !label.is_empty() {
                            pending = Some(Some(label));
                            continue;
                          }
                        }
                      }
                    }
                    Some("button") => {
                      frame.step = NodeStep::AwaitCollect(AwaitCollect {
                        kind: AwaitCollectKind::RoleSpecificButtonText,
                        resume: Some(ElementStep::NameFromContent),
                      });
                      let children = self.composed_children(node);
                      let mode = frame.mode;
                      stack.push(Frame::Node(frame));
                      stack.push(collect_frame(
                        children,
                        TextAltCollectKind::Children,
                        mode,
                        None,
                      ));
                      continue;
                    }
                    Some("option") => {
                      if let Some(label) = node.node.get_attribute_ref("label") {
                        if label.is_empty() {
                          frame.step = NodeStep::AwaitCollect(AwaitCollect {
                            kind: AwaitCollectKind::RoleSpecificOptionText,
                            resume: Some(ElementStep::NameFromContent),
                          });
                          let children = self.composed_children(node);
                          let mode = frame.mode;
                          stack.push(Frame::Node(frame));
                          stack.push(collect_frame(
                            children,
                            TextAltCollectKind::Children,
                            mode,
                            None,
                          ));
                          continue;
                        }

                        let norm = normalize_whitespace(label);
                        if !norm.is_empty() {
                          pending = Some(Some(norm));
                          continue;
                        }
                      } else {
                        frame.step = NodeStep::AwaitCollect(AwaitCollect {
                          kind: AwaitCollectKind::RoleSpecificOptionText,
                          resume: Some(ElementStep::NameFromContent),
                        });
                        let children = self.composed_children(node);
                        let mode = frame.mode;
                        stack.push(Frame::Node(frame));
                        stack.push(collect_frame(
                          children,
                          TextAltCollectKind::Children,
                          mode,
                          None,
                        ));
                        continue;
                      }
                    }
                    Some("fieldset") => {
                      let legend = self.composed_children(node).into_iter().find(|child| {
                        child
                          .node
                          .tag_name()
                          .map(|t| t.eq_ignore_ascii_case("legend"))
                          .unwrap_or(false)
                      });
                      if let Some(legend) = legend {
                        if !self.is_hidden_for_mode(legend, frame.mode) {
                          frame.step = NodeStep::AwaitCollect(AwaitCollect {
                            kind: AwaitCollectKind::RoleSpecificFieldsetLegendText,
                            resume: Some(ElementStep::NameFromContent),
                          });
                          let children = self.composed_children(legend);
                          let mode = frame.mode;
                          stack.push(Frame::Node(frame));
                          stack.push(collect_frame(
                            children,
                            TextAltCollectKind::Children,
                            mode,
                            None,
                          ));
                          continue;
                        }
                      }
                    }
                    Some("table") => {
                      let caption = self.composed_children(node).into_iter().find(|child| {
                        child
                          .node
                          .tag_name()
                          .map(|t| t.eq_ignore_ascii_case("caption"))
                          .unwrap_or(false)
                      });
                      if let Some(caption) = caption {
                        frame.step = NodeStep::AwaitCollect(AwaitCollect {
                          kind: AwaitCollectKind::RoleSpecificCaptionText,
                          resume: Some(ElementStep::NameFromContent),
                        });
                        let children = self.composed_children(caption);
                        let mode = frame.mode;
                        stack.push(Frame::Node(frame));
                        stack.push(collect_frame(
                          children,
                          TextAltCollectKind::Children,
                          mode,
                          None,
                        ));
                        continue;
                      }
                    }
                    Some("figure") => {
                      let figcaption = self.composed_children(node).into_iter().find(|child| {
                        child
                          .node
                          .tag_name()
                          .map(|t| t.eq_ignore_ascii_case("figcaption"))
                          .unwrap_or(false)
                      });
                      if let Some(figcaption) = figcaption {
                        frame.step = NodeStep::AwaitCollect(AwaitCollect {
                          kind: AwaitCollectKind::RoleSpecificFigcaptionText,
                          resume: Some(ElementStep::NameFromContent),
                        });
                        let children = self.composed_children(figcaption);
                        let mode = frame.mode;
                        stack.push(Frame::Node(frame));
                        stack.push(collect_frame(
                          children,
                          TextAltCollectKind::Children,
                          mode,
                          None,
                        ));
                        continue;
                      }
                    }
                    _ => {
                      if role == Some("heading") {
                        frame.step = NodeStep::AwaitCollect(AwaitCollect {
                          kind: AwaitCollectKind::RoleSpecificHeadingText,
                          resume: Some(ElementStep::NameFromContent),
                        });
                        let children = self.composed_children(node);
                        let mode = frame.mode;
                        stack.push(Frame::Node(frame));
                        stack.push(collect_frame(
                          children,
                          TextAltCollectKind::Children,
                          mode,
                          None,
                        ));
                        continue;
                      }
                    }
                  }
                }

                frame.step = NodeStep::Element(ElementStep::NameFromContent);
                stack.push(Frame::Node(frame));
              }
              ElementStep::NameFromContent => {
                let mut allows_content =
                  self.allows_name_from_content(node, role, frame.allow_name_from_content)
                    && allows_visible_text_name(tag, role);

                if allows_content
                  && is_html_element(&node.node)
                  && tag.is_some_and(|t| t.eq_ignore_ascii_case("dialog"))
                  && node
                    .node
                    .get_attribute_ref("title")
                    .map(normalize_whitespace)
                    .is_some_and(|t| !t.is_empty())
                {
                  allows_content = false;
                }

                if allows_content {
                  frame.step = NodeStep::AwaitCollect(AwaitCollect {
                    kind: AwaitCollectKind::NameFromContentText,
                    resume: Some(ElementStep::Alt),
                  });
                  let children = self.composed_children(node);
                  let mode = frame.mode;
                  stack.push(Frame::Node(frame));
                  stack.push(collect_frame(
                    children,
                    TextAltCollectKind::Children,
                    mode,
                    None,
                  ));
                  continue;
                }

                frame.step = NodeStep::Element(ElementStep::Alt);
                stack.push(Frame::Node(frame));
              }
              ElementStep::Alt => {
                if let Some(alt) = node.node.get_attribute_ref("alt") {
                  if alt_applies(tag, role, &node.node) {
                    let norm = normalize_whitespace(alt);
                    if !norm.is_empty() {
                      pending = Some(Some(norm));
                      continue;
                    }
                  }
                }

                frame.step = NodeStep::Element(ElementStep::Fallback);
                stack.push(Frame::Node(frame));
              }
              ElementStep::Fallback => {
                if role == Some("option") {
                  if let Some(label) = node.node.get_attribute_ref("label") {
                    if label.is_empty() {
                      frame.step = NodeStep::AwaitCollect(AwaitCollect {
                        kind: AwaitCollectKind::FallbackRoleOptionText,
                        resume: Some(ElementStep::Title),
                      });
                      let children = self.composed_children(node);
                      let mode = frame.mode;
                      stack.push(Frame::Node(frame));
                      stack.push(collect_frame(
                        children,
                        TextAltCollectKind::Children,
                        mode,
                        None,
                      ));
                      continue;
                    }

                    let norm = normalize_whitespace(label);
                    if !norm.is_empty() {
                      pending = Some(Some(norm));
                      continue;
                    }
                  } else {
                    frame.step = NodeStep::AwaitCollect(AwaitCollect {
                      kind: AwaitCollectKind::FallbackRoleOptionText,
                      resume: Some(ElementStep::Title),
                    });
                    let children = self.composed_children(node);
                    let mode = frame.mode;
                    stack.push(Frame::Node(frame));
                    stack.push(collect_frame(
                      children,
                      TextAltCollectKind::Children,
                      mode,
                      None,
                    ));
                    continue;
                  }
                }

                frame.step = NodeStep::Element(ElementStep::Title);
                stack.push(Frame::Node(frame));
              }
              ElementStep::Title => {
                if let Some(title) = node.node.get_attribute_ref("title") {
                  let norm = normalize_whitespace(title);
                  if !norm.is_empty() {
                    pending = Some(Some(norm));
                    continue;
                  }
                }

                frame.step = NodeStep::Element(ElementStep::Done);
                stack.push(Frame::Node(frame));
              }
              ElementStep::Done => {
                pending = Some(None);
              }
            }
          }
          NodeStep::AwaitCollect(awaited) => {
            let collected = pending.take().unwrap_or(None).unwrap_or_default();

            match awaited.kind {
              AwaitCollectKind::DocumentChildren | AwaitCollectKind::AriaLabelledBy => {
                pending = Some(Some(collected));
              }
              AwaitCollectKind::LabelAssociation => {
                if !collected.is_empty() {
                  pending = Some(Some(collected));
                } else if let Some(resume) = awaited.resume {
                  frame.step = NodeStep::Element(resume);
                  stack.push(Frame::Node(frame));
                } else {
                  pending = Some(None);
                }
              }
              AwaitCollectKind::RoleSpecificButtonText => {
                if !collected.is_empty() {
                  pending = Some(Some(collected));
                } else if let Some(value) = frame.node.node.get_attribute_ref("value") {
                  let norm = normalize_whitespace(value);
                  if !norm.is_empty() {
                    pending = Some(Some(norm));
                  } else if let Some(resume) = awaited.resume {
                    frame.step = NodeStep::Element(resume);
                    stack.push(Frame::Node(frame));
                  } else {
                    pending = Some(None);
                  }
                } else if let Some(resume) = awaited.resume {
                  frame.step = NodeStep::Element(resume);
                  stack.push(Frame::Node(frame));
                } else {
                  pending = Some(None);
                }
              }
              AwaitCollectKind::RoleSpecificOptionText
              | AwaitCollectKind::RoleSpecificFieldsetLegendText
              | AwaitCollectKind::RoleSpecificCaptionText
              | AwaitCollectKind::RoleSpecificFigcaptionText
              | AwaitCollectKind::RoleSpecificHeadingText
              | AwaitCollectKind::NameFromContentText
              | AwaitCollectKind::FallbackRoleOptionText => {
                if !collected.is_empty() {
                  pending = Some(Some(collected));
                } else if let Some(resume) = awaited.resume {
                  frame.step = NodeStep::Element(resume);
                  stack.push(Frame::Node(frame));
                } else {
                  pending = Some(None);
                }
              }
            }
          }
          NodeStep::AwaitNode(awaited) => match awaited.kind {
            AwaitNodeKind::NativeName => {
              let child_text = pending.take().unwrap_or(None);
              if let Some(text) = child_text {
                if !text.is_empty() {
                  pending = Some(Some(text));
                  continue;
                }
              }
              frame.step = NodeStep::Element(awaited.resume);
              stack.push(Frame::Node(frame));
            }
          },
        },
      }
    }

    pending.unwrap_or(None)
  }

  fn subtree_text(
    &self,
    node: &'a StyledNode,
    visited: &mut HashSet<usize>,
    mode: TextAlternativeMode,
  ) -> String {
    self
      .run_text_alternative_engine(
        TextAltEngineStart::Collect {
          nodes: self.composed_children(node),
          kind: TextAltCollectKind::Children,
          mode,
          allow_name_from_content: None,
        },
        visited,
      )
      .unwrap_or_default()
  }

  fn allows_name_from_content(
    &self,
    node: &'a StyledNode,
    role: Option<&str>,
    allow_name_from_content: Option<bool>,
  ) -> bool {
    let tag = node.node.tag_name().map(|t| t.to_ascii_lowercase());
    let Some(allow) = allow_name_from_content else {
      let (computed_role, _, _) = compute_role(node, &[], None, Some(self));
      return role_allows_name_from_content(computed_role.as_deref(), tag.as_deref());
    };
    allow && role_allows_name_from_content(role, tag.as_deref())
  }

  fn text_alternative(
    &self,
    node: &'a StyledNode,
    visited: &mut HashSet<usize>,
    mode: TextAlternativeMode,
    allow_name_from_content: Option<bool>,
  ) -> Option<String> {
    self.run_text_alternative_engine(
      TextAltEngineStart::Node {
        node,
        mode,
        allow_name_from_content,
      },
      visited,
    )
  }
}

fn clone_dom_subtree(node: &StyledNode) -> DomNode {
  fn clone_shallow(styled: &StyledNode) -> DomNode {
    styled.node.clone_shallow()
  }

  struct Frame<'a> {
    src: &'a StyledNode,
    dst: *mut DomNode,
    next_child: usize,
  }

  let mut root = clone_shallow(node);
  let mut stack = vec![Frame {
    src: node,
    dst: &mut root as *mut DomNode,
    next_child: 0,
  }];

  while let Some(mut frame) = stack.pop() {
    // Safety: destination nodes are owned by `root` and its descendants, and we never mutate a
    // node's children while a frame borrowing that node is active. This keeps raw pointers stable
    // for the duration of the DFS clone.
    let dst = unsafe { &mut *frame.dst };
    let src = frame.src;

    if frame.next_child < src.children.len() {
      let child_src = &src.children[frame.next_child];
      frame.next_child += 1;

      dst.children.push(clone_shallow(child_src));
      let Some(child_dst) = dst.children.last_mut() else {
        debug_assert!(false, "child was just pushed");
        continue;
      };
      let child_dst = child_dst as *mut DomNode;

      stack.push(frame);
      stack.push(Frame {
        src: child_src,
        dst: child_dst,
        next_child: 0,
      });
    }
  }

  root
}

#[cfg(any(debug_assertions, feature = "a11y_debug"))]
fn debug_info_for_node(node: &StyledNode, ctx: &BuildContext<'_, '_>) -> Option<AccessibilityDebugInfo> {
  let state = ctx.interaction_state?;
  if !state.is_focused(node.node_id) {
    return None;
  }

  let tag = node.node.tag_name().map(|t| t.to_ascii_lowercase());
  if !matches!(tag.as_deref(), Some("input") | Some("textarea")) {
    return None;
  }

  let edit = state.text_edit_for(node.node_id)?;
  let (selection_start, selection_end) = match edit.selection {
    Some((start, end)) => (Some(start), Some(end)),
    None => (None, None),
  };

  Some(AccessibilityDebugInfo {
    text_selection: Some(AccessibilityTextSelection {
      caret: edit.caret,
      selection_start,
      selection_end,
    }),
    document_selection: state
      .document_selection
      .as_ref()
      .map(debug_document_selection),
    document_has_selection: state
      .document_selection
      .as_ref()
      .is_some_and(|sel| sel.has_highlight()),
  })
}

fn apply_form_state_overrides(root: &mut DomNode, interaction_state: &InteractionState) {
  if !interaction_state.form_state().has_overrides() {
    return;
  }

  #[derive(Clone, Copy)]
  struct Frame {
    ptr: *mut DomNode,
    next_child: usize,
    node_id: usize,
    select_override: Option<usize>,
  }

  // Depth-first pre-order traversal, matching `crate::dom::enumerate_dom_ids`.
  let mut stack: Vec<Frame> = Vec::new();
  stack.push(Frame {
    ptr: root as *mut DomNode,
    next_child: 0,
    node_id: 1,
    select_override: None,
  });
  let mut next_id = 2usize;

  while let Some(mut frame) = stack.pop() {
    // Safety: `root` is mutably borrowed for the duration of this traversal, and we never mutate any
    // `children` vectors while raw pointers are stored in `stack` (only element attributes), so
    // pointers remain valid.
    let node = unsafe { &mut *frame.ptr };

    if frame.next_child == 0 {
      let is_select = node
        .tag_name()
        .is_some_and(|t| t.eq_ignore_ascii_case("select"));
      let is_textarea = node
        .tag_name()
        .is_some_and(|t| t.eq_ignore_ascii_case("textarea"));
      let is_input = node
        .tag_name()
        .is_some_and(|t| t.eq_ignore_ascii_case("input"));
      let is_option = node
        .tag_name()
        .is_some_and(|t| t.eq_ignore_ascii_case("option"));

      // Propagate select override state to descendants.
      if is_select && interaction_state.form_state().select_selected.contains_key(&frame.node_id)
      {
        frame.select_override = Some(frame.node_id);
      }

      if let Some(value) = interaction_state.form_state().values.get(&frame.node_id) {
        if is_textarea {
          // Mirror the DOM layer's current-value representation used by painting and validation.
          node.set_attribute("data-fastr-value", value);
        } else if is_input {
          node.set_attribute("value", value);
        }
      }

      if let Some(checked) = interaction_state.form_state().checked.get(&frame.node_id).copied() {
        if is_input {
          let is_checkbox_or_radio = node.get_attribute_ref("type").is_some_and(|t| {
            t.eq_ignore_ascii_case("checkbox") || t.eq_ignore_ascii_case("radio")
          });
          if is_checkbox_or_radio {
            node.toggle_bool_attribute("checked", checked);
          }
        }
      }

      if is_input && node.get_attribute_ref("type").is_some_and(|t| t.eq_ignore_ascii_case("file")) {
        if let Some(value_string) = interaction_state
          .form_state()
          .file_input_value_string(frame.node_id)
        {
          if value_string.is_empty() {
            node.remove_attribute("data-fastr-file-value");
          } else {
            node.set_attribute("data-fastr-file-value", &value_string);
          }
        }
      }

      if let Some(select_id) = frame.select_override {
        if is_option {
          if let Some(selected) = interaction_state.form_state().select_selected.get(&select_id) {
            node.toggle_bool_attribute("selected", selected.contains(&frame.node_id));
          }
        }
      }
    }

    if frame.next_child < node.children.len() {
      // Safety: `next_child` is in bounds.
      let child_ptr = unsafe { node.children.as_mut_ptr().add(frame.next_child) };
      let child_id = next_id;
      next_id = next_id.saturating_add(1);

      let select_override = frame.select_override;
      frame.next_child += 1;
      stack.push(frame);
      stack.push(Frame {
        ptr: child_ptr,
        next_child: 0,
        node_id: child_id,
        select_override,
      });
    }
  }
}

fn build_nodes<'a, 'state>(node: &'a StyledNode, ctx: &BuildContext<'a, 'state>) -> Vec<AccessibilityNode> {
  if ctx.is_hidden(node) {
    return Vec::new();
  }
  if matches!(node.node.node_type, DomNodeType::Text { .. }) {
    return Vec::new();
  }

  struct Frame<'a> {
    node: &'a StyledNode,
    children: Vec<&'a StyledNode>,
    next_child: usize,
    built_children: Vec<AccessibilityNode>,
  }

  let mut dom_ancestors: Vec<&'a DomNode> = Vec::new();
  let mut styled_ancestors: Vec<&'a StyledNode> = Vec::new();
  let mut stack: Vec<Frame<'a>> = Vec::new();

  dom_ancestors.push(&node.node);
  styled_ancestors.push(node);
  stack.push(Frame {
    node,
    children: ctx.tree_children(node),
    next_child: 0,
    built_children: Vec::new(),
  });

  let mut root_output: Vec<AccessibilityNode> = Vec::new();

  while let Some(frame) = stack.last_mut() {
    if ctx.deadline_tripped() {
      break;
    }
    ctx.deadline_step(RenderStage::BoxTree);

    if frame.next_child < frame.children.len() {
      let child = frame.children[frame.next_child];
      frame.next_child += 1;

      if ctx.is_hidden(child) {
        continue;
      }

      if matches!(child.node.node_type, DomNodeType::Text { .. }) {
        continue;
      }

      dom_ancestors.push(&child.node);
      styled_ancestors.push(child);
      stack.push(Frame {
        node: child,
        children: ctx.tree_children(child),
        next_child: 0,
        built_children: Vec::new(),
      });
      continue;
    }

    let Some(finished) = stack.pop() else { break };
    let node = finished.node;
    let children = finished.built_children;

    // Pop the current node from the ancestor stacks so role computations see only the DOM/styled
    // ancestor chain (excluding the current node), matching the previous recursive implementation.
    dom_ancestors.pop();
    styled_ancestors.pop();

    let output = match node.node.node_type {
      DomNodeType::Text { .. } => Vec::new(),
      DomNodeType::Document { .. } | DomNodeType::ShadowRoot { .. } => children,
      DomNodeType::Element { .. } | DomNodeType::Slot { .. } => {
        // `StyledNode.node` is a shallow copy of the DOM node; its `children` are intentionally
        // empty.
        //
        // Most native accessibility state can be derived from element attributes alone, but some
        // constraint validation rules depend on descendant content (e.g. `<select>` uses `<option>`
        // descendants, and `<textarea>` uses its text contents). Reconstruct a minimal DOM subtree
        // for these controls so `ElementRef` validity helpers can see the required descendants.
        let needs_dom_subtree = node.node.tag_name().is_some_and(|tag| {
          tag.eq_ignore_ascii_case("select") || tag.eq_ignore_ascii_case("textarea")
        });
        let dom_subtree = needs_dom_subtree.then(|| clone_dom_subtree(node));
        let element_ref_node = dom_subtree.as_ref().unwrap_or(&node.node);
        let element_ref = ElementRef::with_ancestors(element_ref_node, dom_ancestors.as_slice());
        let (mut role, presentational_role, role_from_attr) = compute_role(
          node,
          dom_ancestors.as_slice(),
          styled_ancestors.last().copied(),
          Some(ctx),
        );

        // `<legend>` content is used to compute the accessible name for its owning `<fieldset>`; do
        // not expose it as a separate node unless the author explicitly assigns an ARIA role.
        if !role_from_attr
          && node
            .node
            .tag_name()
            .is_some_and(|t| t.eq_ignore_ascii_case("legend"))
          && dom_ancestors
            .last()
            .and_then(|parent| parent.tag_name())
            .is_some_and(|t| t.eq_ignore_ascii_case("fieldset"))
        {
          children
        } else {
          // HTML-AAM: `section` and `form` only expose implicit landmark roles when they have a
          // non-empty author-provided accessible name (not a name derived from their content).
          //
          // In particular, `aria-labelledby` blocks name-from-content fallback; if it resolves to an
          // empty string (missing IDs, referenced nodes hidden from AT, or whitespace-only text),
          // then the element is considered unnamed and must not expose the implicit landmark role.
          if !role_from_attr && matches!(role.as_deref(), Some("region") | Some("form")) {
            let author_name = landmark_author_name(node, ctx);
            let named = author_name.as_ref().is_some_and(|s| !s.is_empty());
            if !named {
              role = None;
            }
          }

          let role_description = compute_role_description(role.as_deref(), &node.node);

          let mut name = compute_name(node, ctx, !presentational_role);

          let native_disabled = compute_native_disabled(node, styled_ancestors.as_slice());
          let aria_disabled = parse_bool_attr(&node.node, "aria-disabled");
          let disabled = native_disabled || aria_disabled == Some(true);

          let native_required = element_ref.accessibility_required();
          let aria_required = parse_bool_attr(&node.node, "aria-required");
          let required = native_required || aria_required == Some(true);
          let invalid = parse_invalid(node, &element_ref, styled_ancestors.as_slice(), ctx);

          let mut description = compute_description(node, ctx, invalid, name.as_deref());
          let decorative_image = is_decorative_img(node, ctx);

          if decorative_image {
            role = None;
            name = None;
            description = None;
          }

          let checked = compute_checked(node, role.as_deref(), &element_ref, ctx);
          let selected =
            compute_selected(node, role.as_deref(), &element_ref, styled_ancestors.as_slice(), ctx);
          let pressed = compute_pressed(node, role.as_deref(), ctx);
          let busy = attr_truthy(&node.node, "aria-busy");
          let modal = compute_modal(&node.node);
          let current = parse_aria_current(&node.node);
          let expanded =
            compute_expanded(node, role.as_deref(), dom_ancestors.as_slice(), ctx.interaction_state);
          let mut has_popup = parse_has_popup(&node.node);
          if has_popup.is_none()
            && node
              .node
              .tag_name()
              .is_some_and(|t| t.eq_ignore_ascii_case("input"))
            && role.as_deref() == Some("combobox")
            && node.node.get_attribute_ref("aria-haspopup").is_none()
            && node
              .node
              .get_attribute_ref("list")
              .map(trim_ascii_whitespace)
              .is_some_and(|v| !v.is_empty())
          {
            has_popup = Some("listbox".to_string());
          }
          let multiline = compute_multiline(node, role.as_deref());
          let live = parse_aria_live(&node.node);
          let atomic = parse_bool_attr(&node.node, "aria-atomic");
          let relevant = parse_aria_relevant(&node.node);
          let visited = role.as_deref() == Some("link")
            && ctx
              .interaction_state
              .is_some_and(|state| state.is_visited_link(node.node_id));
          let focusable = compute_focusable(&node.node, role.as_deref(), disabled);
          let focused = !disabled
            && ctx
              .interaction_state
              .is_some_and(|state| state.is_focused(node.node_id));
          let focus_visible = focused
            && ctx
              .interaction_state
              .is_some_and(|state| state.focus_visible);
          let readonly = compute_readonly(&node.node, role.as_deref(), &element_ref);
          let value = compute_value(node, role.as_deref(), &element_ref, ctx);
          let level = compute_level(&node.node, role.as_deref());

          let states = AccessibilityState {
            focusable,
            focused,
            focus_visible,
            disabled,
            required,
            invalid,
            visited,
            busy,
            readonly,
            has_popup,
            multiline,
            checked,
            selected,
            pressed,
            expanded,
            current,
            modal,
            live,
            atomic,
            relevant,
          };

          let owns_children = ctx
            .aria_owned_children
            .get(&node.node_id)
            .is_some_and(|v| !v.is_empty());

          let should_expose = !decorative_image
            && (role.is_some()
              || name.is_some()
              || description.is_some()
              || value.is_some()
              || focusable
              || owns_children);
          if !should_expose {
            children
          } else {
            let role = role.unwrap_or_else(|| "generic".to_string());
            let html_tag = node.node.tag_name().map(|t| t.to_ascii_lowercase());
            let id = node
              .node
              .get_attribute_ref("id")
              .filter(|s| !s.is_empty())
              .map(|s| s.to_string());
            let relations = compute_relations(node, ctx, invalid);

            vec![AccessibilityNode {
              node_id: node.node_id,
              role,
              role_description,
              name,
              description,
              value,
              level,
              html_tag,
              id,
              dom_node_id: node.node_id,
              relations,
              states,
              children,
              #[cfg(any(debug_assertions, feature = "a11y_debug"))]
              debug: debug_info_for_node(node, ctx),
            }]
          }
        }
      }
    };

    if let Some(parent) = stack.last_mut() {
      parent.built_children.extend(output);
    } else {
      root_output = output;
    }
  }

  root_output
}

fn compute_hidden_and_scoped_ids(
  root: &StyledNode,
  hidden: &mut HashMap<usize, bool>,
  aria_hidden: &mut HashMap<usize, bool>,
  node_scope: &mut HashMap<usize, usize>,
  ids_by_scope: &mut HashMap<usize, HashMap<String, usize>>,
) -> Result<()> {
  struct Frame<'a> {
    node: &'a StyledNode,
    ancestor_hidden: bool,
    ancestor_aria_hidden: bool,
    scope_id: usize,
  }

  let mut stack: Vec<Frame<'_>> = vec![Frame {
    node: root,
    ancestor_hidden: false,
    ancestor_aria_hidden: false,
    scope_id: root.node_id,
  }];
  let mut counter = 0usize;

  while let Some(frame) = stack.pop() {
    render_control::check_active_periodic(&mut counter, 1024, RenderStage::BoxTree)
      .map_err(Error::Render)?;

    let node = frame.node;
    let is_hidden = frame.ancestor_hidden || is_node_hidden(node);
    let is_aria_hidden = frame.ancestor_aria_hidden || is_node_aria_hidden(node);
    hidden.insert(node.node_id, is_hidden);
    aria_hidden.insert(node.node_id, is_aria_hidden);
    node_scope.insert(node.node_id, frame.scope_id);

    if matches!(
      node.node.node_type,
      DomNodeType::Element { .. } | DomNodeType::Slot { .. }
    ) {
      if let Some(id) = node
        .node
        .get_attribute_ref("id")
        .filter(|value| !value.is_empty())
      {
        ids_by_scope
          .entry(frame.scope_id)
          .or_default()
          .entry(id.to_string())
          .or_insert(node.node_id);
      }
    }

    // `<template>` contents are inert and should not participate in accessibility traversal or
    // ARIA ID scoping. Do not descend into template children at all (even if CSS overrides template
    // display properties).
    if node.node.template_contents_are_inert() {
      continue;
    }

    for child in node.children.iter().rev() {
      let child_scope = match child.node.node_type {
        DomNodeType::ShadowRoot { .. } => child.node_id,
        _ => frame.scope_id,
      };
      stack.push(Frame {
        node: child,
        ancestor_hidden: is_hidden,
        ancestor_aria_hidden: is_aria_hidden,
        scope_id: child_scope,
      });
    }
  }

  Ok(())
}

fn collect_labels(
  root: &StyledNode,
  node_scope: &HashMap<usize, usize>,
  ids_by_scope: &HashMap<usize, HashMap<String, usize>>,
  lookup: &HashMap<usize, &StyledNode>,
) -> Result<HashMap<usize, Vec<usize>>> {
  fn node_id_for_id_scoped(
    node_scope: &HashMap<usize, usize>,
    ids_by_scope: &HashMap<usize, HashMap<String, usize>>,
    referrer_node_id: usize,
    id: &str,
  ) -> Option<usize> {
    let scope_id = node_scope.get(&referrer_node_id)?;
    let map = ids_by_scope.get(scope_id)?;
    map.get(id).copied()
  }

  /// HTML label containment is defined in terms of the DOM tree, not the composed tree.
  fn first_labelable_dom_descendant<'a>(
    node: &'a StyledNode,
    counter: &mut usize,
  ) -> Result<Option<&'a StyledNode>> {
    let mut stack: Vec<&'a StyledNode> = node.children.iter().rev().collect();
    while let Some(current) = stack.pop() {
      render_control::check_active_periodic(counter, 1024, RenderStage::BoxTree)
        .map_err(Error::Render)?;

      if matches!(current.node.node_type, DomNodeType::ShadowRoot { .. }) {
        continue;
      }

      if is_labelable(&current.node) {
        return Ok(Some(current));
      }

      if current.node.template_contents_are_inert() {
        continue;
      }

      for child in current.children.iter().rev() {
        stack.push(child);
      }
    }
    Ok(None)
  }

  let mut labels: HashMap<usize, Vec<usize>> = HashMap::new();
  let mut stack: Vec<&StyledNode> = vec![root];
  let mut counter = 0usize;

  while let Some(node) = stack.pop() {
    render_control::check_active_periodic(&mut counter, 1024, RenderStage::BoxTree)
      .map_err(Error::Render)?;

    let is_label = node
      .node
      .tag_name()
      .map(|t| t.eq_ignore_ascii_case("label"))
      .unwrap_or(false);

    if is_label {
      if let Some(for_attr) = node.node.get_attribute_ref("for") {
        let target_key = trim_ascii_whitespace(for_attr);
        if !target_key.is_empty() {
          if let Some(target_id) =
            node_id_for_id_scoped(node_scope, ids_by_scope, node.node_id, target_key)
          {
            if let Some(target_node) = lookup.get(&target_id) {
              if is_labelable(&target_node.node) {
                labels.entry(target_id).or_default().push(node.node_id);
              }
            }
          }
        }
      } else if let Some(target) = first_labelable_dom_descendant(node, &mut counter)? {
        labels.entry(target.node_id).or_default().push(node.node_id);
      }
    }

    if node.node.template_contents_are_inert() {
      continue;
    }

    for child in node.children.iter().rev() {
      stack.push(child);
    }
  }

  Ok(labels)
}

fn compute_aria_owns<'a>(
  root: &StyledNode,
  lookup: &HashMap<usize, &'a StyledNode>,
  hidden: &HashMap<usize, bool>,
  node_scope: &HashMap<usize, usize>,
  ids_by_scope: &HashMap<usize, HashMap<String, usize>>,
) -> Result<(HashMap<usize, Vec<usize>>, HashMap<usize, usize>)> {
  fn node_id_for_id_scoped(
    node_scope: &HashMap<usize, usize>,
    ids_by_scope: &HashMap<usize, HashMap<String, usize>>,
    referrer_node_id: usize,
    id: &str,
  ) -> Option<usize> {
    let scope_id = node_scope.get(&referrer_node_id)?;
    let map = ids_by_scope.get(scope_id)?;
    map.get(id).copied()
  }

  fn would_create_cycle(
    owner_id: usize,
    target_id: usize,
    composed_parent: &HashMap<usize, usize>,
    owned_by: &HashMap<usize, usize>,
  ) -> bool {
    if owner_id == target_id {
      return true;
    }

    let mut current = owner_id;
    // Guard against corrupt graphs: the composed tree is acyclic, but `aria-owns` edges are
    // user-authored and may attempt to introduce cycles. We only insert edges after checking for
    // cycles, so this should terminate quickly.
    let mut safety = 0usize;
    while safety < 1_000_000 {
      safety += 1;
      let parent = owned_by
        .get(&current)
        .copied()
        .or_else(|| composed_parent.get(&current).copied());
      let Some(parent) = parent else {
        return false;
      };
      if parent == target_id {
        return true;
      }
      current = parent;
    }

    true
  }

  let mut composed_parent: HashMap<usize, usize> = HashMap::new();
  let mut in_composed: HashSet<usize> = HashSet::new();
  let mut traversal_order: Vec<&StyledNode> = Vec::new();

  let mut stack: Vec<(&StyledNode, Option<usize>)> = vec![(root, None)];
  let mut counter = 0usize;

  while let Some((node, parent)) = stack.pop() {
    render_control::check_active_periodic(&mut counter, 1024, RenderStage::BoxTree)
      .map_err(Error::Render)?;

    if *hidden.get(&node.node_id).unwrap_or(&false) {
      continue;
    }

    if let Some(parent_id) = parent {
      composed_parent.insert(node.node_id, parent_id);
    }
    in_composed.insert(node.node_id);
    traversal_order.push(node);

    for child in composed_children(node, lookup).into_iter().rev() {
      stack.push((child, Some(node.node_id)));
    }
  }

  let mut aria_owned_children: HashMap<usize, Vec<usize>> = HashMap::new();
  let mut aria_owned_by: HashMap<usize, usize> = HashMap::new();

  let mut counter = 0usize;
  for owner in traversal_order {
    render_control::check_active_periodic(&mut counter, 1024, RenderStage::BoxTree)
      .map_err(Error::Render)?;

    if !matches!(
      owner.node.node_type,
      DomNodeType::Element { .. } | DomNodeType::Slot { .. }
    ) {
      continue;
    }

    let Some(attr) = owner.node.get_attribute_ref("aria-owns") else {
      continue;
    };

    let mut seen_tokens: HashSet<&str> = HashSet::new();
    for token in split_ascii_whitespace(attr) {
      if !seen_tokens.insert(token) {
        continue;
      }

      let Some(target_id) = node_id_for_id_scoped(node_scope, ids_by_scope, owner.node_id, token)
      else {
        continue;
      };

      if !in_composed.contains(&target_id) {
        continue;
      }

      if *hidden.get(&target_id).unwrap_or(&false) {
        continue;
      }

      if aria_owned_by.contains_key(&target_id) {
        continue;
      }

      if would_create_cycle(owner.node_id, target_id, &composed_parent, &aria_owned_by) {
        continue;
      }

      aria_owned_by.insert(target_id, owner.node_id);
      aria_owned_children
        .entry(owner.node_id)
        .or_default()
        .push(target_id);
    }
  }

  Ok((aria_owned_children, aria_owned_by))
}

fn is_decorative_img(node: &StyledNode, ctx: &BuildContext<'_, '_>) -> bool {
  let Some(tag) = node.node.tag_name().map(|t| t.to_ascii_lowercase()) else {
    return false;
  };
  if tag != "img" {
    return false;
  }

  let alt_empty = node
    .node
    .get_attribute_ref("alt")
    .map(normalize_whitespace)
    .is_some_and(|alt| alt.is_empty());
  if !alt_empty {
    return false;
  }

  if node.node.get_attribute_ref("aria-label").is_some()
    || node.node.get_attribute_ref("aria-labelledby").is_some()
  {
    return false;
  }

  if ctx.labels.contains_key(&node.node_id) {
    return false;
  }

  let parsed_role = parse_aria_role_attr(&node.node, &[]);

  if let Some(ParsedRole::Explicit(_)) = parsed_role {
    return false;
  }

  if node
    .node
    .get_attribute_ref("title")
    .map(normalize_whitespace)
    .is_some_and(|title| !title.is_empty())
    && parsed_role.is_none()
  {
    return false;
  }

  true
}

fn is_node_hidden(node: &StyledNode) -> bool {
  let attr_hidden = match node.node.node_type {
    DomNodeType::Element { .. } | DomNodeType::Slot { .. } => {
      node.node.get_attribute_ref("hidden").is_some()
    }
    _ => false,
  };

  attr_hidden
    || is_node_aria_hidden(node)
    || matches!(node.styles.display, Display::None)
    || node.styles.visibility != Visibility::Visible
}

fn is_node_aria_hidden(node: &StyledNode) -> bool {
  parse_bool_attr(&node.node, "aria-hidden").unwrap_or(false) || node.styles.inert
}

fn normalize_whitespace(input: &str) -> String {
  let mut out = String::new();
  let mut last_space = false;
  for ch in input.chars() {
    // Ignore zero-width characters that may be injected for break opportunities.
    if matches!(ch, '\u{200B}' | '\u{FEFF}' | '\u{2060}') {
      continue;
    }

    if is_html_ascii_whitespace(ch) {
      if !last_space {
        out.push(' ');
      }
      last_space = true;
    } else {
      out.push(ch);
      last_space = false;
    }
  }
  trim_ascii_whitespace(&out).to_string()
}

fn is_landmark_role(role: &str) -> bool {
  matches!(
    role,
    "article"
      | "banner"
      | "complementary"
      | "contentinfo"
      | "form"
      | "main"
      | "navigation"
      | "region"
      | "search"
  )
}

fn has_accessible_name_attr(node: &DomNode) -> bool {
  node
    .get_attribute_ref("aria-label")
    .is_some_and(|v| !trim_ascii_whitespace(v).is_empty())
    || node
      .get_attribute_ref("aria-labelledby")
      .is_some_and(|v| !trim_ascii_whitespace(v).is_empty())
    || node
      .get_attribute_ref("title")
      .is_some_and(|v| !trim_ascii_whitespace(v).is_empty())
}

fn landmark_author_name(node: &StyledNode, ctx: &BuildContext) -> Option<String> {
  if let Some(labelledby) = node.node.get_attribute_ref("aria-labelledby") {
    let mut visited = HashSet::new();
    visited.insert(node.node_id);
    return Some(referenced_text_attr(
      ctx,
      node.node_id,
      labelledby,
      &mut visited,
      TextAlternativeMode::Referenced,
    ));
  }

  if let Some(label) = node.node.get_attribute_ref("aria-label") {
    return Some(normalize_whitespace(label));
  }

  node
    .node
    .get_attribute_ref("title")
    .map(normalize_whitespace)
    .map(|s| s.to_string())
}

fn is_html_element(node: &DomNode) -> bool {
  matches!(node.namespace(), Some(ns) if ns.is_empty() || ns == HTML_NAMESPACE)
}

// HTML-AAM: banner/contentinfo (and other landmarks like main) only apply when the element is not
// scoped within other landmarks or sectioning contexts. Sectioning roots also bound these scopes,
// and forms only become landmarks when they are explicitly named.
fn has_landmark_ancestor(ancestors: &[&DomNode], ctx: Option<&BuildContext<'_, '_>>) -> bool {
  for (idx, ancestor) in ancestors.iter().enumerate() {
    if matches!(ancestor.node_type, DomNodeType::ShadowRoot { .. }) {
      return true;
    }

    if !is_html_element(ancestor) {
      continue;
    }

    if let Some(parsed) = parse_aria_role_attr(ancestor, &ancestors[..idx]) {
      if let ParsedRole::Explicit(role) = parsed {
        if is_landmark_role(&role) {
          return true;
        }
      }
    }

    let Some(tag) = ancestor.tag_name().map(|t| t.to_ascii_lowercase()) else {
      continue;
    };

    if matches!(
      tag.as_str(),
      "article" | "aside" | "main" | "nav" | "search" | "section"
    ) {
      return true;
    }

    if matches!(
      tag.as_str(),
      "blockquote" | "details" | "dialog" | "fieldset" | "figure" | "td"
    ) {
      return true;
    }

    if tag == "form" {
      // Forms only become landmark scopes when they are explicitly named. Mirror the `build_nodes`
      // gating behavior by requiring a non-empty author-provided accessible name (resolved).
      //
      // Fall back to the attribute-presence heuristic when no `BuildContext` is available.
      let is_named = if let Some(ctx) = ctx {
        if let Some(styled) = ctx.styled_for_dom_node(ancestor) {
          landmark_author_name(styled, ctx).is_some_and(|name| !name.is_empty())
        } else {
          // Defensive fallback: if the ancestor isn't part of the styled lookup for some reason,
          // preserve the old attribute-based behavior.
          has_accessible_name_attr(ancestor)
        }
      } else {
        has_accessible_name_attr(ancestor)
      };

      if is_named {
        return true;
      }
    }

  }
  false
}

enum ParsedRole {
  Explicit(String),
  Presentational,
}

// Keep this list in sync with the ARIA role tokens accepted by `parse_aria_role_attr`.
//
// We use an explicit allowlist so that FastRender's accessibility output is stable and we can
// deterministically map every emitted role (e.g. into AccessKit for native browser UI).
pub(crate) const FASTRENDER_VALID_ARIA_ROLE_TOKENS: &[&str] = &[
  "alert",
  "alertdialog",
  "application",
  "article",
  "banner",
  "button",
  "caption",
  "cell",
  "checkbox",
  "columnheader",
  "combobox",
  "definition",
  "complementary",
  "contentinfo",
  "dialog",
  "directory",
  "document",
  "feed",
  "figure",
  "form",
  "generic",
  "grid",
  "gridcell",
  "group",
  "heading",
  "img",
  "link",
  "list",
  "listbox",
  "listitem",
  "log",
  "main",
  "marquee",
  "math",
  "menu",
  "menubar",
  "menuitem",
  "menuitemcheckbox",
  "menuitemradio",
  "meter",
  "navigation",
  "none",
  "note",
  "option",
  "paragraph",
  "presentation",
  "progressbar",
  "radio",
  "radiogroup",
  "region",
  "row",
  "rowgroup",
  "rowheader",
  "search",
  "separator",
  "searchbox",
  "slider",
  "spinbutton",
  "status",
  "switch",
  "tab",
  "table",
  "tablist",
  "tabpanel",
  "term",
  "textbox",
  "timer",
  "toolbar",
  "tooltip",
  "tree",
  "treeitem",
  "treegrid",
];

fn is_supported_role(role: &str) -> bool {
  FASTRENDER_VALID_ARIA_ROLE_TOKENS.contains(&role)
}

fn parse_aria_role_attr(node: &DomNode, ancestors: &[&DomNode]) -> Option<ParsedRole> {
  let raw_role = node.get_attribute_ref("role")?;

  for token in raw_role.split_ascii_whitespace() {
    let role = token.to_ascii_lowercase();
    if role == "none" || role == "presentation" {
      if should_honor_presentational(node, ancestors) {
        return Some(ParsedRole::Presentational);
      }
      continue;
    }

    if is_supported_role(&role) {
      return Some(ParsedRole::Explicit(role));
    }
  }

  None
}

fn has_global_aria_attributes(node: &DomNode) -> bool {
  node.attributes_iter().any(|(name, _)| {
    let lower = name.to_ascii_lowercase();
    matches!(
      lower.as_str(),
      // ARIA global states/properties (excluding aria-hidden). These prevent the author from
      // stripping semantics via role="presentation"/"none" per the ARIA-in-HTML processing rules.
      "aria-activedescendant"
        | "aria-atomic"
        | "aria-busy"
        | "aria-controls"
        | "aria-current"
        | "aria-describedby"
        | "aria-description"
        | "aria-details"
        | "aria-disabled"
        | "aria-dropeffect"
        | "aria-errormessage"
        | "aria-flowto"
        | "aria-grabbed"
        | "aria-haspopup"
        | "aria-invalid"
        | "aria-keyshortcuts"
        | "aria-label"
        | "aria-labelledby"
        | "aria-live"
        | "aria-owns"
        | "aria-relevant"
        | "aria-roledescription"
    )
  })
}

fn focusable_for_presentational_role(node: &DomNode, ancestors: &[&DomNode]) -> bool {
  if !node.is_element() {
    return false;
  }

  // Disabled form controls are not focusable, even if `tabindex` is set.
  if ElementRef::with_ancestors(node, ancestors).accessibility_disabled() {
    return false;
  }

  if let Some(tabindex) = node.get_attribute_ref("tabindex") {
    let trimmed = trim_ascii_whitespace(tabindex);
    if !trimmed.is_empty() && trimmed.parse::<i32>().is_ok() {
      return true;
    }
  }

  let Some(tag) = node.tag_name().map(|t| t.to_ascii_lowercase()) else {
    return false;
  };

  if tag == "area" && node.get_attribute_ref("href").is_some() {
    return true;
  }

  if tag == "summary" {
    return true;
  }

  if tag == "a" && node.get_attribute_ref("href").is_some() {
    return true;
  }

  if matches!(tag.as_str(), "button" | "select" | "textarea") {
    return true;
  }

  if tag == "input" {
    let input_type = node
      .get_attribute_ref("type")
      .map(|t| t.to_ascii_lowercase())
      .unwrap_or_else(|| "text".to_string());
    return input_type != "hidden";
  }

  if tag == "option" {
    return true;
  }

  node
    .get_attribute_ref("contenteditable")
    .map(|v| v.is_empty() || v.eq_ignore_ascii_case("true"))
    .unwrap_or(false)
}

fn should_honor_presentational(node: &DomNode, ancestors: &[&DomNode]) -> bool {
  !has_global_aria_attributes(node) && !focusable_for_presentational_role(node, ancestors)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TableContext {
  Table,
  Grid,
  TreeGrid,
  Presentational,
}

fn table_context_for_descendant(ancestors: &[&DomNode]) -> Option<TableContext> {
  let Some((idx, table)) = ancestors.iter().enumerate().rev().find(|(_, ancestor)| {
    is_html_element(ancestor)
      && ancestor
        .tag_name()
        .is_some_and(|tag| tag.eq_ignore_ascii_case("table"))
  }) else {
    return None;
  };

  match parse_aria_role_attr(table, &ancestors[..idx]) {
    Some(ParsedRole::Presentational) => Some(TableContext::Presentational),
    Some(ParsedRole::Explicit(role)) => match role.as_str() {
      "grid" => Some(TableContext::Grid),
      "treegrid" => Some(TableContext::TreeGrid),
      "table" => Some(TableContext::Table),
      _ => None,
    },
    None => Some(TableContext::Table),
  }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ListContext {
  List,
  Presentational,
  NotList,
}

fn list_context_for_container(node: &DomNode) -> ListContext {
  match parse_aria_role_attr(node, &[]) {
    Some(ParsedRole::Presentational) => ListContext::Presentational,
    Some(ParsedRole::Explicit(role)) => {
      if role == "list" {
        ListContext::List
      } else {
        ListContext::NotList
      }
    }
    None => ListContext::List,
  }
}

fn nearest_html_ancestor_with_tag<'a>(
  ancestors: &'a [&DomNode],
  tags: &[&str],
) -> Option<&'a DomNode> {
  for ancestor in ancestors.iter().rev() {
    if !is_html_element(ancestor) {
      continue;
    }
    let Some(tag) = ancestor.tag_name() else {
      continue;
    };
    if tags.iter().any(|t| tag.eq_ignore_ascii_case(t)) {
      return Some(*ancestor);
    }
  }
  None
}
fn compute_role(
  node: &StyledNode,
  ancestors: &[&DomNode],
  styled_parent: Option<&StyledNode>,
  ctx: Option<&BuildContext<'_, '_>>,
) -> (Option<String>, bool, bool) {
  let dom_node = &node.node;

  if let Some(parsed) = parse_aria_role_attr(dom_node, ancestors) {
    match parsed {
      ParsedRole::Explicit(role) => return (Some(role), false, true),
      ParsedRole::Presentational => {
        return (None, true, true);
      }
    }
  }

  if !is_html_element(dom_node) {
    return (None, false, false);
  }

  let Some(tag) = dom_node.tag_name().map(|t| t.to_ascii_lowercase()) else {
    return (None, false, false);
  };

  let table_ctx = match tag.as_str() {
    "thead" | "tbody" | "tfoot" | "tr" | "td" | "th" | "caption" => {
      table_context_for_descendant(ancestors)
    }
    _ => None,
  };

  let role = match tag.as_str() {
    "a" => dom_node
      .get_attribute_ref("href")
      .map(|_| "link".to_string()),
    "area" => dom_node
      .get_attribute_ref("href")
      .map(|_| "link".to_string()),
    "button" => Some("button".to_string()),
    "summary" => is_details_summary(node, styled_parent).then(|| "button".to_string()),
    "input" => input_role(dom_node),
    "textarea" => Some("textbox".to_string()),
    "select" => Some(select_role(dom_node)),
    "datalist" => Some("listbox".to_string()),
    "optgroup" => Some("group".to_string()),
    "option" => Some("option".to_string()),
    "img" => Some("img".to_string()),
    "figcaption" => Some("caption".to_string()),
    "figure" => Some("figure".to_string()),
    "ul" | "ol" | "menu" => Some("list".to_string()),
    "menuitem" => Some("menuitem".to_string()),
    "li" => {
      let list_ancestor = nearest_html_ancestor_with_tag(ancestors, &["ul", "ol", "menu"]);
      match list_ancestor.map(list_context_for_container) {
        Some(ListContext::List) => Some("listitem".to_string()),
        _ => None,
      }
    }
    "dl" => Some("list".to_string()),
    "dt" => {
      let dl_ancestor = nearest_html_ancestor_with_tag(ancestors, &["dl"]);
      match dl_ancestor.map(list_context_for_container) {
        Some(ListContext::List) => Some("term".to_string()),
        _ => None,
      }
    }
    "dd" => {
      let dl_ancestor = nearest_html_ancestor_with_tag(ancestors, &["dl"]);
      match dl_ancestor.map(list_context_for_container) {
        Some(ListContext::List) => Some("definition".to_string()),
        _ => None,
      }
    }
    "table" => Some("table".to_string()),
    "thead" | "tbody" | "tfoot" => matches!(
      table_ctx,
      Some(TableContext::Table) | Some(TableContext::Grid) | Some(TableContext::TreeGrid)
    )
    .then(|| "rowgroup".to_string()),
    "tr" => matches!(
      table_ctx,
      Some(TableContext::Table) | Some(TableContext::Grid) | Some(TableContext::TreeGrid)
    )
    .then(|| "row".to_string()),
    "td" => match table_ctx {
      Some(TableContext::Table) => Some("cell".to_string()),
      Some(TableContext::Grid) | Some(TableContext::TreeGrid) => Some("gridcell".to_string()),
      _ => None,
    },
    "th" => matches!(
      table_ctx,
      Some(TableContext::Table) | Some(TableContext::Grid) | Some(TableContext::TreeGrid)
    )
    .then(|| header_role(dom_node))
    .flatten(),
    "caption" => matches!(
      table_ctx,
      Some(TableContext::Table) | Some(TableContext::Grid) | Some(TableContext::TreeGrid)
    )
    .then(|| "caption".to_string()),
    "progress" => Some("progressbar".to_string()),
    "meter" => Some("meter".to_string()),
    "output" => Some("status".to_string()),
    "details" => Some("group".to_string()),
    "fieldset" => Some("group".to_string()),
    "main" => {
      if has_landmark_ancestor(ancestors, ctx) {
        None
      } else {
        Some("main".to_string())
      }
    }
    "nav" => Some("navigation".to_string()),
    "search" => Some("search".to_string()),
    "header" => {
      if has_landmark_ancestor(ancestors, ctx) {
        None
      } else {
        Some("banner".to_string())
      }
    }
    "footer" => {
      if has_landmark_ancestor(ancestors, ctx) {
        None
      } else {
        Some("contentinfo".to_string())
      }
    }
    "aside" => Some("complementary".to_string()),
    "form" => Some("form".to_string()),
    "article" => Some("article".to_string()),
    "section" => Some("region".to_string()),
    "dialog" => Some("dialog".to_string()),
    "hr" => Some("separator".to_string()),
    "math" => Some("math".to_string()),
    "p" => Some("paragraph".to_string()),
    "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => Some("heading".to_string()),
    _ => None,
  };

  (role, false, false)
}

fn is_details_summary(node: &StyledNode, styled_parent: Option<&StyledNode>) -> bool {
  let Some(parent) = styled_parent else {
    return false;
  };
  if !is_html_element(&node.node) || !is_html_element(&parent.node) {
    return false;
  }

  if !parent
    .node
    .tag_name()
    .map(|t| t.eq_ignore_ascii_case("details"))
    .unwrap_or(false)
  {
    return false;
  }

  for child in &parent.children {
    if is_html_element(&child.node)
      && child
        .node
        .tag_name()
        .map(|t| t.eq_ignore_ascii_case("summary"))
        .unwrap_or(false)
    {
      return ptr::eq(child, node);
    }
  }

  false
}

fn input_role(node: &DomNode) -> Option<String> {
  let input_type = node
    .get_attribute_ref("type")
    .map(|t| t.to_ascii_lowercase())
    .filter(|t| !t.is_empty())
    .unwrap_or_else(|| "text".to_string());

  if input_type == "hidden" {
    return None;
  }

  let has_list = node
    .get_attribute_ref("list")
    .map(trim_ascii_whitespace)
    .is_some_and(|v| !v.is_empty());

  if has_list
    && matches!(
      input_type.as_str(),
      "text" | "search" | "url" | "email" | "tel"
    )
  {
    // HTML-AAM: `<input list=...>` is exposed as a combobox-like control.
    return Some("combobox".to_string());
  }

  match input_type.as_str() {
    "checkbox" => Some("checkbox".to_string()),
    "radio" => Some("radio".to_string()),
    "range" => Some("slider".to_string()),
    "number" => Some("spinbutton".to_string()),
    "search" => Some("searchbox".to_string()),
    "button" | "submit" | "reset" | "image" => Some("button".to_string()),
    _ => Some("textbox".to_string()),
  }
}

fn select_role(node: &DomNode) -> String {
  if crate::dom::select_is_listbox(node) {
    "listbox".to_string()
  } else {
    "combobox".to_string()
  }
}

fn header_role(node: &DomNode) -> Option<String> {
  if !node
    .tag_name()
    .map(|t| t.eq_ignore_ascii_case("th"))
    .unwrap_or(false)
  {
    return None;
  }
  let scope = node
    .get_attribute_ref("scope")
    .map(|s| s.to_ascii_lowercase())
    .unwrap_or_default();

  if scope == "row" || scope == "rowgroup" {
    Some("rowheader".to_string())
  } else {
    Some("columnheader".to_string())
  }
}

/// Whether the element can derive its accessible name from its subtree text.
fn role_allows_name_from_content(role: Option<&str>, tag: Option<&str>) -> bool {
  if matches!(
    role,
    Some("textbox")
      | Some("searchbox")
      | Some("combobox")
      | Some("listbox")
      | Some("spinbutton")
      | Some("slider")
      | Some("checkbox")
      | Some("radio")
      | Some("switch")
      | Some("progressbar")
      | Some("meter")
  ) {
    return false;
  }

  if role.is_some() {
    return true;
  }

  if let Some(tag) = tag {
    let tag = tag.to_ascii_lowercase();
    if matches!(
      tag.as_str(),
      "input" | "select" | "textarea" | "progress" | "meter"
    ) {
      return false;
    }
  }

  true
}

/// Compute the accessible name per the W3C Accessible Name and Description
/// Computation algorithm (https://www.w3.org/TR/accname-1.2/#computation).
fn compute_name(
  node: &StyledNode,
  ctx: &BuildContext<'_, '_>,
  allow_name_from_content: bool,
) -> Option<String> {
  let mut visited = HashSet::new();
  ctx.text_alternative(
    node,
    &mut visited,
    TextAlternativeMode::Visible,
    Some(allow_name_from_content),
  )
}

fn first_child_with_tag<'a, 'state>(
  node: &'a StyledNode,
  ctx: &BuildContext<'a, 'state>,
  tag: &str,
  require_visible: bool,
  mode: TextAlternativeMode,
) -> Option<&'a StyledNode> {
  for child in ctx.composed_children(node) {
    if require_visible && ctx.is_hidden_for_mode(child, mode) {
      continue;
    }

    let is_match = child
      .node
      .tag_name()
      .map(|t| t.eq_ignore_ascii_case(tag))
      .unwrap_or(false);
    if is_match {
      return Some(child);
    }
  }

  None
}

fn allows_visible_text_name(tag: Option<&str>, role: Option<&str>) -> bool {
  let tag_blocked = tag
    .map(|t| {
      let lower = t.to_ascii_lowercase();
      matches!(lower.as_str(), "fieldset" | "figure" | "table" | "select")
    })
    .unwrap_or(false);

  if tag_blocked {
    return false;
  }

  if let Some(role) = role {
    if role.eq_ignore_ascii_case("table")
      || role.eq_ignore_ascii_case("figure")
      || role.eq_ignore_ascii_case("combobox")
      || role.eq_ignore_ascii_case("listbox")
      || role.eq_ignore_ascii_case("dialog")
      || role.eq_ignore_ascii_case("alertdialog")
    {
      return false;
    }
  }

  true
}

fn control_value_text(node: &StyledNode, ctx: &BuildContext) -> Option<String> {
  let tag = node.node.tag_name()?.to_ascii_lowercase();
  match tag.as_str() {
    "input" => {
      let input_type = node
        .node
        .get_attribute_ref("type")
        .map(|t| t.to_ascii_lowercase())
        .unwrap_or_else(|| "text".to_string());
      if input_type == "hidden" {
        return None;
      }
      if input_type == "file" {
        if let Some(value) = ctx
          .interaction_state
          .and_then(|state| state.form_state().file_input_value_string(node.node_id))
        {
          return Some(value);
        }
        // Fall back to DOM-mirrored file input value semantics when no live override exists.
        return crate::dom::input_file_value_string(&node.node);
      }
      if let Some(value) = ctx
        .interaction_state
        .and_then(|state| state.form_state().value_for(node.node_id))
      {
        return Some(value.to_string());
      }
      // Fall back to the DOM layer's value computation so accessibility matches HTML sanitization
      // (e.g. color/date/time inputs) instead of exposing the raw `value=` attribute.
      ElementRef::new(&node.node)
        .accessibility_value()
        .or_else(|| {
          // Defensive fallback: `ElementRef::control_value()` should always return `Some` for
          // `<input>`, but preserve previous behaviour if that ever changes.
          Some(
            node
              .node
              .get_attribute_ref("value")
              .map(|v| v.to_string())
              .unwrap_or_default(),
          )
        })
    }
    "textarea" => {
      if let Some(value) = ctx
        .interaction_state
        .and_then(|state| state.form_state().value_for(node.node_id))
      {
        return Some(value.to_string());
      }
      Some(textarea_value_text(node, ctx))
    }
    "select" => select_value_text(node, ctx),
    _ => None,
  }
}

fn textarea_value_text(node: &StyledNode, ctx: &BuildContext) -> String {
  let mut value = String::new();
  for child in ctx.composed_children(node) {
    if ctx.is_hidden(child) {
      continue;
    }
    if let DomNodeType::Text { content } = &child.node.node_type {
      value.push_str(content);
    }
  }

  crate::dom::textarea_current_value_from_text_content(&node.node, value)
}

fn select_value_text(node: &StyledNode, ctx: &BuildContext) -> Option<String> {
  let multiple = node.node.get_attribute_ref("multiple").is_some();
  if multiple {
    first_selected_option_text(node, ctx)
  } else {
    selected_option_text(node, ctx)
  }
}

fn first_selected_option_text(node: &StyledNode, ctx: &BuildContext) -> Option<String> {
  let selected_override = ctx
    .interaction_state
    .and_then(|state| state.form_state().select_selected_options(node.node_id));

  let mut stack: Vec<&StyledNode> = vec![node];
  while let Some(current) = stack.pop() {
    ctx.deadline_step(RenderStage::BoxTree);
    if ctx.deadline_tripped() {
      return None;
    }

    if ctx.is_hidden(current) {
      continue;
    }

    let is_option = current
      .node
      .tag_name()
      .is_some_and(|tag| tag.eq_ignore_ascii_case("option"));

    let is_selected = if let Some(selected) = selected_override {
      selected.contains(&current.node_id)
    } else {
      current.node.get_attribute_ref("selected").is_some()
    };

    if is_option && is_selected {
      return Some(option_label_text(current, ctx));
    }

    for child in ctx.composed_children(current).into_iter().rev() {
      stack.push(child);
    }
  }

  None
}

fn selected_option_text(node: &StyledNode, ctx: &BuildContext) -> Option<String> {
  let selected_id = selected_option_node_id(node, ctx)?;
  let selected_node = ctx.node_by_id(selected_id)?;
  Some(option_label_text(selected_node, ctx))
}

fn option_label_text(node: &StyledNode, ctx: &BuildContext) -> String {
  if let Some(label) = node
    .node
    .get_attribute_ref("label")
    .filter(|label| !label.is_empty())
  {
    return normalize_whitespace(label);
  }

  option_text(node, ctx)
}

fn option_text(node: &StyledNode, ctx: &BuildContext) -> String {
  let mut visited = HashSet::new();
  ctx.subtree_text(node, &mut visited, TextAlternativeMode::Visible)
}

fn option_value(node: &StyledNode, ctx: &BuildContext) -> String {
  node
    .node
    .get_attribute_ref("value")
    .map(|v| v.to_string())
    .unwrap_or_else(|| option_text(node, ctx))
}

fn select_placeholder_label_option_node_id(
  select: &StyledNode,
  ctx: &BuildContext,
) -> Option<usize> {
  // https://html.spec.whatwg.org/#placeholder-label-option
  if select.node.get_attribute_ref("required").is_none() {
    return None;
  }
  if select.node.get_attribute_ref("multiple").is_some() {
    return None;
  }
  if crate::dom::select_effective_size(&select.node) != 1 {
    return None;
  }

  let mut stack: Vec<(&StyledNode, bool)> = Vec::new();
  for child in ctx.composed_children(select).into_iter().rev() {
    stack.push((child, true));
  }

  while let Some((node, parent_is_select)) = stack.pop() {
    if ctx.is_hidden(node) {
      continue;
    }

    if node
      .node
      .tag_name()
      .is_some_and(|tag| tag.eq_ignore_ascii_case("option"))
    {
      if parent_is_select && option_value(node, ctx).is_empty() {
        return Some(node.node_id);
      }
      return None;
    }

    for child in ctx.composed_children(node).into_iter().rev() {
      stack.push((child, false));
    }
  }

  None
}

fn selected_option_node_id(node: &StyledNode, ctx: &BuildContext) -> Option<usize> {
  if let Some(selected) = ctx
    .interaction_state
    .and_then(|state| state.form_state().select_selected_options(node.node_id))
  {
    let explicit = find_selected_option_node_id_override(node, false, selected, ctx);
    if explicit.is_some() {
      return explicit;
    }
  } else {
    let explicit = find_selected_option_node_id(node, false, ctx);
    if explicit.is_some() {
      return explicit;
    }
  }

  let multiple = node.node.get_attribute_ref("multiple").is_some();
  if multiple {
    return None;
  }

  first_enabled_option_node_id(node, false, ctx).or_else(|| first_option_node_id(node, ctx))
}

fn find_selected_option_node_id_override(
  node: &StyledNode,
  optgroup_disabled: bool,
  selected: &rustc_hash::FxHashSet<usize>,
  ctx: &BuildContext,
) -> Option<usize> {
  let mut out = None;
  let mut stack: Vec<(&StyledNode, bool)> = vec![(node, optgroup_disabled)];
  while let Some((current, optgroup_disabled)) = stack.pop() {
    ctx.deadline_step(RenderStage::BoxTree);
    if ctx.deadline_tripped() {
      return out;
    }

    let tag = current.node.tag_name().map(|t| t.to_ascii_lowercase());
    let is_option = tag.as_deref() == Some("option");

    let option_disabled = current.node.get_attribute_ref("disabled").is_some();
    let next_optgroup_disabled =
      optgroup_disabled || (tag.as_deref() == Some("optgroup") && option_disabled);

    if is_option && selected.contains(&current.node_id) && !ctx.is_hidden(current) {
      out = Some(current.node_id);
    }

    if ctx.is_hidden(current) {
      continue;
    }

    for child in ctx.composed_children(current).into_iter().rev() {
      stack.push((child, next_optgroup_disabled));
    }
  }
  out
}

fn find_selected_option_node_id(
  node: &StyledNode,
  optgroup_disabled: bool,
  ctx: &BuildContext,
) -> Option<usize> {
  let mut selected = None;
  let mut stack: Vec<(&StyledNode, bool)> = vec![(node, optgroup_disabled)];
  while let Some((current, optgroup_disabled)) = stack.pop() {
    ctx.deadline_step(RenderStage::BoxTree);
    if ctx.deadline_tripped() {
      return selected;
    }

    let tag = current.node.tag_name().map(|t| t.to_ascii_lowercase());
    let is_option = tag.as_deref() == Some("option");

    let option_disabled = current.node.get_attribute_ref("disabled").is_some();
    let next_optgroup_disabled =
      optgroup_disabled || (tag.as_deref() == Some("optgroup") && option_disabled);

    if is_option && current.node.get_attribute_ref("selected").is_some() && !ctx.is_hidden(current)
    {
      selected = Some(current.node_id);
    }

    if ctx.is_hidden(current) {
      continue;
    }

    for child in ctx.composed_children(current).into_iter().rev() {
      stack.push((child, next_optgroup_disabled));
    }
  }
  selected
}

fn first_enabled_option_node_id(
  node: &StyledNode,
  optgroup_disabled: bool,
  ctx: &BuildContext,
) -> Option<usize> {
  let mut stack: Vec<(&StyledNode, bool)> = vec![(node, optgroup_disabled)];
  while let Some((current, optgroup_disabled)) = stack.pop() {
    ctx.deadline_step(RenderStage::BoxTree);
    if ctx.deadline_tripped() {
      return None;
    }

    let tag = current.node.tag_name().map(|t| t.to_ascii_lowercase());
    let is_option = tag.as_deref() == Some("option");

    let option_disabled = current.node.get_attribute_ref("disabled").is_some();
    let next_optgroup_disabled =
      optgroup_disabled || (tag.as_deref() == Some("optgroup") && option_disabled);

    if is_option && !(option_disabled || optgroup_disabled) && !ctx.is_hidden(current) {
      return Some(current.node_id);
    }

    if ctx.is_hidden(current) {
      continue;
    }

    for child in ctx.composed_children(current).into_iter().rev() {
      stack.push((child, next_optgroup_disabled));
    }
  }

  None
}

fn first_option_node_id(node: &StyledNode, ctx: &BuildContext) -> Option<usize> {
  let mut stack: Vec<&StyledNode> = vec![node];
  while let Some(current) = stack.pop() {
    ctx.deadline_step(RenderStage::BoxTree);
    if ctx.deadline_tripped() {
      return None;
    }

    if current
      .node
      .tag_name()
      .is_some_and(|tag| tag.eq_ignore_ascii_case("option"))
      && !ctx.is_hidden(current)
    {
      return Some(current.node_id);
    }

    if ctx.is_hidden(current) {
      continue;
    }

    for child in ctx.composed_children(current).into_iter().rev() {
      stack.push(child);
    }
  }

  None
}

/// Use placeholder text as a fallback accessible name for text-entry controls.
///
/// HTML-AAM and major browsers expose the `placeholder` attribute as the name
/// when an `<input>`/`<textarea>` has no explicit label. We mirror that behavior
/// (after ARIA and `<label>` sources) so unlabeled text fields remain
/// discoverable to assistive tech.
fn placeholder_as_name(node: &StyledNode, ctx: &BuildContext) -> Option<String> {
  let tag = node.node.tag_name()?.to_ascii_lowercase();
  let is_textbox_like = match tag.as_str() {
    "textarea" => true,
    "input" => {
      let mut input_type = node
        .node
        .get_attribute_ref("type")
        .map(|t| t.to_ascii_lowercase())
        .unwrap_or_else(|| "text".to_string());
      if trim_ascii_whitespace(&input_type).is_empty() {
        input_type = "text".to_string();
      }
      matches!(
        input_type.as_str(),
        "text" | "search" | "email" | "tel" | "url" | "password" | "number"
      )
    }
    _ => false,
  };

  if !is_textbox_like {
    return None;
  }

  if control_value_text(node, ctx)
    .map(|v| !normalize_whitespace(&v).is_empty())
    .unwrap_or(false)
  {
    return None;
  }

  let placeholder = node.node.get_attribute_ref("placeholder")?;
  let norm = normalize_whitespace(placeholder);
  if norm.is_empty() {
    None
  } else {
    Some(norm)
  }
}

fn alt_applies(tag: Option<&str>, role: Option<&str>, node: &DomNode) -> bool {
  if role == Some("img") {
    return true;
  }

  match tag.map(|t| t.to_ascii_lowercase()) {
    Some(tag) if tag == "img" || tag == "area" => true,
    Some(tag) if tag == "input" => node
      .get_attribute_ref("type")
      .map(|t| t.eq_ignore_ascii_case("image"))
      .unwrap_or(false),
    _ => false,
  }
}

fn referenced_text_attr(
  ctx: &BuildContext,
  referrer_node_id: usize,
  attr_value: &str,
  visited: &mut HashSet<usize>,
  mode: TextAlternativeMode,
) -> String {
  let mut parts = Vec::new();
  let mut seen_tokens: HashSet<&str> = HashSet::new();
  for id in split_ascii_whitespace(attr_value) {
    if !seen_tokens.insert(id) {
      continue;
    }
    if let Some(target) = ctx.node_for_id_scoped(referrer_node_id, id) {
      if let Some(text) = ctx.text_alternative(target, visited, mode, Some(true)) {
        if !text.is_empty() {
          parts.push(text);
        }
      }
    }
  }

  normalize_whitespace(&parts.join(" "))
}

fn default_button_label(input_type: &str) -> Option<&'static str> {
  match input_type {
    "submit" => Some("Submit"),
    "reset" => Some("Reset"),
    "button" => Some("Button"),
    _ => None,
  }
}

fn compute_role_description(role: Option<&str>, node: &DomNode) -> Option<String> {
  if role.is_none() {
    return None;
  }

  let Some(description) = node.get_attribute_ref("aria-roledescription") else {
    return None;
  };

  let norm = normalize_whitespace(description);
  if norm.is_empty() {
    None
  } else {
    Some(norm)
  }
}

fn compute_description(
  node: &StyledNode,
  ctx: &BuildContext,
  invalid: bool,
  computed_name: Option<&str>,
) -> Option<String> {
  let mut parts: Vec<String> = Vec::new();
  let mut seen: HashSet<String> = HashSet::new();

  let describedby_attr = node.node.get_attribute_ref("aria-describedby");
  let has_describedby_attr = describedby_attr.is_some();
  if let Some(desc_attr) = describedby_attr {
    let mut visited = HashSet::new();
    visited.insert(node.node_id);
    let desc = referenced_text_attr(
      ctx,
      node.node_id,
      desc_attr,
      &mut visited,
      TextAlternativeMode::Referenced,
    );
    if !desc.is_empty() {
      if seen.insert(desc.clone()) {
        parts.push(desc);
      }
    }
  }

  if invalid {
    if let Some(err_attr) = node.node.get_attribute_ref("aria-errormessage") {
      if let Some(target) = resolve_idref_target(ctx, node, err_attr) {
        let mut visited = HashSet::new();
        visited.insert(node.node_id);
        let text = ctx
          .text_alternative(
            target,
            &mut visited,
            TextAlternativeMode::Referenced,
            Some(true),
          )
          .unwrap_or_default();
        let norm = normalize_whitespace(&text);
        if !norm.is_empty() && seen.insert(norm.clone()) {
          parts.push(norm);
        }
      }
    }
  }

  let has_aria_description_attr = node.node.get_attribute_ref("aria-description").is_some();
  if let Some(description) = node.node.get_attribute_ref("aria-description") {
    let norm = normalize_whitespace(description);
    if !norm.is_empty() {
      if seen.insert(norm.clone()) {
        parts.push(norm);
      }
    }
  }

  // The HTML `title` attribute is treated as a low-priority description fallback when ARIA
  // description sources are absent. Avoid duplicating the title when it was already used as the
  // accessible name.
  if !has_describedby_attr && !has_aria_description_attr {
    if let Some(title) = node.node.get_attribute_ref("title") {
      let norm_title = normalize_whitespace(title);
      if !norm_title.is_empty() {
        let name_matches_title = computed_name
          .map(|name| normalize_whitespace(name))
          .is_some_and(|name| name == norm_title);

        if !name_matches_title && seen.insert(norm_title.clone()) {
          parts.push(norm_title);
        }
      }
    }
  }

  if parts.is_empty() {
    None
  } else {
    Some(normalize_whitespace(&parts.join(" ")))
  }
}

fn resolve_idref_list(ctx: &BuildContext, origin: &StyledNode, attr_value: &str) -> Vec<String> {
  let mut out = Vec::new();
  let mut seen_tokens: HashSet<&str> = HashSet::new();
  for token in split_ascii_whitespace(attr_value) {
    if !seen_tokens.insert(token) {
      continue;
    }
    if let Some(target) = ctx.node_for_id_scoped(origin.node_id, token) {
      if let Some(id) = target
        .node
        .get_attribute_ref("id")
        .filter(|value| !value.is_empty())
      {
        out.push(id.to_string());
      }
    }
  }
  out
}

fn resolve_idref(ctx: &BuildContext, origin: &StyledNode, attr_value: &str) -> Option<String> {
  let trimmed = trim_ascii_whitespace(attr_value);
  if trimmed.is_empty() {
    return None;
  }

  // IDREF attributes are not lists; ignore whitespace-separated lists.
  let mut tokens = split_ascii_whitespace(trimmed);
  let token = tokens.next()?;
  if tokens.next().is_some() {
    return None;
  }

  let target = ctx.node_for_id_scoped(origin.node_id, token)?;
  target
    .node
    .get_attribute_ref("id")
    .filter(|value| !value.is_empty())
    .map(|value| value.to_string())
}

fn resolve_idref_target<'a, 'state>(
  ctx: &BuildContext<'a, 'state>,
  origin: &StyledNode,
  attr_value: &str,
) -> Option<&'a StyledNode> {
  let trimmed = trim_ascii_whitespace(attr_value);
  if trimmed.is_empty() {
    return None;
  }

  // `aria-errormessage` is an IDREF, not a list; ignore whitespace-separated lists.
  let mut tokens = split_ascii_whitespace(trimmed);
  let token = tokens.next()?;
  if tokens.next().is_some() {
    return None;
  }

  ctx.node_for_id_scoped(origin.node_id, token)
}

fn compute_relations(
  node: &StyledNode,
  ctx: &BuildContext,
  invalid: bool,
) -> Option<AccessibilityRelations> {
  let controls = node
    .node
    .get_attribute_ref("aria-controls")
    .map(|value| resolve_idref_list(ctx, node, value))
    .unwrap_or_default();

  let owns = node
    .node
    .get_attribute_ref("aria-owns")
    .map(|value| resolve_idref_list(ctx, node, value))
    .unwrap_or_default();

  let aria_labelledby_attr = node.node.get_attribute_ref("aria-labelledby");
  let mut labelled_by = aria_labelledby_attr
    .map(|value| resolve_idref_list(ctx, node, value))
    .unwrap_or_default();

  // Preserve native HTML `<label>` associations as a labelled-by relationship when ARIA does not
  // override naming. This enables downstream tooling (e.g. AccessKit) to keep an explicit link
  // between controls and their labels when the label element has an `id`.
  //
  // If `aria-labelledby` is present (even when it resolves to nothing), it overrides native label
  // associations per the ARIA name computation rules, so do not fall back in that case.
  if aria_labelledby_attr.is_none() {
    if let Some(labels) = ctx.labels.get(&node.node_id) {
      let mut seen_ids: HashSet<&str> = HashSet::new();
      for label_node_id in labels {
        let Some(label_node) = ctx.node_by_id(*label_node_id) else {
          continue;
        };
        let Some(id) = label_node.node.get_attribute_ref("id").filter(|s| !s.is_empty()) else {
          continue;
        };
        if !seen_ids.insert(id) {
          continue;
        }
        labelled_by.push(id.to_string());
      }
    }
  }

  let described_by = node
    .node
    .get_attribute_ref("aria-describedby")
    .map(|value| resolve_idref_list(ctx, node, value))
    .unwrap_or_default();

  let active_descendant = node
    .node
    .get_attribute_ref("aria-activedescendant")
    .and_then(|value| resolve_idref(ctx, node, value));

  let details = node
    .node
    .get_attribute_ref("aria-details")
    .and_then(|value| resolve_idref(ctx, node, value));

  let error_message = invalid
    .then(|| {
      node
        .node
        .get_attribute_ref("aria-errormessage")
        .and_then(|value| resolve_idref(ctx, node, value))
    })
    .flatten();

  if controls.is_empty()
    && owns.is_empty()
    && labelled_by.is_empty()
    && described_by.is_empty()
    && active_descendant.is_none()
    && details.is_none()
    && error_message.is_none()
  {
    None
  } else {
    Some(AccessibilityRelations {
      controls,
      owns,
      labelled_by,
      described_by,
      active_descendant,
      details,
      error_message,
    })
  }
}

fn compute_level(node: &DomNode, role: Option<&str>) -> Option<u32> {
  if !matches!(role, Some("heading")) {
    return None;
  }

  if let Some(attr) = node.get_attribute_ref("aria-level") {
    if let Ok(level) = trim_ascii_whitespace(attr).parse::<u32>() {
      if level > 0 {
        return Some(level);
      }
    }
  }

  let tag = node.tag_name().map(|t| t.to_ascii_lowercase());
  match tag.as_deref() {
    Some("h1") => Some(1),
    Some("h2") => Some(2),
    Some("h3") => Some(3),
    Some("h4") => Some(4),
    Some("h5") => Some(5),
    Some("h6") => Some(6),
    _ => Some(2),
  }
}

fn compute_value(
  node: &StyledNode,
  role: Option<&str>,
  element_ref: &ElementRef,
  ctx: &BuildContext,
) -> Option<String> {
  match role {
    Some("textbox") | Some("searchbox") | Some("combobox") | Some("listbox") => {
      control_value_text(node, ctx)
        .map(|v| normalize_whitespace(&v))
        .filter(|v| !v.is_empty())
    }
    Some("spinbutton") | Some("slider") => {
      if let Some(value) = aria_value_attr(&node.node) {
        return Some(value);
      }

      // Fall back to the resolved control value (e.g., range inputs expose their sanitized slider
      // position even when no explicit value is authored).
      element_ref
        .accessibility_value()
        .filter(|v| !v.is_empty())
        .map(|v| normalize_whitespace(&v))
    }
    Some("progressbar") => {
      if let Some(value) = aria_value_attr(&node.node) {
        return Some(value);
      }

      progress_value(&node.node)
    }
    Some("meter") => {
      if let Some(value) = aria_value_attr(&node.node) {
        return Some(value);
      }

      meter_value(&node.node)
    }
    Some("option") => {
      let text = option_label_text(node, ctx);
      if text.is_empty() {
        None
      } else {
        Some(text)
      }
    }
    _ => None,
  }
}

fn aria_value_attr(node: &DomNode) -> Option<String> {
  if let Some(text) = node.get_attribute_ref("aria-valuetext") {
    let norm = normalize_whitespace(text);
    if !norm.is_empty() {
      return Some(norm);
    }
  }

  if let Some(text) = node.get_attribute_ref("aria-valuenow") {
    let norm = normalize_whitespace(text);
    if !norm.is_empty() {
      return Some(norm);
    }
  }

  None
}

fn format_number(mut value: f64) -> String {
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

fn progress_value(node: &DomNode) -> Option<String> {
  let raw_value = node.get_attribute_ref("value")?;
  let parsed = trim_ascii_whitespace(raw_value)
    .parse::<f64>()
    .ok()
    .filter(|v| v.is_finite())?;
  Some(format_number(parsed))
}

fn meter_value(node: &DomNode) -> Option<String> {
  let raw_value = node.get_attribute_ref("value")?;
  let parsed = trim_ascii_whitespace(raw_value)
    .parse::<f64>()
    .ok()
    .filter(|v| v.is_finite())?;
  Some(format_number(parsed))
}

fn supports_disabled(node: &DomNode) -> bool {
  if !is_html_element(node) {
    return false;
  }

  node.tag_name().is_some_and(|tag| {
    tag.eq_ignore_ascii_case("button")
      || tag.eq_ignore_ascii_case("input")
      || tag.eq_ignore_ascii_case("select")
      || tag.eq_ignore_ascii_case("textarea")
      || tag.eq_ignore_ascii_case("option")
      || tag.eq_ignore_ascii_case("optgroup")
      || tag.eq_ignore_ascii_case("fieldset")
  })
}

fn compute_native_disabled(node: &StyledNode, styled_ancestors: &[&StyledNode]) -> bool {
  if !supports_disabled(&node.node) {
    return false;
  }

  if node.node.get_attribute_ref("disabled").is_some() {
    return true;
  }

  // Fieldset disabled state propagates to descendants except those inside the first legend.
  for (i, ancestor) in styled_ancestors.iter().enumerate().rev() {
    if !ancestor
      .node
      .tag_name()
      .is_some_and(|tag| tag.eq_ignore_ascii_case("fieldset"))
    {
      continue;
    }
    if ancestor.node.get_attribute_ref("disabled").is_none() {
      continue;
    }

    let first_legend = ancestor.children.iter().find(|child| {
      child
        .node
        .tag_name()
        .is_some_and(|tag| tag.eq_ignore_ascii_case("legend"))
    });

    if let Some(legend) = first_legend {
      let in_legend = styled_ancestors
        .get(i + 1..)
        .into_iter()
        .flatten()
        .any(|n| ptr::eq(*n, legend));
      if in_legend {
        continue;
      }
    }

    return true;
  }

  let Some(tag) = node.node.tag_name().map(|t| t.to_ascii_lowercase()) else {
    return false;
  };

  if tag == "option" || tag == "optgroup" {
    for ancestor in styled_ancestors.iter().rev() {
      if ancestor.node.tag_name().is_some_and(|tag| {
        tag.eq_ignore_ascii_case("select") || tag.eq_ignore_ascii_case("optgroup")
      }) && ancestor.node.get_attribute_ref("disabled").is_some()
      {
        return true;
      }
    }
  }

  false
}

fn select_has_non_disabled_selected_option(select: &StyledNode, ctx: &BuildContext) -> bool {
  let multiple = select.node.get_attribute_ref("multiple").is_some();
  if !multiple {
    let Some(selected_id) = selected_option_node_id(select, ctx) else {
      return false;
    };

    let mut stack: Vec<(&StyledNode, bool)> = Vec::new();
    stack.push((select, false));

    while let Some((node, optgroup_disabled)) = stack.pop() {
      if ctx.is_hidden(node) {
        continue;
      }

      let is_option = node
        .node
        .tag_name()
        .is_some_and(|tag| tag.eq_ignore_ascii_case("option"));
      let is_optgroup = node
        .node
        .tag_name()
        .is_some_and(|tag| tag.eq_ignore_ascii_case("optgroup"));

      let disabled_attr = node.node.get_attribute_ref("disabled").is_some();
      let next_optgroup_disabled = optgroup_disabled || (is_optgroup && disabled_attr);

      if is_option && node.node_id == selected_id {
        return !(disabled_attr || optgroup_disabled);
      }

      for child in ctx.composed_children(node).into_iter().rev() {
        stack.push((child, next_optgroup_disabled));
      }
    }

    return false;
  }

  let mut stack: Vec<(&StyledNode, bool)> = Vec::new();
  stack.push((select, false));

  while let Some((node, optgroup_disabled)) = stack.pop() {
    if ctx.is_hidden(node) {
      continue;
    }

    let is_option = node
      .node
      .tag_name()
      .is_some_and(|tag| tag.eq_ignore_ascii_case("option"));
    let is_optgroup = node
      .node
      .tag_name()
      .is_some_and(|tag| tag.eq_ignore_ascii_case("optgroup"));

    let disabled_attr = node.node.get_attribute_ref("disabled").is_some();
    let next_optgroup_disabled = optgroup_disabled || (is_optgroup && disabled_attr);

    if is_option && node.node.get_attribute_ref("selected").is_some() {
      let option_disabled = disabled_attr || optgroup_disabled;
      if !option_disabled {
        return true;
      }
    }

    for child in ctx.composed_children(node).into_iter().rev() {
      stack.push((child, next_optgroup_disabled));
    }
  }

  false
}

fn compute_invalid(
  node: &StyledNode,
  element_ref: &ElementRef,
  styled_ancestors: &[&StyledNode],
  ctx: &BuildContext,
) -> bool {
  if let Some(value) = parse_aria_invalid(&node.node) {
    // ARIA should not negate native HTML semantics: allow authors to force the invalid state on,
    // but ignore explicit `false` so native constraint validation still surfaces.
    if value {
      return true;
    }
  }
  let native_disabled = compute_native_disabled(node, styled_ancestors);
  if native_disabled {
    return false;
  }

  if let Some(dom) = ctx.validation_dom.as_ref() {
    return dom
      .with_element_ref(node.node_id, |ref_for_validation| {
        forms_validation::validity_state_with_disabled(&ref_for_validation, native_disabled)
          .is_some_and(|state| !state.valid)
      })
      .unwrap_or(false);
  }

  forms_validation::validity_state_with_disabled(element_ref, native_disabled)
    .is_some_and(|state| !state.valid)
}

fn compute_checked(
  node: &StyledNode,
  role: Option<&str>,
  element_ref: &ElementRef,
  ctx: &BuildContext<'_, '_>,
) -> Option<CheckState> {
  let is_native_checkbox_or_radio = is_html_element(&node.node)
    && node
      .node
      .tag_name()
      .is_some_and(|tag| tag.eq_ignore_ascii_case("input"))
    && node
      .node
      .get_attribute_ref("type")
      .is_some_and(|t| t.eq_ignore_ascii_case("checkbox") || t.eq_ignore_ascii_case("radio"));
  let is_native_radio = is_native_checkbox_or_radio
    && node
      .node
      .get_attribute_ref("type")
      .is_some_and(|t| t.eq_ignore_ascii_case("radio"));

  if is_native_checkbox_or_radio
    && matches!(
      role,
      Some("checkbox")
        | Some("radio")
        | Some("switch")
        | Some("menuitemcheckbox")
        | Some("menuitemradio")
      )
  {
    if !is_native_radio && element_ref.accessibility_indeterminate() {
      return Some(CheckState::Mixed);
    }
    let checked = ctx
      .interaction_state
      .and_then(|state| state.form_state().checked_for(node.node_id))
      .unwrap_or_else(|| element_ref.accessibility_checked());
    if checked {
      return Some(CheckState::True);
    }
    return Some(CheckState::False);
  }

  if let Some(state) = parse_check_state(&node.node, "aria-checked") {
    return Some(state);
  }

  if matches!(
    role,
    Some("checkbox")
      | Some("radio")
      | Some("switch")
      | Some("menuitemcheckbox")
      | Some("menuitemradio")
  ) {
    if !is_native_radio && element_ref.accessibility_indeterminate() {
      return Some(CheckState::Mixed);
    }
    if element_ref.accessibility_checked() {
      return Some(CheckState::True);
    }
    return Some(CheckState::False);
  }

  None
}

fn compute_selected(
  node: &StyledNode,
  role: Option<&str>,
  element_ref: &ElementRef,
  styled_ancestors: &[&StyledNode],
  ctx: &BuildContext,
) -> Option<bool> {
  if role == Some("option") {
    if let Some(select) = styled_ancestors.iter().rev().find(|ancestor| {
      ancestor
        .node
        .tag_name()
        .is_some_and(|tag| tag.eq_ignore_ascii_case("select"))
    }) {
      if select.node.get_attribute_ref("multiple").is_some() {
        if let Some(selected) = ctx
          .interaction_state
          .and_then(|state| state.form_state().select_selected_options(select.node_id))
        {
          return Some(selected.contains(&node.node_id));
        }

        return Some(node.node.get_attribute_ref("selected").is_some());
      }

      let selected_id = selected_option_node_id(select, ctx);
      return Some(selected_id.is_some_and(|id| id == node.node_id));
    }

    if let Some(selected) = parse_bool_attr(&node.node, "aria-selected") {
      return Some(selected);
    }
    return Some(element_ref.accessibility_selected());
  }

  if let Some(selected) = parse_bool_attr(&node.node, "aria-selected") {
    return Some(selected);
  }

  if matches!(role, Some("tab") | Some("treeitem") | Some("gridcell")) {
    return Some(false);
  }

  None
}

fn compute_pressed(
  node: &StyledNode,
  role: Option<&str>,
  ctx: &BuildContext<'_, '_>,
) -> Option<PressedState> {
  if let Some(state) = parse_pressed_state(&node.node, "aria-pressed") {
    return Some(state);
  }

  if role == Some("button")
    && ctx
      .interaction_state
      .is_some_and(|state| state.is_active(node.node_id))
  {
    return Some(PressedState::True);
  }

  None
}

fn compute_multiline(node: &StyledNode, role: Option<&str>) -> Option<bool> {
  if role != Some("textbox") {
    return None;
  }

  let tag = node.node.tag_name().map(|t| t.to_ascii_lowercase());
  match tag.as_deref() {
    Some("textarea") => Some(true),
    Some("input") => Some(false),
    // Allow authors to specify multiline state for custom textboxes, but don't let ARIA negate
    // native textbox semantics for `<input>`/`<textarea>`.
    _ => parse_aria_multiline(&node.node),
  }
}

fn compute_modal(node: &DomNode) -> Option<bool> {
  if let Some(value) = parse_bool_attr(node, "aria-modal") {
    return Some(value);
  }

  let is_dialog = is_html_element(node)
    && match node.tag_name() {
      Some(tag) => tag.eq_ignore_ascii_case("dialog"),
      None => false,
    };

  if is_dialog && attr_truthy(node, "data-fastr-modal") {
    return Some(true);
  }

  None
}

fn compute_readonly(node: &DomNode, _role: Option<&str>, element_ref: &ElementRef) -> bool {
  let native_readonly = element_ref.accessibility_readonly();
  let aria_readonly = parse_bool_attr(node, "aria-readonly");
  native_readonly || aria_readonly == Some(true)
}

fn compute_expanded(
  node: &StyledNode,
  role: Option<&str>,
  ancestors: &[&DomNode],
  interaction_state: Option<&InteractionState>,
) -> Option<bool> {
  if let Some(expanded) = parse_expanded(&node.node) {
    return Some(expanded);
  }

  let tag = node
    .node
    .tag_name()
    .map(|t| t.to_ascii_lowercase())
    .unwrap_or_default();

  if tag == "details" && is_html_element(&node.node) {
    return Some(node.node.get_attribute_ref("open").is_some());
  }

  let is_summary = is_html_element(&node.node)
    && node
      .node
      .tag_name()
      .map(|t| t.eq_ignore_ascii_case("summary"))
      .unwrap_or(false);

  if role == Some("button") && is_summary {
    if let Some(parent) = ancestors.last() {
      if is_html_element(parent)
        && parent
          .tag_name()
          .map(|t| t.eq_ignore_ascii_case("details"))
          .unwrap_or(false)
      {
        if let Some(expanded) = parse_expanded(parent) {
          return Some(expanded);
        }
        return Some(parent.get_attribute_ref("open").is_some());
      }
    }
  }

  // Native `<select>` dropdowns (single select + size==1) are represented as `role=combobox`.
  // The popup UI is owned by the front-end, so we consult `InteractionState` to determine whether
  // the combobox is currently expanded.
  if role == Some("combobox") && tag == "select" && is_html_element(&node.node) {
    if let Some(state) = interaction_state {
      return Some(state.open_select_dropdown == Some(node.node_id));
    }
  }

  if role == Some("combobox") && node.node.get_attribute_ref("data-fastr-open").is_some() {
    return Some(attr_truthy(&node.node, "data-fastr-open"));
  }

  None
}

fn compute_focusable(node: &DomNode, role: Option<&str>, disabled: bool) -> bool {
  if disabled {
    return false;
  }

  if let Some(tabindex) = node.get_attribute_ref("tabindex") {
    let trimmed = trim_ascii_whitespace(tabindex);
    if !trimmed.is_empty() && trimmed.parse::<i32>().is_ok() {
      return true;
    }
  }

  let tag = match node.tag_name() {
    Some(t) => t.to_ascii_lowercase(),
    None => return false,
  };

  if tag == "a" && node.get_attribute_ref("href").is_some() {
    return true;
  }

  if matches!(tag.as_str(), "button" | "select" | "textarea") {
    return true;
  }

  if tag == "input" {
    let input_type = node
      .get_attribute_ref("type")
      .map(|t| t.to_ascii_lowercase())
      .unwrap_or_else(|| "text".to_string());
    return input_type != "hidden";
  }

  if tag == "option" {
    return true;
  }

  if let Some(r) = role {
    if matches!(r, "button" | "link" | "checkbox" | "radio" | "switch") {
      return true;
    }
  }

  node
    .get_attribute_ref("contenteditable")
    .map(|v| v.is_empty() || v.eq_ignore_ascii_case("true"))
    .unwrap_or(false)
}

fn parse_invalid(
  node: &StyledNode,
  element_ref: &ElementRef,
  styled_ancestors: &[&StyledNode],
  ctx: &BuildContext,
) -> bool {
  compute_invalid(node, element_ref, styled_ancestors, ctx)
}

fn parse_expanded(node: &DomNode) -> Option<bool> {
  let value = node.get_attribute_ref("aria-expanded")?;
  let token = trim_ascii_whitespace(value).to_ascii_lowercase();
  parse_bool_token(&token)
}

fn parse_has_popup(node: &DomNode) -> Option<String> {
  let value = node.get_attribute_ref("aria-haspopup")?;
  let trimmed = trim_ascii_whitespace(value);
  // `aria-haspopup` is an enumerated ARIA token attribute. Only allow known tokens; ignore invalid
  // values so they don't leak into serialized accessibility output.
  //
  // In HTML, a minimized attribute like `<button aria-haspopup>` parses as an empty string. The
  // ARIA processing rules treat invalid tokens (including the empty string) as if the attribute
  // was absent.
  if trimmed.is_empty() {
    return None;
  }

  let token = trimmed.to_ascii_lowercase();
  if matches!(token.as_str(), "false" | "0") {
    return None;
  }

  // Accept `1` as a legacy synonym for `true`, matching our other boolean-ish ARIA parsing.
  if token == "1" {
    return Some("true".to_string());
  }

  if matches!(
    token.as_str(),
    "true" | "menu" | "listbox" | "tree" | "grid" | "dialog"
  ) {
    Some(token)
  } else {
    None
  }
}

fn parse_aria_live(node: &DomNode) -> Option<String> {
  let value = node.get_attribute_ref("aria-live")?;
  let token = trim_ascii_whitespace(value).to_ascii_lowercase();
  if matches!(token.as_str(), "off" | "polite" | "assertive") {
    Some(token)
  } else {
    None
  }
}

fn parse_aria_relevant(node: &DomNode) -> Option<String> {
  let value = node.get_attribute_ref("aria-relevant")?;
  let mut tokens: Vec<String> = Vec::new();
  let mut all = false;

  for token in split_ascii_whitespace(value) {
    let lower = token.to_ascii_lowercase();
    match lower.as_str() {
      "all" => all = true,
      "additions" | "removals" | "text" => {
        if !tokens.iter().any(|t| t == &lower) {
          tokens.push(lower);
        }
      }
      _ => {}
    }
  }

  if all {
    return Some("all".to_string());
  }
  if tokens.is_empty() {
    None
  } else {
    Some(tokens.join(" "))
  }
}

fn parse_aria_invalid(node: &DomNode) -> Option<bool> {
  let value = node.get_attribute_ref("aria-invalid")?;
  let token = trim_ascii_whitespace(value).to_ascii_lowercase();

  if matches!(token.as_str(), "grammar" | "spelling") {
    return Some(true);
  }

  parse_bool_token(&token)
}

fn parse_bool_attr(node: &DomNode, name: &str) -> Option<bool> {
  let value = node.get_attribute_ref(name)?;
  let token = trim_ascii_whitespace(value).to_ascii_lowercase();
  parse_bool_token(&token)
}

fn parse_bool_token(token: &str) -> Option<bool> {
  match token {
    "true" | "1" => Some(true),
    "false" | "0" => Some(false),
    // ARIA boolean states are enumerated tokens; ignore invalid values (including empty strings).
    _ => None,
  }
}

fn attr_truthy(node: &DomNode, name: &str) -> bool {
  parse_bool_attr(node, name).unwrap_or(false)
}

fn parse_aria_multiline(node: &DomNode) -> Option<bool> {
  let value = node.get_attribute_ref("aria-multiline")?;
  let token = trim_ascii_whitespace(value).to_ascii_lowercase();
  parse_bool_token(&token)
}

fn parse_check_state(node: &DomNode, name: &str) -> Option<CheckState> {
  let value = node.get_attribute_ref(name)?;
  let token = trim_ascii_whitespace(value).to_ascii_lowercase();
  match token.as_str() {
    "true" | "1" => Some(CheckState::True),
    "false" | "0" => Some(CheckState::False),
    "mixed" => Some(CheckState::Mixed),
    _ => None,
  }
}

fn parse_pressed_state(node: &DomNode, name: &str) -> Option<PressedState> {
  let value = node.get_attribute_ref(name)?;
  let token = trim_ascii_whitespace(value).to_ascii_lowercase();
  match token.as_str() {
    "true" | "1" => Some(PressedState::True),
    "false" | "0" => Some(PressedState::False),
    "mixed" => Some(PressedState::Mixed),
    _ => None,
  }
}

fn parse_aria_current(node: &DomNode) -> Option<AriaCurrent> {
  let value = node.get_attribute_ref("aria-current")?;
  let token = trim_ascii_whitespace(value).to_ascii_lowercase();

  if token.is_empty() || token == "false" {
    return None;
  }

  match token.as_str() {
    "page" => Some(AriaCurrent::Page),
    "step" => Some(AriaCurrent::Step),
    "location" => Some(AriaCurrent::Location),
    "date" => Some(AriaCurrent::Date),
    "time" => Some(AriaCurrent::Time),
    "true" => Some(AriaCurrent::True),
    _ => None,
  }
}

fn is_false(value: &bool) -> bool {
  !*value
}

fn is_labelable(node: &DomNode) -> bool {
  let Some(tag) = node.tag_name() else {
    return false;
  };

  match tag.to_ascii_lowercase().as_str() {
    "button" | "select" | "textarea" | "output" | "progress" | "meter" => true,
    "input" => {
      let input_type = node
        .get_attribute_ref("type")
        .map(|t| t.to_ascii_lowercase())
        .unwrap_or_else(|| "text".to_string());
      input_type != "hidden"
    }
    _ => false,
  }
}

// -----------------------------------------------------------------------------
// AccessKit export (best-effort; optional `browser_ui` feature)
// -----------------------------------------------------------------------------
//
// FastRender's renderer/UI uses AccessKit (via `accesskit_winit`) to expose accessibility metadata
// to native assistive technologies. When available, propagate HTML language (`lang`) into AccessKit
// nodes so screen readers can pronounce text correctly.
//
// This is intentionally best-effort:
// - If an attribute is missing/invalid, the corresponding AccessKit property is left unset.
#[cfg(feature = "browser_ui")]
pub fn build_accesskit_tree_update(root: &StyledNode) -> ::accesskit::TreeUpdate {
  use std::num::NonZeroU128;

  /// Index enabling efficient ancestor traversal by `node_id`.
  struct DomIndex<'a> {
    node_by_id: Vec<Option<&'a StyledNode>>,
    parent_by_id: Vec<usize>,
  }

  impl<'a> DomIndex<'a> {
    fn build(root: &'a StyledNode) -> Self {
      let mut node_by_id: Vec<Option<&'a StyledNode>> = Vec::new();
      let mut parent_by_id: Vec<usize> = Vec::new();
      // Keep index 0 unused so `node_id` can be used directly (ids are 1-based).
      node_by_id.push(None);
      parent_by_id.push(0);

      let mut stack: Vec<(&'a StyledNode, usize)> = vec![(root, 0)];
      while let Some((node, parent)) = stack.pop() {
        let id = node.node_id;
        if node_by_id.len() <= id {
          node_by_id.resize(id + 1, None);
          parent_by_id.resize(id + 1, 0);
        }
        node_by_id[id] = Some(node);
        parent_by_id[id] = parent;

        for child in node.children.iter().rev() {
          stack.push((child, id));
        }
      }

      Self {
        node_by_id,
        parent_by_id,
      }
    }

    fn node(&self, id: usize) -> Option<&'a StyledNode> {
      self.node_by_id.get(id).and_then(|n| *n)
    }

    fn parent(&self, id: usize) -> usize {
      self.parent_by_id.get(id).copied().unwrap_or(0)
    }
  }

  fn accesskit_node_id(node_id: usize) -> ::accesskit::NodeId {
    // Best-effort: `node_id` is 1-based in normal operation. If it is ever 0, fall back to 1.
    let raw = if node_id == 0 { 1 } else { node_id as u128 };
    let raw = NonZeroU128::new(raw).unwrap_or_else(|| NonZeroU128::new(1).unwrap()); // fastrender-allow-unwrap
    ::accesskit::NodeId(raw)
  }

  fn resolve_effective_lang(node_id: usize, index: &DomIndex<'_>) -> Option<String> {
    let mut current = node_id;
    while current != 0 {
      let node = index.node(current)?;
      if let Some(lang) = node.node.get_attribute_ref("lang") {
        let normalized = crate::style::normalize_language_tag(lang);
        if !normalized.is_empty() {
          return Some(normalized);
        }
      }

      let parent = index.parent(current);
      if parent == current {
        break;
      }
      current = parent;
    }
    None
  }
  fn direct_text_name(node: &StyledNode) -> Option<String> {
    // Use *direct* text-node children to avoid assigning the same name to high-level containers
    // (html/body/document) while still surfacing simple text-only elements in tests and debug
    // tooling.
    let mut raw = String::new();
    for child in &node.children {
      if let DomNodeType::Text { content } = &child.node.node_type {
        raw.push_str(content);
      }
    }
    let normalized = normalize_whitespace(&raw);
    (!normalized.is_empty()).then_some(normalized)
  }

  fn should_include(node: &StyledNode) -> bool {
    if is_node_hidden(node) {
      return false;
    }
    matches!(
      node.node.node_type,
      DomNodeType::Document { .. }
        | DomNodeType::Element { .. }
        | DomNodeType::Slot { .. }
        | DomNodeType::ShadowRoot { .. }
    )
  }

  fn role_for_node(node: &StyledNode) -> ::accesskit::Role {
    match node.node.node_type {
      DomNodeType::Document { .. } => ::accesskit::Role::Document,
      _ => ::accesskit::Role::GenericContainer,
    }
  }

  // Precompute an index for ancestor lookups.
  let index = DomIndex::build(root);

  struct Frame<'a> {
    node: &'a StyledNode,
    next_child: usize,
    child_ids: Vec<::accesskit::NodeId>,
  }

  let mut classes = ::accesskit::NodeClassSet::new();
  let mut nodes: Vec<(::accesskit::NodeId, ::accesskit::Node)> = Vec::new();
  let mut stack: Vec<Frame<'_>> = Vec::new();

  if should_include(root) {
    stack.push(Frame {
      node: root,
      next_child: 0,
      child_ids: Vec::new(),
    });
  }

  while let Some(frame) = stack.last_mut() {
    if frame.next_child < frame.node.children.len() {
      let child = &frame.node.children[frame.next_child];
      frame.next_child += 1;

      if should_include(child) {
        stack.push(Frame {
          node: child,
          next_child: 0,
          child_ids: Vec::new(),
        });
      }
      continue;
    }

    let finished = stack.pop().expect("frame must exist"); // fastrender-allow-unwrap
    let node = finished.node;
    let id = accesskit_node_id(node.node_id);

    let mut builder = ::accesskit::NodeBuilder::new(role_for_node(node));
    if let Some(name) = direct_text_name(node) {
      builder.set_name(name);
    }
    if !finished.child_ids.is_empty() {
      builder.set_children(finished.child_ids.clone());
    }

    if let Some(lang) = resolve_effective_lang(node.node_id, &index) {
      builder.set_language(lang);
    }

    let built = builder.build(&mut classes);
    nodes.push((id, built));

    if let Some(parent) = stack.last_mut() {
      parent.child_ids.push(id);
    }
  }

  let root_id = accesskit_node_id(root.node_id);
  ::accesskit::TreeUpdate {
    nodes,
    tree: Some(::accesskit::Tree::new(root_id)),
    focus: None,
  }
}

#[cfg(all(test, feature = "browser_ui"))]
mod accesskit_lang_tests {
  use super::*;

  fn find_node_by_name<'a>(
    update: &'a ::accesskit::TreeUpdate,
    name: &str,
  ) -> Option<&'a ::accesskit::Node> {
    update
      .nodes
      .iter()
      .find_map(|(_id, node)| node.name().is_some_and(|n| n.trim() == name).then_some(node))
  }

  #[test]
  fn accesskit_nodes_include_lang_attribute() {
    let fixture = crate::testing::styled_tree(r#"<div lang="fr">Bonjour</div>"#, "", (800.0, 600.0));
    let update = build_accesskit_tree_update(&fixture.styled);
    let node = find_node_by_name(&update, "Bonjour").expect("expected node with name 'Bonjour'");
    assert_eq!(node.language(), Some("fr"));
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::style::ComputedStyle;
  use crate::dom::SVG_NAMESPACE;
  use selectors::context::QuirksMode;
  use std::sync::Arc;

  #[test]
  fn accessibility_normalize_whitespace_does_not_trim_non_ascii_whitespace() {
    let nbsp = "\u{00A0}";
    assert_eq!(
      normalize_whitespace(&format!("{nbsp}hello{nbsp}")),
      format!("{nbsp}hello{nbsp}")
    );
    assert_eq!(normalize_whitespace("  hello \n"), "hello");
  }

  #[test]
  fn accessibility_split_ascii_whitespace_does_not_split_non_ascii_whitespace() {
    let nbsp = "\u{00A0}";
    let input = format!("a{nbsp}b");
    let tokens: Vec<&str> = split_ascii_whitespace(&input).collect();
    assert_eq!(tokens, vec![input.as_str()]);
  }

  #[test]
  fn accessibility_tree_build_is_stack_safe_for_deep_trees() {
    fn make_styled_node(
      node_id: usize,
      node_type: DomNodeType,
      styles: &Arc<ComputedStyle>,
    ) -> StyledNode {
      StyledNode {
        node_id,
        subtree_size: 1,
        node: DomNode {
          node_type,
          children: Vec::new(),
        },
        styles: Arc::clone(styles),
        starting_styles: Default::default(),
        before_styles: None,
        after_styles: None,
        marker_styles: None,
        placeholder_styles: None,
        file_selector_button_styles: None,
        footnote_call_styles: None,
        footnote_marker_styles: None,
        first_line_styles: None,
        first_letter_styles: None,
        slider_thumb_styles: None,
        slider_track_styles: None,
        progress_bar_styles: None,
        progress_value_styles: None,
        meter_bar_styles: None,
        meter_optimum_value_styles: None,
        meter_suboptimum_value_styles: None,
        meter_even_less_good_value_styles: None,
        assigned_slot: None,
        slotted_node_ids: Vec::new(),
        children: Vec::new(),
      }
    }

    // A deep chain should not overflow the stack during accessibility tree construction.
    let depth = 20_000usize;
    let styles = Arc::new(ComputedStyle::default());
    let mut root = make_styled_node(
      1,
      DomNodeType::Document {
        quirks_mode: QuirksMode::NoQuirks,
        scripting_enabled: true,
        is_html_document: true,
      },
      &styles,
    );

    let mut current: *mut StyledNode = &mut root;
    for node_id in 2..=(depth + 1) {
      let child = make_styled_node(
        node_id,
        DomNodeType::Element {
          tag_name: String::new(),
          namespace: String::new(),
          attributes: vec![("role".to_string(), "presentation".to_string())],
        },
        &styles,
      );

      // Safety: each node gets exactly one child, so we never push to the same `children` vec twice
      // after taking the pointer to its last element.
      unsafe {
        let node = &mut *current;
        node.children.push(child);
        current = node.children.last_mut().expect("child was just pushed") as *mut StyledNode;
      }
    }

    assert!(build_accessibility_tree(&root, None).is_ok());
  }

  #[test]
  fn aria_presentation_is_honored_for_controls_disabled_by_fieldset() {
    let fieldset = DomNode {
      node_type: DomNodeType::Element {
        tag_name: "fieldset".to_string(),
        namespace: String::new(),
        attributes: vec![("disabled".to_string(), String::new())],
      },
      children: vec![DomNode {
        node_type: DomNodeType::Element {
          tag_name: "input".to_string(),
          namespace: String::new(),
          attributes: vec![
            ("role".to_string(), "presentation".to_string()),
            ("tabindex".to_string(), "0".to_string()),
          ],
        },
        children: Vec::new(),
      }],
    };

    let input = &fieldset.children[0];
    let ancestors: [&DomNode; 1] = [&fieldset];
    assert!(
      matches!(
        parse_aria_role_attr(input, &ancestors),
        Some(ParsedRole::Presentational)
      ),
      "controls disabled by <fieldset disabled> should not be treated as focusable when deciding whether to honor role=presentation"
    );
  }

  #[test]
  fn aria_presentation_is_not_honored_for_controls_in_first_legend_of_disabled_fieldset() {
    let fieldset = DomNode {
      node_type: DomNodeType::Element {
        tag_name: "fieldset".to_string(),
        namespace: String::new(),
        attributes: vec![("disabled".to_string(), String::new())],
      },
      children: vec![DomNode {
        node_type: DomNodeType::Element {
          tag_name: "legend".to_string(),
          namespace: String::new(),
          attributes: Vec::new(),
        },
        children: vec![DomNode {
          node_type: DomNodeType::Element {
            tag_name: "input".to_string(),
            namespace: String::new(),
            attributes: vec![("role".to_string(), "presentation".to_string())],
          },
          children: Vec::new(),
        }],
      }],
    };

    let legend = &fieldset.children[0];
    let input = &legend.children[0];
    let ancestors: [&DomNode; 2] = [&fieldset, legend];
    assert!(
      parse_aria_role_attr(input, &ancestors).is_none(),
      "fieldset first-legend exception should keep controls focusable; role=presentation must be ignored in that case"
    );
  }

  #[test]
  fn svg_dialog_does_not_imply_modal_state_from_data_fastr_modal() {
    let svg_dialog = DomNode {
      node_type: DomNodeType::Element {
        tag_name: "dialog".to_string(),
        namespace: SVG_NAMESPACE.to_string(),
        attributes: vec![("data-fastr-modal".to_string(), "true".to_string())],
      },
      children: Vec::new(),
    };

    assert_eq!(
      compute_modal(&svg_dialog),
      None,
      "non-HTML namespace elements must not inherit HTML <dialog> modal semantics"
    );
  }

  #[test]
  fn svg_details_does_not_imply_expanded_state_from_open_attribute() {
    let styles = Arc::new(ComputedStyle::default());
    let svg_details = StyledNode {
      node_id: 1,
      subtree_size: 1,
      node: DomNode {
        node_type: DomNodeType::Element {
          tag_name: "details".to_string(),
          namespace: SVG_NAMESPACE.to_string(),
          attributes: vec![("open".to_string(), String::new())],
        },
        children: Vec::new(),
      },
      styles,
      starting_styles: Default::default(),
      before_styles: None,
      after_styles: None,
      marker_styles: None,
      placeholder_styles: None,
      file_selector_button_styles: None,
      footnote_call_styles: None,
      footnote_marker_styles: None,
      first_line_styles: None,
      first_letter_styles: None,
      slider_thumb_styles: None,
      slider_track_styles: None,
      progress_bar_styles: None,
      progress_value_styles: None,
      meter_bar_styles: None,
      meter_optimum_value_styles: None,
      meter_suboptimum_value_styles: None,
      meter_even_less_good_value_styles: None,
      assigned_slot: None,
      slotted_node_ids: Vec::new(),
      children: Vec::new(),
    };

    assert_eq!(
      compute_expanded(&svg_details, None, &[], None),
      None,
      "non-HTML namespace elements must not inherit HTML <details> expanded semantics"
    );
  }
}
