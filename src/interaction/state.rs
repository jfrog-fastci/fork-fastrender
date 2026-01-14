use crate::dom2::{NodeId, RendererDomMapping};
use crate::interaction::selection_serialize::{
  cmp_point_dom2, DocumentSelectionPoint, DocumentSelectionPointDom2, DocumentSelectionRange,
  DocumentSelectionRangeDom2,
};
use crate::text::caret::CaretAffinity;
use rustc_hash::{FxHashMap, FxHashSet};
use std::cmp::Ordering;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering as AtomicOrdering};

/// Live (non-DOM) form control state.
///
/// This is used to reflect user-driven changes to form controls without mutating DOM attributes.
/// Downstream systems (paint, accessibility, validation) can consult this store to surface the
/// current control state.
#[derive(Debug, Clone, Default)]
pub struct FormState {
  /// Current value for value-bearing controls (`<input>` / `<textarea>` / etc.), keyed by DOM
  /// pre-order node id.
  pub values: FxHashMap<usize, String>,
  /// Current checked state for checkbox/radio inputs, keyed by DOM pre-order node id.
  pub checked: FxHashMap<usize, bool>,
  /// Current file selections for `<input type="file">`, keyed by DOM pre-order node id.
  pub file_inputs: FxHashMap<usize, Vec<FileSelection>>,
  /// Current selected option ids for `<select>` elements, keyed by the select element's DOM pre-order
  /// node id.
  ///
  /// When a select id is present in this map, the selection set is treated as authoritative for that
  /// select (including the empty set for multi-selects).
  pub select_selected: FxHashMap<usize, FxHashSet<usize>>,
}

/// A selected file for an `<input type="file">` control.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileSelection {
  pub path: PathBuf,
  pub filename: String,
  pub content_type: String,
  pub bytes: Vec<u8>,
}

impl FormState {
  #[inline]
  pub fn has_overrides(&self) -> bool {
    !(self.values.is_empty()
      && self.checked.is_empty()
      && self.file_inputs.is_empty()
      && self.select_selected.is_empty())
  }

  #[inline]
  pub fn value_for(&self, node_id: usize) -> Option<&str> {
    self.values.get(&node_id).map(|s| s.as_str())
  }

  #[inline]
  pub fn checked_for(&self, node_id: usize) -> Option<bool> {
    self.checked.get(&node_id).copied()
  }

  /// Returns the browser-like "value string" for an `<input type="file">` override.
  ///
  /// Browsers ignore author-provided markup `value=` for file inputs and instead expose a synthetic
  /// string based on the selected filename, prefixed with `C:\fakepath\`.
  ///
  /// FastRender mirrors this behavior:
  /// - selected file bytes live in [`FormState::file_inputs`], and
  /// - downstream consumers (accessibility, validation) operate on a derived "value string" that
  ///   reflects only the *first* selected filename (even when `multiple` is enabled).
  ///
  /// Returns `None` when no file-input override is present for `node_id`.
  #[inline]
  pub fn file_input_value_string(&self, node_id: usize) -> Option<String> {
    self.file_inputs.get(&node_id).map(|files| {
      files
        .first()
        .map(|file| format!("C:\\fakepath\\{}", file.filename))
        .unwrap_or_default()
    })
  }

  #[inline]
  pub fn files_for(&self, node_id: usize) -> Option<&Vec<FileSelection>> {
    self.file_inputs.get(&node_id)
  }

  #[inline]
  pub fn select_selected_options(&self, select_node_id: usize) -> Option<&FxHashSet<usize>> {
    self.select_selected.get(&select_node_id)
  }
}

/// Document (non-form-control) selection state.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum DocumentSelectionState {
  /// The entire rendered document (excluding non-selectable/hidden content).
  All,
  /// One or more explicit selection ranges.
  Ranges(DocumentSelectionRanges),
}

impl DocumentSelectionState {
  /// Returns true when this selection contains at least one non-collapsed range.
  pub fn has_highlight(&self) -> bool {
    match self {
      Self::All => true,
      Self::Ranges(ranges) => ranges.has_highlight(),
    }
  }
}

/// Document selection state backed by `dom2::NodeId` endpoints.
///
/// `dom2::NodeId` values are stable across DOM mutations, but their numeric indices do **not**
/// reflect DOM tree order.
///
/// Any logic that compares endpoints must use the current DOM tree order (e.g.
/// [`crate::dom2::cmp_dom2_nodes`]) rather than `NodeId::index()`. When projecting the selection
/// into renderer preorder space, use [`RendererDomMapping`].
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum DocumentSelectionStateDom2 {
  /// The entire rendered document (excluding non-selectable/hidden content).
  All,
  /// One or more explicit selection ranges.
  Ranges(DocumentSelectionRangesDom2),
}

impl DocumentSelectionStateDom2 {
  /// Returns true when this selection contains at least one non-collapsed range.
  pub fn has_highlight(&self) -> bool {
    match self {
      Self::All => true,
      Self::Ranges(ranges) => ranges.has_highlight(),
    }
  }

  /// Project this `dom2` selection into renderer preorder space (legacy selection representation).
  ///
  /// This keeps selection endpoints stable across DOM mutations while still allowing downstream
  /// systems (layout/paint/fragment highlighting) to operate on preorder ids.
  pub fn project_to_preorder(&self, mapping: &RendererDomMapping) -> DocumentSelectionState {
    match self {
      DocumentSelectionStateDom2::All => DocumentSelectionState::All,
      DocumentSelectionStateDom2::Ranges(ranges) => {
        let mut projected_ranges: Vec<DocumentSelectionRange> = ranges
          .ranges
          .iter()
          .copied()
          .filter_map(|r| r.project_to_preorder(mapping))
          .collect();

        let mut anchor = ranges.anchor.project_to_preorder(mapping);
        let mut focus = ranges.focus.project_to_preorder(mapping);

        if anchor.is_none() || focus.is_none() {
          // Fallback to the first projected range when the anchor/focus endpoints are no longer
          // mappable (e.g. detached nodes).
          if let Some(first) = projected_ranges.first().copied() {
            anchor = Some(first.start);
            focus = Some(first.end);
          }
        }

        let mut projected = DocumentSelectionRanges {
          ranges: std::mem::take(&mut projected_ranges),
          primary: ranges.primary,
          anchor: anchor.unwrap_or(DocumentSelectionPoint {
            node_id: 0,
            char_offset: 0,
          }),
          focus: focus.unwrap_or(DocumentSelectionPoint {
            node_id: 0,
            char_offset: 0,
          }),
        };
        projected.normalize();
        DocumentSelectionState::Ranges(projected)
      }
    }
  }

  /// Convert a renderer-preorder selection state into a stable `dom2` selection state.
  ///
  /// This is useful when callers initially create selections from renderer hit-testing/layout data
  /// (which is keyed by preorder ids) but want to store selection endpoints robustly across DOM
  /// mutations using `dom2::NodeId`.
  pub fn from_preorder(
    selection: &DocumentSelectionState,
    dom: &crate::dom2::Document,
    mapping: &RendererDomMapping,
  ) -> Option<Self> {
    match selection {
      DocumentSelectionState::All => Some(DocumentSelectionStateDom2::All),
      DocumentSelectionState::Ranges(ranges) => {
        let mut dom2 = DocumentSelectionRangesDom2::from_preorder(ranges, mapping)?;
        dom2.normalize(dom);
        Some(DocumentSelectionStateDom2::Ranges(dom2))
      }
    }
  }

