use crate::dom::{DomNode, DomNodeType};
use crate::geometry::Point;
use crate::style::computed::Visibility;
use crate::style::types::{CursorKeyword, PointerEvents};
use crate::tree::box_tree::{BoxNode, BoxTree, BoxType};
use crate::tree::fragment_tree::{FragmentContent, FragmentTree};
use crate::ui::messages::CursorKind;
use rustc_hash::FxHashMap;
#[cfg(feature = "browser_ui")]
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::marker::PhantomData;
use std::ptr;

use super::effective_disabled::DomIdLookup;
use super::image_maps;
use super::engine::box_is_selectable_for_document_selection;

// -----------------------------------------------------------------------------
// Test hooks
// -----------------------------------------------------------------------------

/// When enabled, `hit_test_dom` increments a global counter on every call.
///
/// This is used by browser UI integration tests to assert that input handlers reuse the
/// interaction engine's hit-test results instead of performing redundant DOM hit-tests.
#[cfg(feature = "browser_ui")]
static HIT_TEST_DOM_COUNTING_ENABLED: AtomicBool = AtomicBool::new(false);
#[cfg(feature = "browser_ui")]
static HIT_TEST_DOM_CALL_COUNT: AtomicUsize = AtomicUsize::new(0);

/// Enable or disable `hit_test_dom` call counting (browser UI integration test hook).
#[cfg(feature = "browser_ui")]
pub fn set_hit_test_dom_counting_enabled_for_test(enabled: bool) {
  HIT_TEST_DOM_COUNTING_ENABLED.store(enabled, Ordering::Relaxed);
  if enabled {
    HIT_TEST_DOM_CALL_COUNT.store(0, Ordering::Relaxed);
  }
}

/// Reset the `hit_test_dom` call count to zero (browser UI integration test hook).
#[cfg(feature = "browser_ui")]
pub fn reset_hit_test_dom_call_count_for_test() {
  HIT_TEST_DOM_CALL_COUNT.store(0, Ordering::Relaxed);
}

/// Return the current `hit_test_dom` call count (browser UI integration test hook).
#[cfg(feature = "browser_ui")]
pub fn hit_test_dom_call_count_for_test() -> usize {
  HIT_TEST_DOM_CALL_COUNT.load(Ordering::Relaxed)
}

fn trim_ascii_whitespace(value: &str) -> &str {
  value.trim_matches(|c: char| {
    matches!(
      c,
      '\u{0009}' | '\u{000A}' | '\u{000C}' | '\u{000D}' | '\u{0020}'
    )
  })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HitTestResult {
  pub box_id: usize,
  /// The computed CSS `cursor` keyword for the hit box.
  pub css_cursor: CursorKeyword,
  /// True when the hit corresponds to a selectable text fragment, meaning a document selection can
  /// start at this location and the UI should show an I-beam cursor.
  pub is_selectable_text: bool,
  /// The resolved element's HTML `id` attribute (if any).
  ///
  /// This corresponds to `dom_node_id` (including `<img usemap>` → `<area>` resolution), and is
  /// only populated when the resolved node is an element with a non-empty `id` attribute.
  pub element_id: Option<String>,
  /// True when the resolved hit target is a text control that could accept dropped text.
  ///
  /// This is a fast pre-check that ignores inherited disabled/inert/hidden state (e.g. `<fieldset
  /// disabled>`). Callers that need the fully-effective "can accept drop" decision must still
  /// consult `effective_disabled`.
  pub is_editable_text_drop_target_candidate: bool,
  /// The default cursor kind for a form-control hit when CSS `cursor` is `auto`.
  pub form_control_cursor: CursorKind,
  pub styled_node_id: usize,
  pub dom_node_id: usize,
  pub kind: HitTestKind,
  pub href: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HitTestKind {
  Link,
  FormControl,
  Label,
  Other,
}

pub(crate) trait DomIdLookupExt: DomIdLookup {
  fn id_for_ptr(&self, ptr: *const DomNode) -> Option<usize> {
    if ptr.is_null() {
      return None;
    }
    // Pre-order node ids are expected to be stable, but `DomIdLookup` doesn't guarantee pointer
    // lookup. Fall back to a linear scan.
    for node_id in 1..=self.len() {
      let Some(node) = self.node(node_id) else {
        continue;
      };
      if (node as *const DomNode) == ptr {
        return Some(node_id);
      }
    }
    None
  }
}

// NOTE: We intentionally avoid a blanket `impl<T: DomIdLookup> DomIdLookupExt for T` so specific
// index implementations can provide a faster `id_for_ptr` (e.g. an O(1) pointer->id hash map)
// without unstable specialization.

impl DomIdLookupExt for DomIndex<'_> {
  #[inline]
  fn id_for_ptr(&self, ptr: *const DomNode) -> Option<usize> {
    DomIndex::id_for_ptr(self, ptr)
  }
}

impl DomIdLookupExt for super::dom_index::DomIndex {
  #[inline]
  fn id_for_ptr(&self, ptr: *const DomNode) -> Option<usize> {
    super::dom_index::DomIndex::id_for_ptr(self, ptr)
  }
}

pub(crate) struct BoxIndex<'a> {
  id_to_ptr: Vec<*const BoxNode>,
  parent: Vec<usize>,
  _marker: PhantomData<&'a BoxNode>,
}

impl<'a> BoxIndex<'a> {
  pub(crate) fn new(box_tree: &'a BoxTree) -> Self {
    let mut id_to_ptr: Vec<*const BoxNode> = vec![ptr::null()];
    let mut parent: Vec<usize> = vec![0];

    // (node, parent_box_id)
    let mut stack: Vec<(&BoxNode, usize)> = vec![(&box_tree.root, 0)];
    while let Some((node, parent_id)) = stack.pop() {
      let id = node.id;
      if id == 0 {
        // `BoxTree::new` assigns ids starting from 1; ignore any uninitialized nodes.
        continue;
      }
      if id >= id_to_ptr.len() {
        id_to_ptr.resize(id + 1, ptr::null());
        parent.resize(id + 1, 0);
      }
      id_to_ptr[id] = node as *const BoxNode;
      parent[id] = parent_id;

      // Mirror `assign_box_ids` traversal ordering.
      if let Some(body) = node.footnote_body.as_deref() {
        stack.push((body, id));
      }
      for child in node.children.iter().rev() {
        stack.push((child, id));
      }
    }

    Self {
      id_to_ptr,
      parent,
      _marker: PhantomData,
    }
  }

  pub(crate) fn node(&self, box_id: usize) -> Option<&BoxNode> {
    let ptr = *self.id_to_ptr.get(box_id)?;
    if ptr.is_null() {
      return None;
    }
    // SAFETY: `ptr` originates from a live `BoxTree` borrowed for the duration of `hit_test_dom`.
    Some(unsafe { &*ptr })
  }

  pub(crate) fn parent_id(&self, box_id: usize) -> Option<usize> {
    self.parent.get(box_id).copied()
  }
}

struct DomIndex<'a> {
  id_to_ptr: Vec<*const DomNode>,
  parent: Vec<usize>,
  // `id_to_ptr` is efficient for id -> pointer lookups, but image map hit-testing frequently needs
  // pointer -> id to resolve `<area>` elements back to cascade pre-order ids.
  //
  // Keep this optional to leave room for future memory/perf tuning, but construct it by default.
  ptr_to_id: Option<FxHashMap<*const DomNode, usize>>,
  _marker: PhantomData<&'a DomNode>,
}