  /// Drop any selection endpoints that are no longer reachable from the document root for the
  /// current renderer snapshot.
  ///
  /// Returns `false` when the selection no longer contains any mappable ranges and should be
  /// cleared by the caller.
  pub fn prune_detached(&mut self, mapping: &RendererDomMapping) -> bool {
    let is_connected = |id: NodeId| mapping.preorder_for_node_id(id).is_some();
    match self {
      DocumentSelectionStateDom2::All => true,
      DocumentSelectionStateDom2::Ranges(ranges) => {
        ranges
          .ranges
          .retain(|range| is_connected(range.start.node_id) && is_connected(range.end.node_id));
        if ranges.ranges.is_empty() {
          return false;
        }

        if !is_connected(ranges.anchor.node_id) || !is_connected(ranges.focus.node_id) {
          // Keep anchor/focus consistent with remaining ranges when the endpoints are detached.
          let first = ranges.ranges[0];
          ranges.anchor = first.start;
          ranges.focus = first.end;
        }

        // Repair primary index based on the current anchor/focus span. This uses the preorder
        // mapping because we do not have access to the live `dom2::Document` here.
        let primary_span = DocumentSelectionRangeDom2 {
          start: ranges.anchor,
          end: ranges.focus,
        }
        .normalized(mapping);

        if let Some(idx) = ranges
          .ranges
          .iter()
          .copied()
          .position(|r| {
            let r = r.normalized(mapping);
            cmp_point_dom2(r.start, primary_span.start, mapping) != Ordering::Greater
              && cmp_point_dom2(r.end, primary_span.end, mapping) != Ordering::Less
          })
        {
          ranges.primary = idx;
        } else {
          // Fallback: clamp primary into bounds and update anchor/focus to match.
          ranges.primary = ranges.primary.min(ranges.ranges.len().saturating_sub(1));
          let primary = ranges.ranges[ranges.primary];
          ranges.anchor = primary.start;
          ranges.focus = primary.end;
        }
        true
      }
    }
  }
}

/// A multi-range document selection.
///
/// Ranges are expected to be:
/// - normalized (`start <= end`),
/// - ordered by DOM position, and
/// - non-overlapping (adjacent/overlapping ranges are merged).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct DocumentSelectionRanges {
  pub ranges: Vec<DocumentSelectionRange>,
  /// Index into `ranges` representing the primary range for caret/extension semantics.
  pub primary: usize,
  /// Fixed anchor point for extending the primary range.
  pub anchor: DocumentSelectionPoint,
  /// Moving focus point for extending the primary range.
  pub focus: DocumentSelectionPoint,
}

impl DocumentSelectionRanges {
  pub fn collapsed(point: DocumentSelectionPoint) -> Self {
    Self {
      ranges: vec![DocumentSelectionRange {
        start: point,
        end: point,
      }],
      primary: 0,
      anchor: point,
      focus: point,
    }
  }

  pub fn has_highlight(&self) -> bool {
    self.ranges.iter().any(|r| r.start != r.end)
  }

  fn cmp_point(a: DocumentSelectionPoint, b: DocumentSelectionPoint) -> Ordering {
    a.node_id
      .cmp(&b.node_id)
      .then_with(|| a.char_offset.cmp(&b.char_offset))
  }

  fn range_contains_range(outer: &DocumentSelectionRange, inner: &DocumentSelectionRange) -> bool {
    Self::cmp_point(outer.start, inner.start) != Ordering::Greater
      && Self::cmp_point(outer.end, inner.end) != Ordering::Less
  }

  /// Ensure `ranges` are normalized, sorted, and non-overlapping (merging overlap/adjacency).
  ///
  /// Also repairs `primary` to point at the range containing the current anchor/focus span.
  pub fn normalize(&mut self) {
    if self.ranges.is_empty() {
      self.primary = 0;
      return;
    }

    for range in &mut self.ranges {
      *range = range.normalized();
    }

    self.ranges.sort_by(|a, b| {
      Self::cmp_point(a.start, b.start).then_with(|| Self::cmp_point(a.end, b.end))
    });

    let mut merged: Vec<DocumentSelectionRange> = Vec::with_capacity(self.ranges.len());
    for range in self.ranges.drain(..) {
      if let Some(last) = merged.last_mut() {
        // Merge when overlapping or adjacent.
        if Self::cmp_point(range.start, last.end) != Ordering::Greater {
          if Self::cmp_point(range.end, last.end) == Ordering::Greater {
            last.end = range.end;
          }
          continue;
        }
      }
      merged.push(range);
    }
    self.ranges = merged;

    // Repair primary index based on the current anchor/focus span.
    let primary_span = DocumentSelectionRange {
      start: self.anchor,
      end: self.focus,
    }
    .normalized();
    if let Some(idx) = self
      .ranges
      .iter()
      .position(|r| Self::range_contains_range(r, &primary_span))
    {
      self.primary = idx;
    } else {
      // Fallback: clamp primary into bounds and update anchor/focus to match.
      self.primary = self.primary.min(self.ranges.len().saturating_sub(1));
      let primary = self.ranges[self.primary];
      self.anchor = primary.start;
      self.focus = primary.end;
    }
  }
}

/// A `dom2` multi-range document selection.
///
/// Ranges are expected to be:
/// - normalized (`start <= end` in DOM order),
/// - ordered by DOM position, and
/// - non-overlapping (adjacent/overlapping ranges are merged).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct DocumentSelectionRangesDom2 {
  pub ranges: Vec<DocumentSelectionRangeDom2>,
  /// Index into `ranges` representing the primary range for caret/extension semantics.
  pub primary: usize,
  /// Fixed anchor point for extending the primary range.
  pub anchor: DocumentSelectionPointDom2,
  /// Moving focus point for extending the primary range.
  pub focus: DocumentSelectionPointDom2,
}

impl DocumentSelectionRangesDom2 {
  pub fn collapsed(point: DocumentSelectionPointDom2) -> Self {
    Self {
      ranges: vec![DocumentSelectionRangeDom2 {
        start: point,
        end: point,
      }],
      primary: 0,
      anchor: point,
      focus: point,
    }
  }

  pub fn has_highlight(&self) -> bool {
    self.ranges.iter().any(|r| r.start != r.end)
  }

  /// Convert a renderer-preorder multi-range selection into a stable `dom2` selection.
  ///
  /// Returns `None` if any of the required endpoints are not mappable in `mapping`.
  pub fn from_preorder(
    ranges: &DocumentSelectionRanges,
    mapping: &RendererDomMapping,
  ) -> Option<Self> {
    let mut ranges_dom2: Vec<DocumentSelectionRangeDom2> = Vec::with_capacity(ranges.ranges.len());
    for &range in &ranges.ranges {
      ranges_dom2.push(DocumentSelectionRangeDom2::from_preorder(range, mapping)?);
    }

    let anchor = DocumentSelectionPointDom2::from_preorder(ranges.anchor, mapping)?;
    let focus = DocumentSelectionPointDom2::from_preorder(ranges.focus, mapping)?;

    Some(Self {
      ranges: ranges_dom2,
      primary: ranges.primary,
      anchor,
      focus,
    })
  }

  fn cmp_point(
    dom: &crate::dom2::Document,
    a: DocumentSelectionPointDom2,
    b: DocumentSelectionPointDom2,
  ) -> Ordering {
    let node_cmp = crate::dom2::cmp_dom2_nodes(dom, a.node_id, b.node_id);
    if node_cmp != Ordering::Equal {
      return node_cmp;
    }
    // If nodes are unordered (e.g. detached or cross-shadow), treat the points as unordered too.
    if a.node_id != b.node_id {
      return Ordering::Equal;
    }
    a.char_offset.cmp(&b.char_offset)
  }

  fn normalize_range(
    dom: &crate::dom2::Document,
    mut range: DocumentSelectionRangeDom2,
  ) -> Option<DocumentSelectionRangeDom2> {
    // Drop detached/out-of-bounds nodes.
    if !dom.is_connected(range.start.node_id) || !dom.is_connected(range.end.node_id) {
      return None;
    }

    // Range ordering/merging is only defined within a single Range tree root (Document or
    // ShadowRoot). Cross-root selections are currently dropped.
    if dom.tree_root_for_range(range.start.node_id) != dom.tree_root_for_range(range.end.node_id) {
      return None;
    }

    match Self::cmp_point(dom, range.start, range.end) {
      Ordering::Greater => {
        std::mem::swap(&mut range.start, &mut range.end);
      }
      Ordering::Equal if range.start.node_id != range.end.node_id => {
        // Unordered endpoints (detached/cross-shadow) should be pruned rather than using a bogus
        // ordering.
        return None;
      }
      _ => {}
    }

    Some(range)
  }