impl<'a> DomIndex<'a> {
  fn new(dom: &'a DomNode) -> Self {
    let mut id_to_ptr: Vec<*const DomNode> = vec![ptr::null()];
    let mut parent: Vec<usize> = vec![0];
    let mut ptr_to_id: FxHashMap<*const DomNode, usize> = FxHashMap::default();

    // Pre-order traversal, matching `dom::enumerate_dom_ids` / cascade node ids.
    // (node, parent_dom_id)
    let mut stack: Vec<(&DomNode, usize)> = vec![(dom, 0)];
    while let Some((node, parent_id)) = stack.pop() {
      let id = id_to_ptr.len();
      let node_ptr = node as *const DomNode;
      debug_assert!(!node_ptr.is_null());
      id_to_ptr.push(node_ptr);
      parent.push(parent_id);
      ptr_to_id.insert(node_ptr, id);
      for child in node.children.iter().rev() {
        stack.push((child, id));
      }
    }

    Self {
      id_to_ptr,
      parent,
      ptr_to_id: Some(ptr_to_id),
      _marker: PhantomData,
    }
  }

  fn node(&self, node_id: usize) -> Option<&DomNode> {
    let ptr = *self.id_to_ptr.get(node_id)?;
    if ptr.is_null() {
      return None;
    }
    // SAFETY: `ptr` originates from a live `DomNode` borrowed for the duration of `hit_test_dom`.
    Some(unsafe { &*ptr })
  }

  fn parent_id(&self, node_id: usize) -> Option<usize> {
    self.parent.get(node_id).copied()
  }

  fn node_ids(&self) -> impl Iterator<Item = usize> + '_ {
    (1..self.id_to_ptr.len()).filter(|&id| !self.id_to_ptr[id].is_null())
  }

  fn id_for_ptr(&self, ptr: *const DomNode) -> Option<usize> {
    if ptr.is_null() {
      return None;
    }
    if let Some(map) = &self.ptr_to_id {
      return map.get(&ptr).copied();
    }
    self.id_to_ptr.iter().position(|&candidate| candidate == ptr)
  }

  fn is_ancestor(&self, ancestor: usize, mut node_id: usize) -> bool {
    while node_id != 0 {
      if node_id == ancestor {
        return true;
      }
      node_id = self.parent.get(node_id).copied().unwrap_or(0);
    }
    false
  }
}

impl<'a> DomIdLookup for DomIndex<'a> {
  fn len(&self) -> usize {
    self.id_to_ptr.len().saturating_sub(1)
  }

  fn node(&self, node_id: usize) -> Option<&DomNode> {
    self.node(node_id)
  }

  fn parent_id(&self, node_id: usize) -> usize {
    self.parent_id(node_id).unwrap_or(0)
  }
}

fn tree_root_boundary_id(dom_index: &DomIndex<'_>, mut node_id: usize) -> Option<usize> {
  while node_id != 0 {
    let node = dom_index.node(node_id)?;
    if matches!(
      node.node_type,
      DomNodeType::Document { .. } | DomNodeType::ShadowRoot { .. }
    ) {
      return Some(node_id);
    }
    node_id = dom_index.parent_id(node_id).unwrap_or(0);
  }
  None
}

fn node_or_ancestor_is_template(dom_index: &DomIndex<'_>, node_id: usize) -> bool {
  super::effective_disabled::is_in_template_contents(node_id, dom_index)
}

fn find_element_by_id_attr_in_tree(
  dom_index: &DomIndex<'_>,
  tree_root_id: usize,
  html_id: &str,
) -> Option<usize> {
  for node_id in dom_index.node_ids() {
    let Some(node) = dom_index.node(node_id) else {
      debug_assert!(false, "node_ids only yields valid ids");
      continue;
    };
    if !node.is_element() {
      continue;
    }
    if node_or_ancestor_is_template(dom_index, node_id) {
      continue;
    }
    if node.get_attribute_ref("id") != Some(html_id) {
      continue;
    }
    if tree_root_boundary_id(dom_index, node_id) == Some(tree_root_id) {
      return Some(node_id);
    }
  }
  None
}

fn box_is_interactive(box_node: &BoxNode) -> bool {
  let style = &box_node.style;
  style.pointer_events != PointerEvents::None
    && style.visibility == Visibility::Visible
    && style.inert == false
}

fn node_is_inert_like(node: &DomNode) -> bool {
  super::effective_disabled::node_self_is_inert(node)
}

fn node_is_link(node: &DomNode) -> Option<String> {
  let tag = node.tag_name()?;
  if !(tag.eq_ignore_ascii_case("a") || tag.eq_ignore_ascii_case("area")) {
    return None;
  }
  node.get_attribute_ref("href").map(|href| href.to_string())
}

fn node_is_form_control(node: &DomNode) -> bool {
  let Some(tag) = node.tag_name() else {
    return false;
  };

  if tag.eq_ignore_ascii_case("input") {
    let ty = trim_ascii_whitespace(node.get_attribute_ref("type").unwrap_or(""));
    return !ty.eq_ignore_ascii_case("hidden");
  }

  tag.eq_ignore_ascii_case("textarea")
    || tag.eq_ignore_ascii_case("select")
    || tag.eq_ignore_ascii_case("button")
}

fn node_is_text_input(node: &DomNode) -> bool {
  let Some(tag) = node.tag_name() else {
    return false;
  };
  if !tag.eq_ignore_ascii_case("input") {
    return false;
  }

  let ty = trim_ascii_whitespace(node.get_attribute_ref("type").unwrap_or(""));
  let ty = if ty.is_empty() { "text" } else { ty };

  // MVP heuristic: treat any non-button-ish, non-choice-ish input as a text control (mirrors
  // `interaction::engine::is_text_input`).
  !ty.eq_ignore_ascii_case("checkbox")
    && !ty.eq_ignore_ascii_case("radio")
    && !ty.eq_ignore_ascii_case("button")
    && !ty.eq_ignore_ascii_case("submit")
    && !ty.eq_ignore_ascii_case("reset")
    && !ty.eq_ignore_ascii_case("hidden")
    && !ty.eq_ignore_ascii_case("range")
    && !ty.eq_ignore_ascii_case("color")
    && !ty.eq_ignore_ascii_case("file")
    && !ty.eq_ignore_ascii_case("image")
}

fn node_is_text_control(node: &DomNode) -> bool {
  node
    .tag_name()
    .is_some_and(|tag| tag.eq_ignore_ascii_case("textarea"))
    || node_is_text_input(node)
}

fn cursor_for_form_control(node: &DomNode) -> CursorKind {
  // Mirror the UI worker's cursor heuristic for form controls so hover cursor behaviour is
  // consistent across embedding layers.
  //
  // Even when `cursor: auto` is in effect, disabled controls should not show an I-beam: they are
  // not editable/interactive.
  if node.get_attribute_ref("disabled").is_some() {
    return CursorKind::Default;
  }

  node_is_text_control(node)
    .then_some(CursorKind::Text)
    .unwrap_or(CursorKind::Default)
}

fn is_editable_text_drop_target_candidate(node: &DomNode) -> bool {
  // Fast pre-check for whether this element could accept a dragged text payload.
  //
  // Disabled/inert/hidden state can be inherited from ancestors (e.g. `<fieldset disabled>`), so the
  // full "is this actually editable?" decision must consult `effective_disabled` via a DOM index.
  node_is_text_control(node) && node.get_attribute_ref("readonly").is_none()
}