  fn range_contains_range(
    dom: &crate::dom2::Document,
    outer: &DocumentSelectionRangeDom2,
    inner: &DocumentSelectionRangeDom2,
  ) -> bool {
    Self::cmp_point(dom, outer.start, inner.start) != Ordering::Greater
      && Self::cmp_point(dom, outer.end, inner.end) != Ordering::Less
  }

  /// Ensure `ranges` are normalized, sorted, and non-overlapping (merging overlap/adjacency).
  ///
  /// This mirrors `DocumentSelectionRanges::normalize`, but uses DOM tree order rather than
  /// `NodeId::index()` ordering.
  pub fn normalize(&mut self, dom: &crate::dom2::Document) {
    if self.ranges.is_empty() {
      self.primary = 0;
      return;
    }

    let mut normalized: Vec<DocumentSelectionRangeDom2> = Vec::with_capacity(self.ranges.len());
    for range in self.ranges.drain(..) {
      if let Some(range) = Self::normalize_range(dom, range) {
        normalized.push(range);
      }
    }
    self.ranges = normalized;

    if self.ranges.is_empty() {
      self.primary = 0;
      return;
    }

    // `cmp_dom2_nodes` yields `Ordering::Equal` for nodes in different Range tree roots (Document
    // vs ShadowRoot). That is intentional so callers can prune cross-boundary selections, but it
    // also means we must ensure the selection only contains ranges from a single comparable root
    // before we sort/merge.
    let primary_span = Self::normalize_range(
      dom,
      DocumentSelectionRangeDom2 {
        start: self.anchor,
        end: self.focus,
      },
    );

    let fallback_root = dom.tree_root_for_range(self.ranges[0].start.node_id);
    let selection_root = primary_span
      .as_ref()
      .map(|span| dom.tree_root_for_range(span.start.node_id))
      .filter(|&root| {
        // Only trust the primary span root if at least one actual range shares it; otherwise fall
        // back to the first normalized range root to avoid dropping the entire selection due to an
        // inconsistent anchor/focus.
        self
          .ranges
          .iter()
          .any(|r| dom.tree_root_for_range(r.start.node_id) == root)
      })
      .unwrap_or(fallback_root);
    self
      .ranges
      .retain(|r| dom.tree_root_for_range(r.start.node_id) == selection_root);
    if self.ranges.is_empty() {
      self.primary = 0;
      return;
    }

    self.ranges.sort_by(|a, b| {
      Self::cmp_point(dom, a.start, b.start).then_with(|| Self::cmp_point(dom, a.end, b.end))
    });

    let mut merged: Vec<DocumentSelectionRangeDom2> = Vec::with_capacity(self.ranges.len());
    for range in self.ranges.drain(..) {
      if let Some(last) = merged.last_mut() {
        // Merge when overlapping or adjacent.
        if Self::cmp_point(dom, range.start, last.end) != Ordering::Greater {
          if Self::cmp_point(dom, range.end, last.end) == Ordering::Greater {
            last.end = range.end;
          }
          continue;
        }
      }
      merged.push(range);
    }
    self.ranges = merged;

    // Repair primary index based on the current anchor/focus span.
    let primary_span = primary_span
      .filter(|span| dom.tree_root_for_range(span.start.node_id) == selection_root);

    if let Some(primary_span) = primary_span {
      if let Some(idx) = self
        .ranges
        .iter()
        .position(|r| Self::range_contains_range(dom, r, &primary_span))
      {
        self.primary = idx;
        return;
      }
    }

    // Fallback: clamp primary into bounds and update anchor/focus to match.
    self.primary = self.primary.min(self.ranges.len().saturating_sub(1));
    let primary = self.ranges[self.primary];
    self.anchor = primary.start;
    self.focus = primary.end;
  }
}

/// Returns true when `point` lies within any *non-collapsed* range in the selection.
///
/// This mirrors the legacy `document_selection_contains_point` helper in `interaction::engine`, but
/// operates on `dom2::NodeId` endpoints and uses the current preorder mapping for ordering.
pub(crate) fn document_selection_contains_point_dom2(
  selection: &DocumentSelectionStateDom2,
  point: DocumentSelectionPointDom2,
  mapping: &RendererDomMapping,
) -> bool {
  // If the point is not present in the current renderer snapshot, it cannot be part of the painted
  // document selection highlight.
  if mapping.preorder_for_node_id(point.node_id).is_none() {
    return false;
  }
  match selection {
    DocumentSelectionStateDom2::All => true,
    DocumentSelectionStateDom2::Ranges(ranges) => ranges.ranges.iter().any(|range| {
      // Collapsed ranges represent a caret without any selected text; starting a drag-drop from such
      // a point would be surprising when other ranges in the selection are highlighted.
      if range.start == range.end {
        return false;
      }
      // Ignore ranges whose endpoints are not present in the current snapshot. Without a preorder
      // mapping we cannot derive a meaningful DOM order (and must not fall back to `NodeId::index()`).
      if mapping.preorder_for_node_id(range.start.node_id).is_none()
        || mapping.preorder_for_node_id(range.end.node_id).is_none()
      {
        return false;
      }
      let range = range.normalized(mapping);
      // Allow starting a drag at either boundary. This is more forgiving than the half-open
      // selection model and better matches typical "click anywhere on the highlight" UX.
      cmp_point_dom2(range.start, point, mapping) != Ordering::Greater
        && cmp_point_dom2(point, range.end, mapping) != Ordering::Greater
    }),
  }
}

/// Internal, non-DOM-visible interaction state for a single document/tab.
///
/// This replaces the legacy `data-fastr-*` DOM attribute mutations that were previously used to
/// represent dynamic user interaction state (hover/active/focus/visited/user validity/IME preedit).
/// Keeping this state out of the DOM avoids observable author CSS/DOM side effects and reduces DOM
/// churn.
///
/// ## Cached interaction hashes
///
/// `InteractionState` caches two digests used by render invalidation:
/// - [`interaction_css_hash`](Self::interaction_css_hash): affects selector matching / cascade.
/// - [`interaction_paint_hash`](Self::interaction_paint_hash): paint-only state (caret/selection/IME,
///   file-input labels, etc).
///
/// When mutating public fields directly (i.e. outside [`InteractionEngine`](crate::interaction::InteractionEngine)),
/// callers must mark the appropriate digest dirty via [`mark_css_hash_dirty`](Self::mark_css_hash_dirty)
/// and/or [`mark_paint_hash_dirty`](Self::mark_paint_hash_dirty).
///
/// Note: `visited_links` and `user_validity` are kept private; use
/// [`visited_links_mut`](Self::visited_links_mut) / [`user_validity_mut`](Self::user_validity_mut)
/// (or the `insert_*` helpers) to mutate them, which automatically dirties the CSS hash.
/// For paint-only state (`document_selection`, `text_edit`, `ime_preedit`, and `form_state`), prefer
/// the helper setters/mutable accessors on [`InteractionState`] so the cached paint hash is dirtied
/// automatically.
#[derive(Debug)]
pub struct InteractionState {
  /// Currently focused element node id (pre-order id from `crate::dom::enumerate_dom_ids`).
  pub focused: Option<usize>,
  /// Whether the focused element should match `:focus-visible`.
  pub focus_visible: bool,
  /// Current fullscreen element node id (pre-order id from `crate::dom::enumerate_dom_ids`).
  ///
  /// When set, the element matches `:fullscreen` (and vendor aliases like `:-webkit-full-screen`)
  /// and is promoted to the top layer so `::backdrop` can be generated and it paints above normal
  /// document content.
  pub fullscreen_element: Option<usize>,
  /// The focused element and its element ancestors (used for `:focus-within` matching).
  focus_chain: Vec<usize>,
  focus_chain_membership: FxHashSet<usize>,

  /// The pre-order DOM id of the `<select>` element whose dropdown popup is currently open, if any.
  ///
  /// This is used to represent the open/closed state of native dropdown selects (single-select
  /// `<select>` controls with `multiple` absent and `size == 1`) whose popup UI is owned by the
  /// front-end.
  pub open_select_dropdown: Option<usize>,

  /// The element under the pointer and its element ancestors (used for `:hover` matching).
  hover_chain: Vec<usize>,
  hover_chain_membership: FxHashSet<usize>,
  /// The active element (e.g. pointer down) and its element ancestors (used for `:active` matching).
  active_chain: Vec<usize>,
  active_chain_membership: FxHashSet<usize>,

  /// Set of link node ids that have been visited in this document.
  ///
  /// Note: This is currently per-document (cleared on navigation), matching the legacy behaviour
  /// where visited state was stored on the DOM element itself.
  visited_links: FxHashSet<usize>,

  /// Optional IME composition (preedit) state for the focused text control.
  pub ime_preedit: Option<ImePreeditState>,

  /// Optional caret/selection state for a focused text control (`<input>` / `<textarea>`).
  ///
  /// This is internal UI state used for form-control painting. It must never be mirrored onto the
  /// DOM (e.g. via `data-*` attributes), because that would make selection/caret state observable to
  /// author CSS/DOM.
  pub text_edit: Option<TextEditPaintState>,

  /// Live form state for value-bearing and toggleable controls.
  form_state: FormState,

  /// Current document (non-form-control) selection.
  pub document_selection: Option<DocumentSelectionState>,

  /// Node ids (controls/forms) that have flipped HTML "user validity" from false to true.
  ///
  /// This gates `:user-valid` / `:user-invalid` pseudo-classes.
  user_validity: FxHashSet<usize>,

  /// Cached hash of interaction state that can affect CSS selector matching.
  ///
  /// This is derived from fields such as focus/hover/active state, visited links, and user validity.
  /// The value is recomputed lazily when [`css_hash_dirty`](Self::css_hash_dirty) is set.
  cached_css_hash: AtomicU64,
  /// Cached hash of interaction state that affects paint-only output (caret/selection/IME, etc).
  ///
  /// The value is recomputed lazily when [`paint_hash_dirty`](Self::paint_hash_dirty) is set.
  cached_paint_hash: AtomicU64,
  /// Whether [`cached_css_hash`](Self::cached_css_hash) needs recomputation.
  css_hash_dirty: AtomicBool,
  /// Whether [`cached_paint_hash`](Self::cached_paint_hash) needs recomputation.
  paint_hash_dirty: AtomicBool,
}

impl InteractionState {
  /// Set (or clear) the currently focused element, marking the cached CSS hash dirty on change.
  pub fn set_focused(&mut self, focused: Option<usize>) {
    if self.focused == focused {
      return;
    }
    self.focused = focused;
    self.mark_css_hash_dirty();
  }

  /// Mutably access the currently focused element, marking the cached CSS hash dirty.
  pub fn focused_mut(&mut self) -> &mut Option<usize> {
    self.mark_css_hash_dirty();
    &mut self.focused
  }

  /// Set whether the focused element should match `:focus-visible`, marking the cached CSS hash
  /// dirty on change.
  pub fn set_focus_visible(&mut self, focus_visible: bool) {
    if self.focus_visible == focus_visible {
      return;
    }
    self.focus_visible = focus_visible;
    self.mark_css_hash_dirty();
  }

  /// Mutably access the `:focus-visible` flag, marking the cached CSS hash dirty.
  pub fn focus_visible_mut(&mut self) -> &mut bool {
    self.mark_css_hash_dirty();
    &mut self.focus_visible
  }

  /// Set (or clear) the currently fullscreen element, marking the cached CSS hash dirty on change.
  pub fn set_fullscreen_element(&mut self, fullscreen_element: Option<usize>) {
    if self.fullscreen_element == fullscreen_element {
      return;
    }
    self.fullscreen_element = fullscreen_element;
    self.mark_css_hash_dirty();
  }

  /// Returns the currently fullscreen element node id (preorder), if any.
  #[inline]
  pub fn fullscreen_element(&self) -> Option<usize> {
    self.fullscreen_element
  }

  /// Mutably access the fullscreen element id, marking the cached CSS hash dirty.
  pub fn fullscreen_element_mut(&mut self) -> &mut Option<usize> {
    self.mark_css_hash_dirty();
    &mut self.fullscreen_element
  }

  #[inline]
  pub fn focus_chain(&self) -> &[usize] {
    &self.focus_chain
  }

  #[inline]
  pub fn hover_chain(&self) -> &[usize] {
    &self.hover_chain
  }

  #[inline]
  pub fn active_chain(&self) -> &[usize] {
    &self.active_chain
  }

  pub fn set_focus_chain(&mut self, chain: Vec<usize>) {
    if self.focus_chain == chain {
      return;
    }
    self.focus_chain_membership.clear();
    self.focus_chain_membership.reserve(chain.len());
    for &id in &chain {
      self.focus_chain_membership.insert(id);
    }
    self.focus_chain = chain;
    self.mark_css_hash_dirty();
  }

  pub fn set_hover_chain(&mut self, chain: Vec<usize>) {
    if self.hover_chain == chain {
      return;
    }
    self.hover_chain_membership.clear();
    self.hover_chain_membership.reserve(chain.len());
    for &id in &chain {
      self.hover_chain_membership.insert(id);
    }
    self.hover_chain = chain;
    self.mark_css_hash_dirty();
  }

  pub fn set_active_chain(&mut self, chain: Vec<usize>) {
    if self.active_chain == chain {
      return;
    }
    self.active_chain_membership.clear();
    self.active_chain_membership.reserve(chain.len());
    for &id in &chain {
      self.active_chain_membership.insert(id);
    }
    self.active_chain = chain;
    self.mark_css_hash_dirty();
  }

  pub fn clear_focus_chain(&mut self) {
    if self.focus_chain.is_empty() {
      return;
    }
    self.focus_chain.clear();
    self.focus_chain_membership.clear();
    self.mark_css_hash_dirty();
  }

  pub fn clear_hover_chain(&mut self) {
    if self.hover_chain.is_empty() {
      return;
    }
    self.hover_chain.clear();
    self.hover_chain_membership.clear();
    self.mark_css_hash_dirty();
  }

  pub fn clear_active_chain(&mut self) {
    if self.active_chain.is_empty() {
      return;
    }
    self.active_chain.clear();
    self.active_chain_membership.clear();
    self.mark_css_hash_dirty();
  }

  pub(crate) fn mutate_focus_chain(&mut self, f: impl FnOnce(&mut Vec<usize>)) {
    f(&mut self.focus_chain);
    self.focus_chain_membership.clear();
    self.focus_chain_membership.reserve(self.focus_chain.len());
    for &id in &self.focus_chain {
      self.focus_chain_membership.insert(id);
    }
    self.mark_css_hash_dirty();
  }

  pub(crate) fn mutate_hover_chain(&mut self, f: impl FnOnce(&mut Vec<usize>)) {
    f(&mut self.hover_chain);
    self.hover_chain_membership.clear();
    self.hover_chain_membership.reserve(self.hover_chain.len());
    for &id in &self.hover_chain {
      self.hover_chain_membership.insert(id);
    }
    self.mark_css_hash_dirty();
  }

  pub(crate) fn mutate_active_chain(&mut self, f: impl FnOnce(&mut Vec<usize>)) {
    f(&mut self.active_chain);
    self.active_chain_membership.clear();
    self
      .active_chain_membership
      .reserve(self.active_chain.len());
    for &id in &self.active_chain {
      self.active_chain_membership.insert(id);
    }
    self.mark_css_hash_dirty();
  }