fn resolve_styled_node_id_from_box_ancestors(
  box_index: &BoxIndex<'_>,
  mut box_id: usize,
) -> Option<usize> {
  while box_id != 0 {
    let node = box_index.node(box_id)?;
    if let Some(styled_node_id) = node.styled_node_id {
      return Some(styled_node_id);
    }
    box_id = box_index.parent_id(box_id).unwrap_or(0);
  }
  None
}

enum SemanticResolveResult {
  Hit {
    node_id: usize,
    kind: HitTestKind,
    href: Option<String>,
  },
  /// The starting node (or one of its ancestors) is in an inert subtree.
  ///
  /// Inert subtrees must be excluded from user interaction, but they should *not* block hit-testing
  /// from falling through to underlying content (similar to `pointer-events: none`).
  InertSubtree,
  Invalid,
}

fn resolve_semantic_target(
  dom_index: &(impl DomIdLookup + ?Sized),
  start_node_id: usize,
) -> SemanticResolveResult {
  if dom_index.node(start_node_id).is_none() {
    return SemanticResolveResult::Invalid;
  }

  let mut current = start_node_id;
  let mut first_element: Option<usize> = None;

  while current != 0 {
    let Some(node) = dom_index.node(current) else {
      return SemanticResolveResult::Invalid;
    };

    if node.is_element() {
      if node_is_inert_like(node) {
        // Mark inert subtrees so hit-testing can skip these fragments and fall through to any
        // underlying, non-inert content.
        return SemanticResolveResult::InertSubtree;
      }

      first_element.get_or_insert(current);

      if let Some(href) = node_is_link(node) {
        return SemanticResolveResult::Hit {
          node_id: current,
          kind: HitTestKind::Link,
          href: Some(href),
        };
      }
      if node_is_form_control(node) {
        return SemanticResolveResult::Hit {
          node_id: current,
          kind: HitTestKind::FormControl,
          href: None,
        };
      }
      if matches!(node.tag_name(), Some(tag) if tag.eq_ignore_ascii_case("label")) {
        return SemanticResolveResult::Hit {
          node_id: current,
          kind: HitTestKind::Label,
          href: None,
        };
      }
    }

    current = dom_index.parent_id(current);
  }

  match first_element {
    Some(node_id) => SemanticResolveResult::Hit {
      node_id,
      kind: HitTestKind::Other,
      href: None,
    },
    None => SemanticResolveResult::Invalid,
  }
}

pub(crate) fn hit_test_dom_with_indices<D: DomIdLookupExt + ?Sized>(
  dom: &DomNode,
  dom_index: &D,
  box_index: &BoxIndex<'_>,
  fragment_tree: &FragmentTree,
  point: Point,
) -> Option<HitTestResult> {
  #[cfg(feature = "browser_ui")]
  if HIT_TEST_DOM_COUNTING_ENABLED.load(Ordering::Relaxed) {
    HIT_TEST_DOM_CALL_COUNT.fetch_add(1, Ordering::Relaxed);
  }

  for fragment in fragment_tree.hit_test(point) {
    let Some(box_id) = fragment.box_id() else {
      continue;
    };
    let Some(box_node) = box_index.node(box_id) else {
      continue;
    };
    if !box_is_interactive(box_node) {
      continue;
    }

    let css_cursor = box_node.style.cursor;
    let is_selectable_text =
      matches!(&fragment.content, FragmentContent::Text { is_marker, .. } if !*is_marker)
        && matches!(&box_node.box_type, BoxType::Text(_))
        && box_is_selectable_for_document_selection(box_node);

    let Some(styled_node_id) = resolve_styled_node_id_from_box_ancestors(box_index, box_id) else {
      continue;
    };

    // MVP: styled node ids are the cascade DOM pre-order ids.
    let dom_node_id = styled_node_id;

    let (semantic_dom_node_id, mut kind, mut href) =
      match resolve_semantic_target(dom_index, dom_node_id) {
        SemanticResolveResult::Hit {
          node_id,
          kind,
          href,
        } => (node_id, kind, href),
        SemanticResolveResult::InertSubtree => {
          // Inert subtrees are excluded from interaction; treat them like a non-interactive
          // fragment so hit-testing can fall through to any underlying targets.
          continue;
        }
        SemanticResolveResult::Invalid => continue,
      };

    let mut resolved_dom_node_id = semantic_dom_node_id;

    // Client-side image maps: `<img usemap>` hit-testing resolves to the associated `<area>`.
    if dom_index
      .node(dom_node_id)
      .and_then(|node| node.tag_name())
      .is_some_and(|tag| tag.eq_ignore_ascii_case("img"))
    {
      if let Some(usemap) = dom_index
        .node(dom_node_id)
        .and_then(|node| node.get_attribute_ref("usemap"))
      {
        if let Some(image_point) =
          image_maps::local_point_in_fragment(fragment_tree, fragment, point)
        {
          if let Some(area) = image_maps::hit_test_image_map(dom, usemap, image_point) {
            if let Some(area_id) = dom_index.id_for_ptr(area as *const DomNode) {
              resolved_dom_node_id = area_id;
              if let Some(area_href) = area.get_attribute_ref("href") {
                kind = HitTestKind::Link;
                href = Some(area_href.to_string());
              } else {
                kind = HitTestKind::Other;
                href = None;
              }
            }
          }
        }
      }
    }

    let element_id = dom_index
      .node(resolved_dom_node_id)
      .filter(|node| node.is_element())
      .and_then(|node| node.get_attribute_ref("id"))
      .filter(|id| !id.is_empty())
      .map(|id| id.to_string());
    let (editable_text_drop_target_candidate, form_control_cursor) = dom_index
      .node(resolved_dom_node_id)
      .map(|node| {
        (
          is_editable_text_drop_target_candidate(node),
          cursor_for_form_control(node),
        )
      })
      .unwrap_or((false, CursorKind::Default));

    return Some(HitTestResult {
      box_id,
      css_cursor,
      is_selectable_text,
      element_id,
      is_editable_text_drop_target_candidate: editable_text_drop_target_candidate,
      form_control_cursor,
      styled_node_id,
      dom_node_id: resolved_dom_node_id,
      kind,
      href,
    });
  }

  None
}

pub fn hit_test_dom(
  dom: &DomNode,
  box_tree: &BoxTree,
  fragment_tree: &FragmentTree,
  point: Point,
) -> Option<HitTestResult> {
  let box_index = BoxIndex::new(box_tree);
  let dom_index = DomIndex::new(dom);
  hit_test_dom_with_indices(dom, &dom_index, &box_index, fragment_tree, point)
}

/// Like [`hit_test_dom`], but returns *all* hit targets (topmost first).
///
/// This is a convenience for APIs like `Document.elementsFromPoint()` that need to enumerate the
/// stacking order at a viewport coordinate.
pub fn hit_test_dom_all(
  dom: &DomNode,
  box_tree: &BoxTree,
  fragment_tree: &FragmentTree,
  point: Point,
) -> Vec<HitTestResult> {
  let box_index = BoxIndex::new(box_tree);
  let dom_index = DomIndex::new(dom);
  hit_test_dom_all_with_indices(dom, &dom_index, &box_index, fragment_tree, point)
}