  #[cfg(any(debug_assertions, test))]
  pub(crate) fn debug_assert_chain_caches_consistent(&self) {
    debug_assert_eq!(
      self.focus_chain.len(),
      self.focus_chain_membership.len(),
      "focus_chain cache out of sync"
    );
    for &id in &self.focus_chain {
      debug_assert!(
        self.focus_chain_membership.contains(&id),
        "focus_chain missing cached membership for {id}"
      );
    }

    debug_assert_eq!(
      self.hover_chain.len(),
      self.hover_chain_membership.len(),
      "hover_chain cache out of sync"
    );
    for &id in &self.hover_chain {
      debug_assert!(
        self.hover_chain_membership.contains(&id),
        "hover_chain missing cached membership for {id}"
      );
    }

    debug_assert_eq!(
      self.active_chain.len(),
      self.active_chain_membership.len(),
      "active_chain cache out of sync"
    );
    for &id in &self.active_chain {
      debug_assert!(
        self.active_chain_membership.contains(&id),
        "active_chain missing cached membership for {id}"
      );
    }
  }

  #[inline]
  pub fn is_focused(&self, node_id: usize) -> bool {
    self.focused == Some(node_id)
  }

  #[inline]
  pub fn is_focus_within(&self, node_id: usize) -> bool {
    self.focus_chain_membership.contains(&node_id)
  }

  #[inline]
  pub fn is_hovered(&self, node_id: usize) -> bool {
    self.hover_chain_membership.contains(&node_id)
  }

  #[inline]
  pub fn is_active(&self, node_id: usize) -> bool {
    self.active_chain_membership.contains(&node_id)
  }

  #[inline]
  pub fn is_fullscreen(&self, node_id: usize) -> bool {
    self.fullscreen_element == Some(node_id)
  }

  #[inline]
  pub fn is_visited_link(&self, node_id: usize) -> bool {
    self.visited_links.contains(&node_id)
  }

  #[inline]
  pub fn visited_links(&self) -> &FxHashSet<usize> {
    &self.visited_links
  }

  /// Mutably access the visited-links set.
  ///
  /// This automatically marks the cached CSS interaction hash dirty so render caching observes any
  /// modifications.
  #[inline]
  pub fn visited_links_mut(&mut self) -> &mut FxHashSet<usize> {
    self.mark_css_hash_dirty();
    &mut self.visited_links
  }

  #[inline]
  pub fn insert_visited_link(&mut self, node_id: usize) -> bool {
    let changed = self.visited_links.insert(node_id);
    if changed {
      self.mark_css_hash_dirty();
    }
    changed
  }

  #[inline]
  pub fn ime_preedit_for(&self, node_id: usize) -> Option<&str> {
    self
      .ime_preedit
      .as_ref()
      .filter(|state| state.node_id == node_id)
      .map(|state| state.text.as_str())
  }

  #[inline]
  pub fn ime_preedit_state_for(&self, node_id: usize) -> Option<&ImePreeditState> {
    self
      .ime_preedit
      .as_ref()
      .filter(|state| state.node_id == node_id)
  }

  #[inline]
  pub fn text_edit_for(&self, node_id: usize) -> Option<&TextEditPaintState> {
    self
      .text_edit
      .as_ref()
      .filter(|state| state.node_id == node_id)
  }

  /// Set (or clear) the document selection, marking the cached paint hash dirty on change.
  pub fn set_document_selection(&mut self, selection: Option<DocumentSelectionState>) {
    if self.document_selection == selection {
      return;
    }
    self.document_selection = selection;
    self.mark_paint_hash_dirty();
  }

  /// Mutate the existing document selection in-place, marking the cached paint hash dirty when the
  /// selection changes.
  ///
  /// This is intended for callers that want to update selection ranges without allocating a fresh
  /// [`DocumentSelectionState`]. The closure is only invoked when a selection is present.
  ///
  /// Returns `true` when the selection was present and changed.
  pub fn mutate_document_selection(
    &mut self,
    f: impl FnOnce(&mut DocumentSelectionState),
  ) -> bool {
    let Some(selection) = self.document_selection.as_mut() else {
      return false;
    };
    let before = selection.clone();
    f(selection);
    if *selection == before {
      return false;
    }
    self.mark_paint_hash_dirty();
    true
  }

  /// Mutably access the document selection, marking the cached paint hash dirty.
  pub fn document_selection_mut(&mut self) -> &mut Option<DocumentSelectionState> {
    self.mark_paint_hash_dirty();
    &mut self.document_selection
  }

  /// Set (or clear) the focused-control text-edit paint state, marking the cached paint hash dirty
  /// on change.
  pub fn set_text_edit(&mut self, edit: Option<TextEditPaintState>) {
    if self.text_edit == edit {
      return;
    }
    self.text_edit = edit;
    self.mark_paint_hash_dirty();
  }

  /// Mutably access the focused-control text-edit paint state, marking the cached paint hash dirty.
  pub fn text_edit_mut(&mut self) -> &mut Option<TextEditPaintState> {
    self.mark_paint_hash_dirty();
    &mut self.text_edit
  }

  /// Set (or clear) the IME preedit state, marking the cached paint hash dirty on change.
  pub fn set_ime_preedit(&mut self, preedit: Option<ImePreeditState>) {
    if self.ime_preedit == preedit {
      return;
    }
    self.ime_preedit = preedit;
    self.mark_paint_hash_dirty();
  }

  /// Update the IME preedit (composition) state for `node_id`, marking the cached paint hash dirty
  /// when the state changes.
  ///
  /// This preserves the existing allocation for the preedit string when possible (by mutating the
  /// stored `String` in place).
  pub fn update_ime_preedit(
    &mut self,
    node_id: usize,
    text: &str,
    cursor: Option<(usize, usize)>,
  ) -> bool {
    let mut changed = false;
    match self.ime_preedit.as_mut() {
      Some(existing) if existing.node_id == node_id => {
        if existing.text != text || existing.cursor != cursor {
          existing.text.clear();
          existing.text.push_str(text);
          existing.cursor = cursor;
          changed = true;
        }
      }
      _ => {
        self.ime_preedit = Some(ImePreeditState {
          node_id,
          text: text.to_string(),
          cursor,
        });
        changed = true;
      }
    }
    if changed {
      self.mark_paint_hash_dirty();
    }
    changed
  }

  /// Mutably access the IME preedit state, marking the cached paint hash dirty.
  pub fn ime_preedit_mut(&mut self) -> &mut Option<ImePreeditState> {
    self.mark_paint_hash_dirty();
    &mut self.ime_preedit
  }

  /// Mutably access the file-input selection map, marking the cached paint hash dirty.
  pub fn file_inputs_mut(&mut self) -> &mut FxHashMap<usize, Vec<FileSelection>> {
    self.mark_paint_hash_dirty();
    &mut self.form_state.file_inputs
  }

  /// Set (or clear) the selected files for an `<input type=file>` control, marking the cached paint
  /// hash dirty when the entry changes.
  pub fn set_file_input_files(&mut self, node_id: usize, files: Vec<FileSelection>) -> bool {
    let changed = match self.form_state.file_inputs.get(&node_id) {
      Some(existing) => existing.as_slice() != files.as_slice(),
      None => !files.is_empty(),
    };
    if !changed {
      return false;
    }
    if files.is_empty() {
      self.form_state.file_inputs.remove(&node_id);
    } else {
      self.form_state.file_inputs.insert(node_id, files);
    }
    self.mark_paint_hash_dirty();
    true
  }

  #[inline]
  pub fn form_state(&self) -> &FormState {
    &self.form_state
  }

  /// Mutably access live form state overrides.
  ///
  /// This automatically marks the cached paint interaction hash dirty so render caching observes
  /// changes without requiring callers to remember to invoke
  /// [`mark_paint_hash_dirty`](Self::mark_paint_hash_dirty).
  #[inline]
  pub fn form_state_mut(&mut self) -> &mut FormState {
    self.mark_paint_hash_dirty();
    &mut self.form_state
  }

  #[inline]
  pub fn has_user_validity(&self, node_id: usize) -> bool {
    self.user_validity.contains(&node_id)
  }