pub(crate) fn hit_test_dom_all_with_indices<D: DomIdLookupExt + ?Sized>(
  dom: &DomNode,
  dom_index: &D,
  box_index: &BoxIndex<'_>,
  fragment_tree: &FragmentTree,
  point: Point,
) -> Vec<HitTestResult> {
  #[cfg(feature = "browser_ui")]
  if HIT_TEST_DOM_COUNTING_ENABLED.load(Ordering::Relaxed) {
    HIT_TEST_DOM_CALL_COUNT.fetch_add(1, Ordering::Relaxed);
  }

  let mut results: Vec<HitTestResult> = Vec::new();
  let mut seen: std::collections::HashSet<usize> = std::collections::HashSet::new();

  for fragment in fragment_tree.hit_test(point) {
    let Some(box_id) = fragment.box_id() else {
      continue;
    };
    let Some(box_node) = box_index.node(box_id) else {
      continue;
    };
    if !box_is_interactive(box_node) {
      continue;
    }

    let css_cursor = box_node.style.cursor;
    let is_selectable_text =
      matches!(&fragment.content, FragmentContent::Text { is_marker, .. } if !*is_marker)
        && matches!(&box_node.box_type, BoxType::Text(_))
        && box_is_selectable_for_document_selection(box_node);

    let Some(styled_node_id) = resolve_styled_node_id_from_box_ancestors(box_index, box_id) else {
      continue;
    };

    // MVP: styled node ids are the cascade DOM pre-order ids.
    let dom_node_id = styled_node_id;

    let (semantic_dom_node_id, mut kind, mut href) =
      match resolve_semantic_target(dom_index, dom_node_id) {
        SemanticResolveResult::Hit {
          node_id,
          kind,
          href,
        } => (node_id, kind, href),
        SemanticResolveResult::InertSubtree => {
          // Inert subtrees are excluded from interaction; skip these hits so callers can enumerate
          // underlying targets (fallthrough semantics).
          continue;
        }
        SemanticResolveResult::Invalid => continue,
      };

    let mut resolved_dom_node_id = semantic_dom_node_id;

    // Client-side image maps: `<img usemap>` hit-testing resolves to the associated `<area>`.
    if dom_index
      .node(dom_node_id)
      .and_then(|node| node.tag_name())
      .is_some_and(|tag| tag.eq_ignore_ascii_case("img"))
    {
      if let Some(usemap) = dom_index
        .node(dom_node_id)
        .and_then(|node| node.get_attribute_ref("usemap"))
      {
        if let Some(image_point) =
          image_maps::local_point_in_fragment(fragment_tree, fragment, point)
        {
          if let Some(area) = image_maps::hit_test_image_map(dom, usemap, image_point) {
            if let Some(area_id) = dom_index.id_for_ptr(area as *const DomNode) {
              resolved_dom_node_id = area_id;
              if let Some(area_href) = area.get_attribute_ref("href") {
                kind = HitTestKind::Link;
                href = Some(area_href.to_string());
              } else {
                kind = HitTestKind::Other;
                href = None;
              }
            }
          }
        }
      }
    }

    if !seen.insert(resolved_dom_node_id) {
      continue;
    }

    let element_id = dom_index
      .node(resolved_dom_node_id)
      .filter(|node| node.is_element())
      .and_then(|node| node.get_attribute_ref("id"))
      .filter(|id| !id.is_empty())
      .map(|id| id.to_string());
    let (editable_text_drop_target_candidate, form_control_cursor) = dom_index
      .node(resolved_dom_node_id)
      .map(|node| {
        (
          is_editable_text_drop_target_candidate(node),
          cursor_for_form_control(node),
        )
      })
      .unwrap_or((false, CursorKind::Default));

    results.push(HitTestResult {
      box_id,
      css_cursor,
      is_selectable_text,
      element_id,
      is_editable_text_drop_target_candidate: editable_text_drop_target_candidate,
      form_control_cursor,
      styled_node_id,
      dom_node_id: resolved_dom_node_id,
      kind,
      href,
    });
  }

  results
}

pub fn resolve_label_associated_control(dom: &DomNode, label_node_id: usize) -> Option<usize> {
  let dom_index = DomIndex::new(dom);
  let label = dom_index.node(label_node_id)?;

  if !matches!(label.tag_name(), Some(tag) if tag.eq_ignore_ascii_case("label")) {
    return None;
  }

  let label_tree_root = tree_root_boundary_id(&dom_index, label_node_id)?;
  if let Some(for_value) = label
    .get_attribute_ref("for")
    .map(trim_ascii_whitespace)
    .filter(|v| !v.is_empty())
  {
    let referenced = find_element_by_id_attr_in_tree(&dom_index, label_tree_root, for_value)?;
    return dom_index
      .node(referenced)
      .is_some_and(node_is_form_control)
      .then_some(referenced);
  }

  // No explicit `for` => first descendant control element inside the label.
  for candidate_id in dom_index.node_ids() {
    if candidate_id == label_node_id {
      continue;
    }
    if !dom_index.is_ancestor(label_node_id, candidate_id) {
      continue;
    }
    let Some(node) = dom_index.node(candidate_id) else {
      debug_assert!(false, "node_ids only yields valid ids");
      continue;
    };
    if node_is_form_control(node) {
      return Some(candidate_id);
    }
  }

  None
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::interaction::cursor::cursor_kind_for_hit;
  use crate::geometry::Point;

  fn find_element_node_id(dom: &DomNode, tag: &str) -> usize {
    let index = DomIndex::new(dom);
    let found = index.node_ids().find(|&id| {
      index
        .node(id)
        .and_then(|node| node.tag_name())
        .is_some_and(|name| name.eq_ignore_ascii_case(tag))
    });
    found.unwrap_or_else(|| panic!("missing element {tag}"))
  }

  #[test]
  fn non_ascii_whitespace_hit_test_input_type_does_not_trim_nbsp() {
    let nbsp = "\u{00A0}";
    let html = format!("<html><body><input type=\"{nbsp}hidden\"></body></html>");
    let dom = crate::dom::parse_html(&html).expect("parse");
    let input_id = find_element_node_id(&dom, "input");

    let index = DomIndex::new(&dom);
    let input = index.node(input_id).expect("input node");
    assert!(
      node_is_form_control(input),
      "NBSP must not be treated as ASCII whitespace when checking input type"
    );
  }

  #[test]
  fn non_ascii_whitespace_label_for_does_not_trim_nbsp() {
    let nbsp = "\u{00A0}";
    let html =
      format!("<html><body><label for=\"{nbsp}x\">Label</label><input id=\"x\"></body></html>");
    let dom = crate::dom::parse_html(&html).expect("parse");
    let label_id = find_element_node_id(&dom, "label");

    assert_eq!(
      resolve_label_associated_control(&dom, label_id),
      None,
      "NBSP must not be treated as ASCII whitespace when resolving label for= associations"
    );
  }

  #[test]
  fn cursor_for_form_control_text_input_is_text() {
    let dom = crate::dom::parse_html("<html><body><input></body></html>").expect("parse");
    let input_id = find_element_node_id(&dom, "input");
    let index = DomIndex::new(&dom);
    let node = index.node(input_id).expect("input node");
    assert_eq!(cursor_for_form_control(node), CursorKind::Text);
  }

  #[test]
  fn cursor_for_form_control_checkbox_input_is_default() {
    let dom =
      crate::dom::parse_html("<html><body><input type=\"checkbox\"></body></html>").expect("parse");
    let input_id = find_element_node_id(&dom, "input");
    let index = DomIndex::new(&dom);
    let node = index.node(input_id).expect("input node");
    assert_eq!(cursor_for_form_control(node), CursorKind::Default);
  }

  #[test]
  fn cursor_for_form_control_textarea_is_text() {
    let dom =
      crate::dom::parse_html("<html><body><textarea>hi</textarea></body></html>").expect("parse");
    let textarea_id = find_element_node_id(&dom, "textarea");
    let index = DomIndex::new(&dom);
    let node = index.node(textarea_id).expect("textarea node");
    assert_eq!(cursor_for_form_control(node), CursorKind::Text);
  }

  #[test]
  fn cursor_for_form_control_disabled_text_input_is_default() {
    let dom =
      crate::dom::parse_html("<html><body><input disabled></body></html>").expect("parse");
    let input_id = find_element_node_id(&dom, "input");
    let index = DomIndex::new(&dom);
    let node = index.node(input_id).expect("input node");
    assert_eq!(cursor_for_form_control(node), CursorKind::Default);
  }

  #[test]
  fn cursor_kind_for_hit_none_is_default() {
    assert_eq!(cursor_kind_for_hit(None), CursorKind::Default);
  }

  #[test]
  fn cursor_kind_for_hit_link_is_pointer() {
    let dom =
      crate::dom::parse_html("<html><body><a href=\"x\">Link</a></body></html>").expect("parse");
    let a_id = find_element_node_id(&dom, "a");
    let hit = HitTestResult {
      box_id: 1,
      css_cursor: CursorKeyword::Auto,
      is_selectable_text: false,
      element_id: None,
      is_editable_text_drop_target_candidate: false,
      form_control_cursor: CursorKind::Default,
      styled_node_id: a_id,
      dom_node_id: a_id,
      kind: HitTestKind::Link,
      href: Some("x".to_string()),
    };
    assert_eq!(cursor_kind_for_hit(Some(&hit)), CursorKind::Pointer);
  }

  #[test]
  fn cursor_kind_for_hit_text_input_is_text() {
    let dom = crate::dom::parse_html("<html><body><input></body></html>").expect("parse");
    let input_id = find_element_node_id(&dom, "input");
    let hit = HitTestResult {
      box_id: 1,
      css_cursor: CursorKeyword::Auto,
      is_selectable_text: false,
      element_id: None,
      is_editable_text_drop_target_candidate: false,
      form_control_cursor: CursorKind::Text,
      styled_node_id: input_id,
      dom_node_id: input_id,
      kind: HitTestKind::FormControl,
      href: None,
    };
    assert_eq!(cursor_kind_for_hit(Some(&hit)), CursorKind::Text);
  }

  #[test]
  fn cursor_kind_for_hit_checkbox_input_is_default() {
    let dom =
      crate::dom::parse_html("<html><body><input type=\"checkbox\"></body></html>").expect("parse");
    let input_id = find_element_node_id(&dom, "input");
    let hit = HitTestResult {
      box_id: 1,
      css_cursor: CursorKeyword::Auto,
      is_selectable_text: false,
      element_id: None,
      is_editable_text_drop_target_candidate: false,
      form_control_cursor: CursorKind::Default,
      styled_node_id: input_id,
      dom_node_id: input_id,
      kind: HitTestKind::FormControl,
      href: None,
    };
    assert_eq!(cursor_kind_for_hit(Some(&hit)), CursorKind::Default);
  }

  #[test]
  fn cursor_kind_for_hit_textarea_is_text() {
    let dom =
      crate::dom::parse_html("<html><body><textarea>hi</textarea></body></html>").expect("parse");
    let textarea_id = find_element_node_id(&dom, "textarea");
    let hit = HitTestResult {
      box_id: 1,
      css_cursor: CursorKeyword::Auto,
      is_selectable_text: false,
      element_id: None,
      is_editable_text_drop_target_candidate: false,
      form_control_cursor: CursorKind::Text,
      styled_node_id: textarea_id,
      dom_node_id: textarea_id,
      kind: HitTestKind::FormControl,
      href: None,
    };
    assert_eq!(cursor_kind_for_hit(Some(&hit)), CursorKind::Text);
  }
  fn prepare_for_hit_testing(html: &str) -> crate::api::PreparedDocument {
    let mut renderer = crate::api::FastRender::new().expect("renderer");
    let options = crate::api::RenderOptions::new().with_viewport(256, 128);
    renderer.prepare_html(html, options).expect("prepare html")
  }

  #[test]
  fn hit_test_reports_selectable_text_for_document_cursor() {
    let prepared = prepare_for_hit_testing(
      r#"
        <style>
          html, body { margin: 0; padding: 0; }
          p { margin: 0; }
          #text { position: absolute; top: 10px; left: 10px; }
        </style>
        <p id="text">Plain text</p>
      "#,
    );

    let hit = hit_test_dom(
      prepared.dom(),
      prepared.box_tree(),
      prepared.fragment_tree(),
      Point::new(15.0, 15.0),
    )
    .expect("hit");

    assert!(hit.is_selectable_text, "expected selectable text hit");
    let cursor = if hit.is_selectable_text {
      CursorKind::Text
    } else {
      CursorKind::Default
    };
    assert_eq!(cursor, CursorKind::Text);
  }

  #[test]
  fn hit_test_does_not_report_text_cursor_when_user_select_none() {
    let prepared = prepare_for_hit_testing(
      r#"
        <style>
          html, body { margin: 0; padding: 0; }
          p { margin: 0; }
          #text { position: absolute; top: 10px; left: 10px; user-select: none; }
        </style>
        <p id="text">Unselectable</p>
      "#,
    );

    let hit = hit_test_dom(
      prepared.dom(),
      prepared.box_tree(),
      prepared.fragment_tree(),
      Point::new(15.0, 15.0),
    )
    .expect("hit");

    assert!(!hit.is_selectable_text, "user-select:none must not be selectable");
  }

  #[test]
  fn hit_test_does_not_report_text_cursor_for_non_text_boxes() {
    let prepared = prepare_for_hit_testing(
      r#"
        <style>
          html, body { margin: 0; padding: 0; }
          #box { position: absolute; top: 10px; left: 10px; width: 100px; height: 20px; background: rgb(0, 0, 0); }
        </style>
        <div id="box"></div>
      "#,
    );

    let hit = hit_test_dom(
      prepared.dom(),
      prepared.box_tree(),
      prepared.fragment_tree(),
      Point::new(15.0, 15.0),
    )
    .expect("hit");

    assert!(
      !hit.is_selectable_text,
      "non-text boxes must not be treated as selectable text"
    );
  }
}

#[cfg(test)]
mod dom_hit_testing_tests {
  use super::{
    hit_test_dom, hit_test_dom_all, hit_test_dom_with_indices, resolve_label_associated_control,
    BoxIndex as HitTestBoxIndex, CursorKind, DomIndex, HitTestKind,
  };
  use crate::dom::{DomNode, DomNodeType, ShadowRootMode};
  use crate::geometry::{Point, Rect};
  use crate::interaction::dom_index::DomIndex as MutableDomIndex;
  use crate::style::display::FormattingContextType;
  use crate::style::types::PointerEvents;
  use crate::style::ComputedStyle;
  use crate::tree::box_tree::{BoxNode, BoxTree};
  use crate::tree::fragment_tree::{FragmentNode, FragmentTree};
  use selectors::context::QuirksMode;
  use std::sync::Arc;