  #[inline]
  pub fn user_validity(&self) -> &FxHashSet<usize> {
    &self.user_validity
  }

  /// Mutably access the user-validity set.
  ///
  /// This automatically marks the cached CSS interaction hash dirty so render caching observes any
  /// modifications.
  #[inline]
  pub fn user_validity_mut(&mut self) -> &mut FxHashSet<usize> {
    self.mark_css_hash_dirty();
    &mut self.user_validity
  }

  #[inline]
  pub fn insert_user_validity(&mut self, node_id: usize) -> bool {
    let changed = self.user_validity.insert(node_id);
    if changed {
      self.mark_css_hash_dirty();
    }
    changed
  }

  /// Mark the cached CSS interaction hash as dirty.
  ///
  /// Callers that mutate focus/hover/active/visited/user-validity state must invoke this so render
  /// caching can observe the change without re-hashing large sets every frame.
  #[inline]
  pub fn mark_css_hash_dirty(&self) {
    self.css_hash_dirty.store(true, AtomicOrdering::Release);
  }

  /// Mark the cached paint interaction hash as dirty.
  ///
  /// Callers that mutate paint-only interaction state (IME preedit, caret/selection state, document
  /// selection, or live form-state overrides) must invoke this so render caching can observe the
  /// change without re-hashing large structures every frame.
  #[inline]
  pub fn mark_paint_hash_dirty(&self) {
    self.paint_hash_dirty.store(true, AtomicOrdering::Release);
  }

  #[inline]
  pub fn mark_all_hashes_dirty(&self) {
    self.mark_css_hash_dirty();
    self.mark_paint_hash_dirty();
  }

  /// Stable interaction hash for fields that can affect CSS selector matching.
  ///
  /// This is intended for render caching: it avoids per-frame sorting of large sets by caching the
  /// computed digest and only recomputing when the interaction state mutates.
  pub fn interaction_css_hash(&self) -> u64 {
    if self.css_hash_dirty.load(AtomicOrdering::Acquire) {
      let hash = compute_css_hash(self);
      // Store the computed hash before clearing the dirty flag so readers never observe
      // `css_hash_dirty=false` with a stale `cached_css_hash`.
      self.cached_css_hash.store(hash, AtomicOrdering::Relaxed);
      self.css_hash_dirty.store(false, AtomicOrdering::Release);
      return hash;
    }
    self.cached_css_hash.load(AtomicOrdering::Relaxed)
  }

  /// Stable interaction hash for fields that only affect paint output.
  ///
  /// This is intended for render caching: it avoids per-frame sorting of large sets/maps by caching
  /// the computed digest and only recomputing when the interaction state mutates.
  pub fn interaction_paint_hash(&self) -> u64 {
    if self.paint_hash_dirty.load(AtomicOrdering::Acquire) {
      let hash = compute_paint_hash(self);
      self.cached_paint_hash.store(hash, AtomicOrdering::Relaxed);
      self.paint_hash_dirty.store(false, AtomicOrdering::Release);
      return hash;
    }
    self.cached_paint_hash.load(AtomicOrdering::Relaxed)
  }
}

impl Default for InteractionState {
  fn default() -> Self {
    Self {
      focused: None,
      focus_visible: false,
      fullscreen_element: None,
      focus_chain: Vec::new(),
      focus_chain_membership: FxHashSet::default(),
      hover_chain: Vec::new(),
      hover_chain_membership: FxHashSet::default(),
      active_chain: Vec::new(),
      active_chain_membership: FxHashSet::default(),
      visited_links: FxHashSet::default(),
      open_select_dropdown: None,
      ime_preedit: None,
      text_edit: None,
      form_state: FormState::default(),
      document_selection: None,
      user_validity: FxHashSet::default(),
      cached_css_hash: AtomicU64::new(0),
      cached_paint_hash: AtomicU64::new(0),
      css_hash_dirty: AtomicBool::new(true),
      paint_hash_dirty: AtomicBool::new(true),
    }
  }
}

impl Clone for InteractionState {
  fn clone(&self) -> Self {
    Self {
      focused: self.focused,
      focus_visible: self.focus_visible,
      fullscreen_element: self.fullscreen_element,
      focus_chain: self.focus_chain.clone(),
      focus_chain_membership: self.focus_chain_membership.clone(),
      hover_chain: self.hover_chain.clone(),
      hover_chain_membership: self.hover_chain_membership.clone(),
      active_chain: self.active_chain.clone(),
      active_chain_membership: self.active_chain_membership.clone(),
      visited_links: self.visited_links.clone(),
      open_select_dropdown: self.open_select_dropdown,
      ime_preedit: self.ime_preedit.clone(),
      text_edit: self.text_edit,
      form_state: self.form_state.clone(),
      document_selection: self.document_selection.clone(),
      user_validity: self.user_validity.clone(),
      cached_css_hash: AtomicU64::new(self.cached_css_hash.load(AtomicOrdering::Relaxed)),
      cached_paint_hash: AtomicU64::new(self.cached_paint_hash.load(AtomicOrdering::Relaxed)),
      // `InteractionState` is frequently cloned for "build a slightly modified state" patterns in
      // tests and embedder code. Mark hashes dirty so callers can safely mutate public fields on the
      // clone without needing to remember to call `mark_*_hash_dirty()`.
      css_hash_dirty: AtomicBool::new(true),
      paint_hash_dirty: AtomicBool::new(true),
    }
  }
}

fn hash_usize_set(hasher: &mut DefaultHasher, set: &FxHashSet<usize>) {
  let mut values: Vec<usize> = set.iter().copied().collect();
  values.sort_unstable();
  values.hash(hasher);
}

fn compute_css_hash(state: &InteractionState) -> u64 {
  let mut hasher = DefaultHasher::new();
  state.focused.hash(&mut hasher);
  state.focus_visible.hash(&mut hasher);
  state.fullscreen_element.hash(&mut hasher);
  state.focus_chain.hash(&mut hasher);
  state.hover_chain.hash(&mut hasher);
  state.active_chain.hash(&mut hasher);
  hash_usize_set(&mut hasher, &state.visited_links);
  hash_usize_set(&mut hasher, &state.user_validity);
  hasher.finish()
}

fn compute_paint_hash(state: &InteractionState) -> u64 {
  let mut hasher = DefaultHasher::new();

  // Live form-control state can change without DOM mutations (e.g. JS/property edits). Include it so
  // cached paint paths observe updates without forcing a full cascade/layout.
  if !state.form_state.values.is_empty() {
    let mut keys: Vec<usize> = state.form_state.values.keys().copied().collect();
    keys.sort_unstable();
    for node_id in keys {
      node_id.hash(&mut hasher);
      if let Some(value) = state.form_state.values.get(&node_id) {
        value.hash(&mut hasher);
      }
    }
  }
  if !state.form_state.checked.is_empty() {
    let mut keys: Vec<usize> = state.form_state.checked.keys().copied().collect();
    keys.sort_unstable();
    for node_id in keys {
      node_id.hash(&mut hasher);
      if let Some(checked) = state.form_state.checked.get(&node_id) {
        checked.hash(&mut hasher);
      }
    }
  }
  if !state.form_state.select_selected.is_empty() {
    let mut keys: Vec<usize> = state.form_state.select_selected.keys().copied().collect();
    keys.sort_unstable();
    for select_id in keys {
      select_id.hash(&mut hasher);
      if let Some(selected) = state.form_state.select_selected.get(&select_id) {
        hash_usize_set(&mut hasher, selected);
      }
    }
  }

  // File input state is stored out-of-DOM, so include it so file drops trigger repaints (label
  // updates and form submission semantics).
  if !state.form_state.file_inputs.is_empty() {
    let mut keys: Vec<usize> = state.form_state.file_inputs.keys().copied().collect();
    keys.sort_unstable();
    for node_id in keys {
      node_id.hash(&mut hasher);
      if let Some(files) = state.form_state.file_inputs.get(&node_id) {
        files.len().hash(&mut hasher);
        for file in files {
          file.path.to_string_lossy().as_ref().hash(&mut hasher);
          file.filename.hash(&mut hasher);
          file.bytes.len().hash(&mut hasher);
          file.content_type.hash(&mut hasher);
        }
      }
    }
  }

  if let Some(preedit) = &state.ime_preedit {
    1u8.hash(&mut hasher);
    preedit.node_id.hash(&mut hasher);
    preedit.text.hash(&mut hasher);
    preedit.cursor.hash(&mut hasher);
  } else {
    0u8.hash(&mut hasher);
  }

  if let Some(edit) = &state.text_edit {
    1u8.hash(&mut hasher);
    edit.node_id.hash(&mut hasher);
    edit.caret.hash(&mut hasher);
    edit.caret_affinity.hash(&mut hasher);
    edit.selection.hash(&mut hasher);
  } else {
    0u8.hash(&mut hasher);
  }

  if let Some(selection) = &state.document_selection {
    1u8.hash(&mut hasher);
    selection.hash(&mut hasher);
  } else {
    0u8.hash(&mut hasher);
  }

  hasher.finish()
}

/// In-progress IME (Input Method Editor) composition state for a focused control.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImePreeditState {
  pub node_id: usize,
  pub text: String,
  pub cursor: Option<(usize, usize)>,
}

/// Caret + selection state for a focused text control (`<input>` / `<textarea>`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TextEditPaintState {
  pub node_id: usize,
  /// Caret position in character indices.
  pub caret: usize,
  /// Visual affinity for the caret when the logical boundary maps to multiple x positions.
  pub caret_affinity: CaretAffinity,
  /// Optional selection range in character indices (start, end), where `start < end`.
  pub selection: Option<(usize, usize)>,
}

/// Live (non-DOM) form control state keyed by stable [`dom2::NodeId`](crate::dom2::NodeId).
///
/// This is the stable counterpart to [`FormState`]. It is intended to be stored alongside a live
/// `dom2` document where nodes can be inserted/removed without invalidating this state.
#[derive(Debug, Clone, Default)]
pub struct FormStateDom2 {
  /// Current value for value-bearing controls (`<input>` / `<textarea>` / etc.), keyed by stable
  /// `dom2` [`NodeId`].
  pub values: FxHashMap<NodeId, String>,
  /// Current checked state for checkbox/radio inputs, keyed by stable `dom2` [`NodeId`].
  pub checked: FxHashMap<NodeId, bool>,
  /// Current file selections for `<input type="file">`, keyed by stable `dom2` [`NodeId`].
  pub file_inputs: FxHashMap<NodeId, Vec<FileSelection>>,
  /// Current selected option ids for `<select>` elements, keyed by stable `dom2` [`NodeId`].
  ///
  /// When a select id is present in this map, the selection set is treated as authoritative for that
  /// select (including the empty set for multi-selects).
  pub select_selected: FxHashMap<NodeId, FxHashSet<NodeId>>,
}

impl FormStateDom2 {
  #[inline]
  pub fn has_overrides(&self) -> bool {
    !(self.values.is_empty()
      && self.checked.is_empty()
      && self.file_inputs.is_empty()
      && self.select_selected.is_empty())
  }

  /// Project this stable state into the renderer's preorder-id keyed [`FormState`].
  ///
  /// Any entries whose nodes are detached from the renderer snapshot (unmappable `NodeId`) are
  /// dropped.
  pub fn project_to_preorder(&self, mapping: &RendererDomMapping) -> FormState {
    let mut projected = FormState::default();

    projected.values = self
      .values
      .iter()
      .filter_map(|(&node_id, value)| {
        mapping
          .preorder_for_node_id(node_id)
          .map(|preorder| (preorder, value.clone()))
      })
      .collect();

    projected.checked = self
      .checked
      .iter()
      .filter_map(|(&node_id, &checked)| {
        mapping
          .preorder_for_node_id(node_id)
          .map(|preorder| (preorder, checked))
      })
      .collect();

    projected.file_inputs = self
      .file_inputs
      .iter()
      .filter_map(|(&node_id, files)| {
        mapping
          .preorder_for_node_id(node_id)
          .map(|preorder| (preorder, files.clone()))
      })
      .collect();

    projected.select_selected = self
      .select_selected
      .iter()
      .filter_map(|(&select_id, options)| {
        let select_preorder = mapping.preorder_for_node_id(select_id)?;
        let projected_options: FxHashSet<usize> = options
          .iter()
          .filter_map(|&id| mapping.preorder_for_node_id(id))
          .collect();
        Some((select_preorder, projected_options))
      })
      .collect();

    projected
  }
}

/// In-progress IME (Input Method Editor) composition state for a focused control, keyed by stable
/// `dom2` node ids.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImePreeditStateDom2 {
  pub node_id: NodeId,
  pub text: String,
  pub cursor: Option<(usize, usize)>,
}

/// Caret + selection state for a focused text control (`<input>` / `<textarea>`), keyed by stable
/// `dom2` node ids.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TextEditPaintStateDom2 {
  pub node_id: NodeId,
  /// Caret position in character indices.
  pub caret: usize,
  /// Visual affinity for the caret when the logical boundary maps to multiple x positions.
  pub caret_affinity: CaretAffinity,
  /// Optional selection range in character indices (start, end), where `start < end`.
  pub selection: Option<(usize, usize)>,
}

/// Internal, non-DOM-visible interaction state keyed by stable [`dom2::NodeId`](crate::dom2::NodeId).
///
/// ## Stable vs renderer interaction state
///
/// The renderer's selector/layout/paint pipeline consumes [`InteractionState`], which is keyed by
/// **renderer preorder ids** (the 1-based ids produced by [`crate::dom::enumerate_dom_ids`]).
/// Preorder ids are specific to a particular immutable DOM snapshot; they can change whenever the
/// underlying `dom2` document is mutated (insertion/removal/reordering).
///
/// `InteractionStateDom2` is the stable counterpart intended to be stored alongside the live `dom2`
/// document. When the renderer needs interaction state for a particular snapshot, project this
/// stable state to preorder ids using [`InteractionStateDom2::project_to_preorder`] with the
/// snapshot's [`RendererDomMapping`].
#[derive(Debug, Clone, Default)]
pub struct InteractionStateDom2 {
  /// Currently focused element `NodeId`.
  pub focused: Option<NodeId>,
  /// Whether the focused element should match `:focus-visible`.
  pub focus_visible: bool,
  /// Current fullscreen element `NodeId` (drives `:fullscreen` pseudo-class matching).
  pub fullscreen_element: Option<NodeId>,
  /// The focused element and its element ancestors (used for `:focus-within` matching).
  pub focus_chain: Vec<NodeId>,
  /// The element under the pointer and its element ancestors (used for `:hover` matching).
  pub hover_chain: Vec<NodeId>,
  /// The active element (e.g. pointer down) and its element ancestors (used for `:active` matching).
  pub active_chain: Vec<NodeId>,
  /// Set of link node ids that have been visited in this document.
  pub visited_links: FxHashSet<NodeId>,
  /// Optional IME composition (preedit) state for the focused text control.
  pub ime_preedit: Option<ImePreeditStateDom2>,
  /// Optional caret/selection state for a focused text control (`<input>` / `<textarea>`).
  pub text_edit: Option<TextEditPaintStateDom2>,
  /// Live form state for value-bearing and toggleable controls.
  pub form_state: FormStateDom2,
  /// Current document (non-form-control) selection.
  pub document_selection: Option<DocumentSelectionStateDom2>,
  /// Node ids (controls/forms) that have flipped HTML "user validity" from false to true.
  pub user_validity: FxHashSet<NodeId>,
}