  fn doc(children: Vec<DomNode>) -> DomNode {
    DomNode {
      node_type: DomNodeType::Document {
        quirks_mode: QuirksMode::NoQuirks,
        scripting_enabled: true,
        is_html_document: true,
      },
      children,
    }
  }

  fn elem(tag: &str, attrs: Vec<(&str, &str)>, children: Vec<DomNode>) -> DomNode {
    DomNode {
      node_type: DomNodeType::Element {
        tag_name: tag.to_string(),
        namespace: String::new(),
        attributes: attrs
          .into_iter()
          .map(|(k, v)| (k.to_string(), v.to_string()))
          .collect(),
      },
      children,
    }
  }

  fn text(content: &str) -> DomNode {
    DomNode {
      node_type: DomNodeType::Text {
        content: content.to_string(),
      },
      children: Vec::new(),
    }
  }

  fn default_style() -> Arc<ComputedStyle> {
    Arc::new(ComputedStyle::default())
  }

  fn hit_test_with_prebuilt_indices(
    dom: &mut DomNode,
    box_tree: &BoxTree,
    fragment_tree: &FragmentTree,
    point: Point,
  ) -> Option<super::HitTestResult> {
    let box_index = HitTestBoxIndex::new(box_tree);
    let dom_index = MutableDomIndex::build(dom);
    hit_test_dom_with_indices(dom, &dom_index, &box_index, fragment_tree, point)
  }

  #[test]
  fn dom_index_id_for_ptr_maps_pointers_to_preorder_ids() {
    let dom = doc(vec![elem(
      "div",
      vec![("id", "root")],
      vec![elem("span", vec![], vec![]), text("hi")],
    )]);

    // DOM ids (pre-order):
    // 1 document
    // 2 div#root
    // 3 span
    // 4 text("hi")
    let index = DomIndex::new(&dom);
    assert_eq!(index.id_for_ptr(&dom as *const DomNode), Some(1));
    assert_eq!(
      index.id_for_ptr(&dom.children[0] as *const DomNode),
      Some(2)
    );
    assert_eq!(
      index.id_for_ptr(&dom.children[0].children[0] as *const DomNode),
      Some(3)
    );
    assert_eq!(
      index.id_for_ptr(&dom.children[0].children[1] as *const DomNode),
      Some(4)
    );
  }

  #[test]
  fn hit_test_dom_resolves_link_ancestor() {
    let dom = doc(vec![elem(
      "a",
      vec![("id", "link"), ("href", "/foo")],
      vec![elem("span", vec![], vec![text("txt")])],
    )]);

    // DOM ids (pre-order):
    // 1 document
    // 2 a
    // 3 span
    // 4 text
    let style = default_style();

    let mut dummy_text = BoxNode::new_text(style.clone(), "txt".to_string());
    dummy_text.styled_node_id = Some(4);

    let anonymous = BoxNode::new_anonymous_inline(style.clone(), vec![]);

    let mut span = BoxNode::new_inline(style.clone(), vec![dummy_text, anonymous]);
    span.styled_node_id = Some(3);

    let mut a_box = BoxNode::new_inline(style, vec![span]);
    a_box.styled_node_id = Some(2);

    let box_tree = BoxTree::new(a_box);
    let anonymous_box_id = box_tree.root.children[0].children[1].id;

    let hit_fragment = FragmentNode::new_block_with_id(
      Rect::from_xywh(0.0, 0.0, 50.0, 50.0),
      anonymous_box_id,
      vec![],
    );
    let root_fragment = FragmentNode::new_block_with_id(
      Rect::from_xywh(0.0, 0.0, 100.0, 100.0),
      box_tree.root.id,
      vec![hit_fragment],
    );
    let fragment_tree = FragmentTree::new(root_fragment);

    let result = hit_test_dom(&dom, &box_tree, &fragment_tree, Point::new(10.0, 10.0))
      .expect("expected a hit result");
    assert_eq!(result.box_id, anonymous_box_id);
    assert_eq!(result.styled_node_id, 3);
    assert_eq!(result.dom_node_id, 2);
    assert_eq!(result.element_id.as_deref(), Some("link"));
    assert!(!result.is_editable_text_drop_target_candidate);
    assert_eq!(result.form_control_cursor, CursorKind::Default);
    assert_eq!(result.kind, HitTestKind::Link);
    assert_eq!(result.href.as_deref(), Some("/foo"));
  }

  fn image_map_fixture() -> (DomNode, BoxTree, FragmentTree, usize, usize, usize, usize) {
    let dom = doc(vec![elem(
      "div",
      vec![("id", "container")],
      vec![
        elem("img", vec![("id", "img"), ("usemap", "#m")], vec![]),
        elem(
          "map",
          vec![("id", "m")],
          vec![
            elem(
              "area",
              vec![
                ("id", "a1"),
                ("shape", "rect"),
                ("coords", "0,0,10,10"),
                ("href", "/first"),
              ],
              vec![],
            ),
            elem(
              "area",
              vec![
                ("id", "a2"),
                ("shape", "rect"),
                ("coords", "0,0,10,10"),
                ("href", "/second"),
              ],
              vec![],
            ),
            elem(
              "area",
              vec![("id", "dead"), ("shape", "rect"), ("coords", "20,20,30,30")],
              vec![],
            ),
          ],
        ),
      ],
    )]);

    let index = DomIndex::new(&dom);
    let container_id = index
      .id_for_ptr(&dom.children[0] as *const DomNode)
      .expect("container id");
    let img_id = index
      .id_for_ptr(&dom.children[0].children[0] as *const DomNode)
      .expect("img id");
    let area1_id = index
      .id_for_ptr(&dom.children[0].children[1].children[0] as *const DomNode)
      .expect("area1 id");
    let dead_id = index
      .id_for_ptr(&dom.children[0].children[1].children[2] as *const DomNode)
      .expect("dead id");

    let mut img_box = BoxNode::new_block(default_style(), FormattingContextType::Block, vec![]);
    img_box.styled_node_id = Some(img_id);
    let mut container_box =
      BoxNode::new_block(default_style(), FormattingContextType::Block, vec![img_box]);
    container_box.styled_node_id = Some(container_id);
    let box_tree = BoxTree::new(container_box);
    let img_box_id = box_tree.root.children[0].id;

    // Root fragment is offset to ensure image-map coordinate mapping accounts for ancestor offsets.
    let img_fragment = FragmentNode::new_block_with_id(
      Rect::from_xywh(10.0, 10.0, 100.0, 100.0),
      img_box_id,
      vec![],
    );
    let root_fragment = FragmentNode::new_block_with_id(
      Rect::from_xywh(50.0, 50.0, 200.0, 200.0),
      box_tree.root.id,
      vec![img_fragment],
    );
    let fragment_tree = FragmentTree::new(root_fragment);

    (
      dom,
      box_tree,
      fragment_tree,
      img_box_id,
      img_id,
      area1_id,
      dead_id,
    )
  }

  #[test]
  fn hit_test_dom_resolves_img_usemap_area_links() {
    let (dom, box_tree, fragment_tree, img_box_id, img_id, area1_id, _) = image_map_fixture();

    let result =
      hit_test_dom(&dom, &box_tree, &fragment_tree, Point::new(65.0, 65.0)).expect("hit");
    assert_eq!(result.box_id, img_box_id);
    assert_eq!(result.styled_node_id, img_id);
    assert_eq!(result.dom_node_id, area1_id);
    assert_eq!(result.element_id.as_deref(), Some("a1"));
    assert!(!result.is_editable_text_drop_target_candidate);
    assert_eq!(result.form_control_cursor, CursorKind::Default);
    assert_eq!(result.kind, HitTestKind::Link);
    assert_eq!(result.href.as_deref(), Some("/first"));
  }

  #[test]
  fn hit_test_dom_resolves_img_usemap_area_without_href_as_other() {
    let (dom, box_tree, fragment_tree, _, _, _, dead_id) = image_map_fixture();

    let result =
      hit_test_dom(&dom, &box_tree, &fragment_tree, Point::new(85.0, 85.0)).expect("hit");
    assert_eq!(result.dom_node_id, dead_id);
    assert_eq!(result.element_id.as_deref(), Some("dead"));
    assert_eq!(result.kind, HitTestKind::Other);
    assert_eq!(result.href, None);
  }

  #[test]
  fn hit_test_dom_resolves_form_control() {
    let dom = doc(vec![elem(
      "input",
      vec![("id", "x"), ("type", "text")],
      vec![],
    )]);

    // DOM ids (pre-order):
    // 1 document
    // 2 input
    let style = default_style();
    let mut input_box = BoxNode::new_inline(style, vec![]);
    input_box.styled_node_id = Some(2);

    let box_tree = BoxTree::new(input_box);
    let input_box_id = box_tree.root.id;

    let root_fragment = FragmentNode::new_block_with_id(
      Rect::from_xywh(0.0, 0.0, 100.0, 100.0),
      input_box_id,
      vec![],
    );
    let fragment_tree = FragmentTree::new(root_fragment);

    let result = hit_test_dom(&dom, &box_tree, &fragment_tree, Point::new(10.0, 10.0))
      .expect("expected a hit result");
    assert_eq!(result.box_id, input_box_id);
    assert_eq!(result.styled_node_id, 2);
    assert_eq!(result.dom_node_id, 2);
    assert_eq!(result.element_id.as_deref(), Some("x"));
    assert!(result.is_editable_text_drop_target_candidate);
    assert_eq!(result.form_control_cursor, CursorKind::Text);
    assert_eq!(result.kind, HitTestKind::FormControl);
    assert_eq!(result.href, None);
  }

  #[test]
  fn hit_test_dom_skips_pointer_events_none() {
    let dom = doc(vec![elem(
      "div",
      vec![("id", "root")],
      vec![
        elem("a", vec![("href", "/ok")], vec![]),
        elem("div", vec![("id", "overlay")], vec![]),
      ],
    )]);

    // DOM ids (pre-order):
    // 1 document
    // 2 div#root
    // 3 a[href]
    // 4 div#overlay
    let style = default_style();
    let mut overlay_style = ComputedStyle::default();
    overlay_style.pointer_events = PointerEvents::None;
    let overlay_style = Arc::new(overlay_style);

    let mut link_box = BoxNode::new_inline(style.clone(), vec![]);
    link_box.styled_node_id = Some(3);

    let mut overlay_box = BoxNode::new_block(overlay_style, FormattingContextType::Block, vec![]);
    overlay_box.styled_node_id = Some(4);

    let mut root_box = BoxNode::new_block(
      style,
      FormattingContextType::Block,
      vec![link_box, overlay_box],
    );
    root_box.styled_node_id = Some(2);

    let box_tree = BoxTree::new(root_box);
    let link_box_id = box_tree.root.children[0].id;
    let overlay_box_id = box_tree.root.children[1].id;

    let link_fragment =
      FragmentNode::new_block_with_id(Rect::from_xywh(0.0, 0.0, 100.0, 100.0), link_box_id, vec![]);
    let overlay_fragment = FragmentNode::new_block_with_id(
      Rect::from_xywh(0.0, 0.0, 100.0, 100.0),
      overlay_box_id,
      vec![],
    );
    let root_fragment = FragmentNode::new_block_with_id(
      Rect::from_xywh(0.0, 0.0, 100.0, 100.0),
      box_tree.root.id,
      vec![link_fragment, overlay_fragment],
    );
    let fragment_tree = FragmentTree::new(root_fragment);

    let result = hit_test_dom(&dom, &box_tree, &fragment_tree, Point::new(10.0, 10.0))
      .expect("expected a hit result");

    assert_eq!(result.box_id, link_box_id);
    assert_eq!(result.dom_node_id, 3);
    assert_eq!(result.kind, HitTestKind::Link);
    assert_eq!(result.href.as_deref(), Some("/ok"));
  }

  #[test]
  fn hit_test_dom_returns_none_for_inert_subtree() {
    let dom = doc(vec![elem(
      "a",
      vec![("href", "/foo"), ("inert", "")],
      vec![elem("span", vec![], vec![text("txt")])],
    )]);

    let style = default_style();

    let anonymous = BoxNode::new_anonymous_inline(style.clone(), vec![]);

    let mut span = BoxNode::new_inline(style.clone(), vec![anonymous]);
    span.styled_node_id = Some(3);

    let mut a_box = BoxNode::new_inline(style, vec![span]);
    a_box.styled_node_id = Some(2);

    let box_tree = BoxTree::new(a_box);
    let anonymous_box_id = box_tree.root.children[0].children[0].id;

    let hit_fragment = FragmentNode::new_block_with_id(
      Rect::from_xywh(0.0, 0.0, 50.0, 50.0),
      anonymous_box_id,
      vec![],
    );
    let root_fragment = FragmentNode::new_block_with_id(
      Rect::from_xywh(0.0, 0.0, 100.0, 100.0),
      box_tree.root.id,
      vec![hit_fragment],
    );
    let fragment_tree = FragmentTree::new(root_fragment);

    assert_eq!(
      hit_test_dom(&dom, &box_tree, &fragment_tree, Point::new(10.0, 10.0)),
      None
    );
  }