impl InteractionStateDom2 {
  /// Drop or clear any interaction targets that are not connected for scripting in the live `dom2`
  /// document.
  ///
  /// This is stronger than [`Self::prune_detached`], which only checks whether nodes are reachable
  /// from the current *renderer snapshot* via [`RendererDomMapping`]. The renderer snapshot includes
  /// inert `<template>` contents for id stability, but scripting algorithms treat those nodes as
  /// disconnected; callers that maintain interaction state alongside a live `dom2::Document` should
  /// prefer this method when responding to DOM mutations.
  pub fn prune_disconnected(&mut self, dom: &crate::dom2::Document) {
    let is_connected = |id: NodeId| dom.is_connected_for_scripting(id);

    if self
      .fullscreen_element
      .is_some_and(|id| !is_connected(id))
    {
      self.fullscreen_element = None;
    }

    if self.focused.is_some_and(|id| !is_connected(id)) {
      self.focused = None;
      self.focus_visible = false;
      self.focus_chain.clear();
      self.ime_preedit = None;
      self.text_edit = None;
    } else if self.focused.is_none() {
      self.focus_visible = false;
      self.focus_chain.clear();
      self.ime_preedit = None;
      self.text_edit = None;
    } else {
      self.focus_chain.retain(|&id| is_connected(id));
      if let Some(focused) = self.focused {
        if self
          .ime_preedit
          .as_ref()
          .is_some_and(|state| state.node_id != focused || !is_connected(state.node_id))
        {
          self.ime_preedit = None;
        }
        if self
          .text_edit
          .is_some_and(|state| state.node_id != focused || !is_connected(state.node_id))
        {
          self.text_edit = None;
        }
      }
    }

    fn prune_hit_chain(chain: &mut Vec<NodeId>, dom: &crate::dom2::Document) {
      let Some(&root) = chain.first() else {
        return;
      };
      if !dom.is_connected_for_scripting(root) {
        chain.clear();
        return;
      }
      chain.retain(|&id| dom.is_connected_for_scripting(id));
    }

    prune_hit_chain(&mut self.hover_chain, dom);
    prune_hit_chain(&mut self.active_chain, dom);
  }

  /// Drop or clear any interaction targets that are no longer reachable from the document root for
  /// the current renderer snapshot.
  ///
  /// `dom2::NodeId` handles remain valid even after nodes are removed from the tree, so interaction
  /// state must actively prune stale ids to avoid keeping focus/hover/active state pinned to
  /// detached subtrees.
  ///
  /// A node is treated as "detached" when it has no renderer preorder id in `mapping` (i.e.
  /// [`RendererDomMapping::preorder_for_node_id`] returns `None`).
  pub fn prune_detached(&mut self, mapping: &RendererDomMapping) {
    let is_connected = |id: NodeId| mapping.preorder_for_node_id(id).is_some();

    if self.fullscreen_element.is_some_and(|id| !is_connected(id)) {
      self.fullscreen_element = None;
    }

    if self.focused.is_some_and(|id| !is_connected(id)) {
      self.focused = None;
      self.focus_visible = false;
      self.focus_chain.clear();
      self.ime_preedit = None;
      self.text_edit = None;
    } else if self.focused.is_none() {
      // Maintain invariants even when callers clear focus directly.
      self.focus_visible = false;
      self.focus_chain.clear();
      self.ime_preedit = None;
      self.text_edit = None;
    } else {
      // Focus chain is derived from focus; drop any ancestors that are no longer mappable.
      self.focus_chain.retain(|&id| is_connected(id));

      // IME/text-edit state is tied to the focused control.
      if let Some(focused) = self.focused {
        if self
          .ime_preedit
          .as_ref()
          .is_some_and(|state| state.node_id != focused || !is_connected(state.node_id))
        {
          self.ime_preedit = None;
        }
        if self
          .text_edit
          .is_some_and(|state| state.node_id != focused || !is_connected(state.node_id))
        {
          self.text_edit = None;
        }
      }
    }

    fn prune_hit_chain(chain: &mut Vec<NodeId>, mapping: &RendererDomMapping) {
      let Some(&root) = chain.first() else {
        return;
      };
      if mapping.preorder_for_node_id(root).is_none() {
        // If the root hit node becomes detached, drop the whole chain (including any associated
        // control nodes that may still be connected).
        chain.clear();
        return;
      }
      chain.retain(|&id| mapping.preorder_for_node_id(id).is_some());
    }

    prune_hit_chain(&mut self.hover_chain, mapping);
    prune_hit_chain(&mut self.active_chain, mapping);

    // Conservative: drop detached nodes from these per-element sets to avoid retaining stale ids.
    self.visited_links.retain(|&id| is_connected(id));
    self.user_validity.retain(|&id| is_connected(id));

    let should_clear_selection = if let Some(selection) = &mut self.document_selection {
      !selection.prune_detached(mapping)
    } else {
      false
    };
    if should_clear_selection {
      self.document_selection = None;
    }
  }

  /// Project this stable, `dom2::NodeId` keyed state into the renderer's preorder-id keyed
  /// [`InteractionState`].
  ///
  /// Mapping semantics:
  /// - Each `NodeId` is translated via [`RendererDomMapping::preorder_for_node_id`].
  /// - Any nodes that are detached/unmappable in the target snapshot are dropped.
  /// - For vec "chains", order is preserved while filtering out unmappable nodes.
  /// - If the focused node is unmappable, the projected `focused` is set to `None` and the projected
  ///   `focus_chain` is cleared (since it is derived from focus).
  pub fn project_to_preorder(&mut self, mapping: &RendererDomMapping) -> InteractionState {
    self.prune_detached(mapping);

    let focused_preorder = self
      .focused
      .and_then(|node_id| mapping.preorder_for_node_id(node_id));

    let fullscreen_preorder = self
      .fullscreen_element
      .and_then(|node_id| mapping.preorder_for_node_id(node_id));

    let focus_chain = if focused_preorder.is_some() {
      self
        .focus_chain
        .iter()
        .copied()
        .filter_map(|id| mapping.preorder_for_node_id(id))
        .collect()
    } else {
      Vec::new()
    };

    let hover_chain: Vec<usize> = self
      .hover_chain
      .iter()
      .copied()
      .filter_map(|id| mapping.preorder_for_node_id(id))
      .collect();

    let active_chain: Vec<usize> = self
      .active_chain
      .iter()
      .copied()
      .filter_map(|id| mapping.preorder_for_node_id(id))
      .collect();

    let visited_links: FxHashSet<usize> = self
      .visited_links
      .iter()
      .copied()
      .filter_map(|id| mapping.preorder_for_node_id(id))
      .collect();

    let user_validity: FxHashSet<usize> = self
      .user_validity
      .iter()
      .copied()
      .filter_map(|id| mapping.preorder_for_node_id(id))
      .collect();

    let ime_preedit = self.ime_preedit.as_ref().and_then(|state| {
      let node_id = mapping.preorder_for_node_id(state.node_id)?;
      Some(ImePreeditState {
        node_id,
        text: state.text.clone(),
        cursor: state.cursor,
      })
    });

    let text_edit = self.text_edit.and_then(|state| {
      let node_id = mapping.preorder_for_node_id(state.node_id)?;
      Some(TextEditPaintState {
        node_id,
        caret: state.caret,
        caret_affinity: state.caret_affinity,
        selection: state.selection,
      })
    });

    let document_selection = self
      .document_selection
      .as_ref()
      .map(|sel| sel.project_to_preorder(mapping));

    let fullscreen_element = self
      .fullscreen_element
      .and_then(|id| mapping.preorder_for_node_id(id));

    let mut projected = InteractionState::default();
    projected.focused = focused_preorder;
    projected.focus_visible = self.focus_visible && focused_preorder.is_some();
    projected.set_fullscreen_element(fullscreen_preorder);
    projected.set_focus_chain(focus_chain);
    projected.set_hover_chain(hover_chain);
    projected.set_active_chain(active_chain);
    projected.visited_links = visited_links;
    projected.ime_preedit = ime_preedit;
    projected.text_edit = text_edit;
    projected.form_state = self.form_state.project_to_preorder(mapping);
    projected.document_selection = document_selection;
    projected.user_validity = user_validity;
    projected.fullscreen_element = fullscreen_element;
    projected
  }
}