  #[test]
  fn hit_test_dom_with_indices_matches_hit_test_dom_for_image_maps_and_inert_and_pointer_events() {
    // Image map case.
    let (mut dom, box_tree, fragment_tree, _, _, _, _) = image_map_fixture();
    let point = Point::new(65.0, 65.0);
    let expected = hit_test_dom(&dom, &box_tree, &fragment_tree, point);
    let actual = hit_test_with_prebuilt_indices(&mut dom, &box_tree, &fragment_tree, point);
    assert_eq!(actual, expected);
    assert!(
      expected.is_some(),
      "fixture should produce a hit for image maps"
    );

    // Pointer-events:none overlay case.
    let mut dom = doc(vec![elem(
      "div",
      vec![("id", "root")],
      vec![
        elem("a", vec![("href", "/ok")], vec![]),
        elem("div", vec![("id", "overlay")], vec![]),
      ],
    )]);
    let style = default_style();
    let mut overlay_style = ComputedStyle::default();
    overlay_style.pointer_events = PointerEvents::None;
    let overlay_style = Arc::new(overlay_style);

    let mut link_box = BoxNode::new_inline(style.clone(), vec![]);
    link_box.styled_node_id = Some(3);

    let mut overlay_box = BoxNode::new_block(overlay_style, FormattingContextType::Block, vec![]);
    overlay_box.styled_node_id = Some(4);

    let mut root_box = BoxNode::new_block(
      style,
      FormattingContextType::Block,
      vec![link_box, overlay_box],
    );
    root_box.styled_node_id = Some(2);

    let box_tree = BoxTree::new(root_box);
    let link_box_id = box_tree.root.children[0].id;
    let overlay_box_id = box_tree.root.children[1].id;

    let link_fragment = FragmentNode::new_block_with_id(
      Rect::from_xywh(0.0, 0.0, 100.0, 100.0),
      link_box_id,
      vec![],
    );
    let overlay_fragment = FragmentNode::new_block_with_id(
      Rect::from_xywh(0.0, 0.0, 100.0, 100.0),
      overlay_box_id,
      vec![],
    );
    let root_fragment = FragmentNode::new_block_with_id(
      Rect::from_xywh(0.0, 0.0, 100.0, 100.0),
      box_tree.root.id,
      vec![link_fragment, overlay_fragment],
    );
    let fragment_tree = FragmentTree::new(root_fragment);

    let point = Point::new(10.0, 10.0);
    let expected = hit_test_dom(&dom, &box_tree, &fragment_tree, point);
    let actual = hit_test_with_prebuilt_indices(&mut dom, &box_tree, &fragment_tree, point);
    assert_eq!(actual, expected);
    assert!(
      expected.is_some(),
      "fixture should produce a hit under pointer-events:none overlay"
    );

    // Inert subtree case.
    let mut dom = doc(vec![elem(
      "a",
      vec![("href", "/foo"), ("inert", "")],
      vec![elem("span", vec![], vec![text("txt")])],
    )]);
    let style = default_style();
    let anonymous = BoxNode::new_anonymous_inline(style.clone(), vec![]);
    let mut span = BoxNode::new_inline(style.clone(), vec![anonymous]);
    span.styled_node_id = Some(3);
    let mut a_box = BoxNode::new_inline(style, vec![span]);
    a_box.styled_node_id = Some(2);
    let box_tree = BoxTree::new(a_box);
    let anonymous_box_id = box_tree.root.children[0].children[0].id;
    let hit_fragment = FragmentNode::new_block_with_id(
      Rect::from_xywh(0.0, 0.0, 50.0, 50.0),
      anonymous_box_id,
      vec![],
    );
    let root_fragment = FragmentNode::new_block_with_id(
      Rect::from_xywh(0.0, 0.0, 100.0, 100.0),
      box_tree.root.id,
      vec![hit_fragment],
    );
    let fragment_tree = FragmentTree::new(root_fragment);
    let point = Point::new(10.0, 10.0);
    let expected = hit_test_dom(&dom, &box_tree, &fragment_tree, point);
    let actual = hit_test_with_prebuilt_indices(&mut dom, &box_tree, &fragment_tree, point);
    assert_eq!(actual, expected);
    assert_eq!(expected, None, "fixture should be blocked by inert semantics");
  }

  #[test]
  fn hit_test_dom_skips_inert_overlay_and_falls_through() {
    let dom = doc(vec![elem(
      "div",
      vec![("id", "root")],
      vec![
        elem("a", vec![("href", "/ok")], vec![]),
        elem("div", vec![("id", "overlay"), ("inert", "")], vec![]),
      ],
    )]);

    // DOM ids (pre-order):
    // 1 document
    // 2 div#root
    // 3 a[href]
    // 4 div#overlay[inert]
    //
    // Use default styles so the inert skip logic is exercised via DOM attribute resolution rather
    // than `ComputedStyle::inert`.
    let style = default_style();

    let mut link_box = BoxNode::new_inline(style.clone(), vec![]);
    link_box.styled_node_id = Some(3);

    let mut overlay_box = BoxNode::new_block(style.clone(), FormattingContextType::Block, vec![]);
    overlay_box.styled_node_id = Some(4);

    let mut root_box = BoxNode::new_block(
      style,
      FormattingContextType::Block,
      vec![link_box, overlay_box],
    );
    root_box.styled_node_id = Some(2);

    let box_tree = BoxTree::new(root_box);
    let link_box_id = box_tree.root.children[0].id;
    let overlay_box_id = box_tree.root.children[1].id;

    let link_fragment = FragmentNode::new_block_with_id(
      Rect::from_xywh(0.0, 0.0, 100.0, 100.0),
      link_box_id,
      vec![],
    );
    let overlay_fragment = FragmentNode::new_block_with_id(
      Rect::from_xywh(0.0, 0.0, 100.0, 100.0),
      overlay_box_id,
      vec![],
    );
    let root_fragment = FragmentNode::new_block_with_id(
      Rect::from_xywh(0.0, 0.0, 100.0, 100.0),
      box_tree.root.id,
      vec![link_fragment, overlay_fragment],
    );
    let fragment_tree = FragmentTree::new(root_fragment);

    let result = hit_test_dom(&dom, &box_tree, &fragment_tree, Point::new(10.0, 10.0))
      .expect("expected a hit result");
    assert_eq!(result.box_id, link_box_id);
    assert_eq!(result.dom_node_id, 3);
    assert_eq!(result.kind, HitTestKind::Link);
    assert_eq!(result.href.as_deref(), Some("/ok"));

    let all = hit_test_dom_all(&dom, &box_tree, &fragment_tree, Point::new(10.0, 10.0));
    assert!(!all.is_empty());
    assert_eq!(all[0].dom_node_id, 3);
    assert_eq!(all[0].kind, HitTestKind::Link);
    assert!(all.iter().all(|hit| hit.dom_node_id != 4));
  }

  #[test]
  fn resolve_label_associated_control_for_attribute() {
    let dom = doc(vec![
      elem("label", vec![("for", "x")], vec![text("Name")]),
      elem("input", vec![("id", "x"), ("type", "text")], vec![]),
    ]);

    // DOM ids:
    // 1 document
    // 2 label
    // 3 text
    // 4 input#x
    assert_eq!(resolve_label_associated_control(&dom, 2), Some(4));
  }

  #[test]
  fn resolve_label_associated_control_descendant_input() {
    let dom = doc(vec![elem(
      "label",
      vec![],
      vec![elem("input", vec![("type", "text")], vec![]), text("Name")],
    )]);

    // DOM ids:
    // 1 document
    // 2 label
    // 3 input
    assert_eq!(resolve_label_associated_control(&dom, 2), Some(3));
  }

  #[test]
  fn resolve_label_associated_control_does_not_cross_shadow_root_boundary() {
    let dom = doc(vec![
      elem("input", vec![("id", "x"), ("type", "text")], vec![]),
      elem(
        "div",
        vec![("id", "host")],
        vec![DomNode {
          node_type: DomNodeType::ShadowRoot {
            mode: ShadowRootMode::Open,
            delegates_focus: false,
          },
          children: vec![elem("label", vec![("for", "x")], vec![text("Label")])],
        }],
      ),
    ]);

    let index = DomIndex::new(&dom);
    let label = &dom.children[1].children[0].children[0] as *const DomNode;
    let label_id = index.id_for_ptr(label).expect("label id");

    assert_eq!(
      resolve_label_associated_control(&dom, label_id),
      None,
      "label `for` associations must not cross the shadow root boundary"
    );
  }
}
