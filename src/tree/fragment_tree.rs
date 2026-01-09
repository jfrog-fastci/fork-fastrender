//! Fragment Tree - Represents layout results with positions and sizes
//!
//! Fragments are the output of layout. Unlike boxes (which represent what
//! to layout), fragments represent where things ended up with final positions
//! and dimensions.
//!
//! # Key Differences from Boxes
//!
//! | Box Tree | Fragment Tree |
//! |----------|---------------|
//! | Immutable | Created per layout |
//! | No position | Has position (Rect) |
//! | 1 box = 1 box | 1 box → N fragments (splitting) |
//! | Input to layout | Output of layout |
//!
//! # Fragment Splitting
//!
//! A single box can generate multiple fragments:
//! - Inline boxes split across multiple lines
//! - Blocks split across columns or pages
//! - Table cells split for pagination
//!
//! # Usage
//!
//! ```
//! use fastrender::{FragmentNode, FragmentContent};
//! use fastrender::{Rect, Point, Size};
//!
//! let fragment = FragmentNode::new_block(
//!     Rect::from_xywh(10.0, 20.0, 100.0, 50.0),
//!     vec![],
//! );
//!
//! assert_eq!(fragment.bounds.x(), 10.0);
//! assert!(fragment.contains_point(Point::new(50.0, 30.0)));
//! ```
//!
//! # Structural sharing
//!
//! Fragment trees clone via structural sharing: child vectors are wrapped in an `Arc` and cloned
//! by reference. Mutations must go through `children_mut()` (or `&mut fragment.children`), which
//! triggers copy-on-write and preserves immutability for cached subtrees. Use `deep_clone()` when
//! a fully-owned copy of a fragment subtree is required.
//!
//! # Structural sharing invariants
//!
//! - Fragment nodes are treated as immutable once produced by layout. Cached subtrees may be reused
//!   across formatting context cache hits, so mutations must always go through `children_mut()` to
//!   trigger copy-on-write when a shared child slice is still referenced elsewhere.
//! - `clone()` is intentionally shallow and should remain effectively O(1) without heap allocations;
//!   use [`deep_clone`](FragmentNode::deep_clone) when a call site needs to detach the entire
//!   subtree for mutation.
//! - When adding new mutation sites, prefer `FragmentNode::clone_without_children` and
//!   `FragmentNode::set_children` to avoid accidentally deep-cloning child lists.
//! - Fragment clone instrumentation (`FASTR_PROFILE_FRAGMENT_CLONES=1`) records shallow clone calls,
//!   deep clone events, and traversal counts when translation routines walk subtrees. Enable it when
//!   validating structural sharing or cache reuse.

use crate::css::types::KeyframesRule;
use crate::geometry::Point;
use crate::geometry::Rect;
use crate::geometry::Size;
use crate::scroll::ScrollMetadata;
use crate::animation::TransitionState;
use crate::style::color::Rgba;
use crate::style::types::{BorderStyle, Overflow};
use crate::style::ComputedStyle;
use crate::text::pipeline::ShapedRun;
use crate::tree::box_tree::{BoxNode, BoxTree, ReplacedType};
use std::cell::Cell;
use std::collections::HashMap;
use std::fmt;
use std::num::NonZeroU64;
use std::ops::{Deref, DerefMut};
use std::ops::Range;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;

static FRAGMENT_DEEP_CLONES: AtomicUsize = AtomicUsize::new(0);
static FRAGMENT_TRAVERSAL_NODES: AtomicUsize = AtomicUsize::new(0);
static FRAGMENT_SHALLOW_CLONES: AtomicUsize = AtomicUsize::new(0);
static FRAGMENT_INSTRUMENTATION_ENABLED: AtomicBool = AtomicBool::new(false);

thread_local! {
  static FRAGMENT_INSTRUMENTATION_THREAD_ENABLED: Cell<bool> = const { Cell::new(false) };
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FragmentInstrumentationCounters {
  pub shallow_clones: usize,
  pub deep_clones: usize,
  pub traversed_nodes: usize,
}

pub fn reset_fragment_instrumentation_counters() {
  FRAGMENT_SHALLOW_CLONES.store(0, Ordering::Relaxed);
  FRAGMENT_DEEP_CLONES.store(0, Ordering::Relaxed);
  FRAGMENT_TRAVERSAL_NODES.store(0, Ordering::Relaxed);
}

pub fn fragment_instrumentation_counters() -> FragmentInstrumentationCounters {
  FragmentInstrumentationCounters {
    shallow_clones: FRAGMENT_SHALLOW_CLONES.load(Ordering::Relaxed),
    deep_clones: FRAGMENT_DEEP_CLONES.load(Ordering::Relaxed),
    traversed_nodes: FRAGMENT_TRAVERSAL_NODES.load(Ordering::Relaxed),
  }
}

pub fn set_fragment_instrumentation_enabled(enabled: bool) {
  FRAGMENT_INSTRUMENTATION_ENABLED.store(enabled, Ordering::Relaxed);
}

pub(crate) fn fragment_instrumentation_enabled() -> bool {
  FRAGMENT_INSTRUMENTATION_ENABLED.load(Ordering::Relaxed)
    || FRAGMENT_INSTRUMENTATION_THREAD_ENABLED.with(|flag| flag.get())
}

pub(crate) fn record_fragment_traversal(nodes: usize) {
  if fragment_instrumentation_enabled() && nodes > 0 {
    FRAGMENT_TRAVERSAL_NODES.fetch_add(nodes, Ordering::Relaxed);
  }
}

fn record_shallow_clone() {
  if fragment_instrumentation_enabled() {
    FRAGMENT_SHALLOW_CLONES.fetch_add(1, Ordering::Relaxed);
  }
}

fn record_deep_clone() {
  if fragment_instrumentation_enabled() {
    FRAGMENT_DEEP_CLONES.fetch_add(1, Ordering::Relaxed);
  }
}

pub struct FragmentInstrumentationGuard {
  previous: bool,
}

impl Drop for FragmentInstrumentationGuard {
  fn drop(&mut self) {
    set_fragment_instrumentation_enabled(self.previous);
  }
}

pub fn enable_fragment_instrumentation(enabled: bool) -> FragmentInstrumentationGuard {
  let previous = FRAGMENT_INSTRUMENTATION_ENABLED.swap(enabled, Ordering::Relaxed);
  FragmentInstrumentationGuard { previous }
}

pub struct FragmentInstrumentationThreadGuard {
  previous: bool,
}

impl Drop for FragmentInstrumentationThreadGuard {
  fn drop(&mut self) {
    FRAGMENT_INSTRUMENTATION_THREAD_ENABLED.with(|flag| flag.set(self.previous));
  }
}

pub fn enable_fragment_instrumentation_for_current_thread(
  enabled: bool,
) -> FragmentInstrumentationThreadGuard {
  let previous = FRAGMENT_INSTRUMENTATION_THREAD_ENABLED.with(|flag| flag.replace(enabled));
  FragmentInstrumentationThreadGuard { previous }
}

/// Overrides for paint-time stacking context behavior.
///
/// Most fragments follow the normal CSS stacking context rules derived from their computed style.
/// Some synthetic fragments (e.g. CSS Paged Media page boxes / margin boxes) need special
/// stacking behavior that cannot be expressed purely via style properties.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FragmentStackingContext {
  /// Use normal CSS stacking context creation rules.
  #[default]
  Normal,
  /// Force this fragment to establish a stacking context with the provided z-index.
  Forced { z_index: i32 },
}

impl FragmentStackingContext {
  pub fn forced_z_index(&self) -> Option<i32> {
    match *self {
      FragmentStackingContext::Forced { z_index } => Some(z_index),
      FragmentStackingContext::Normal => None,
    }
  }
}

/// Shared, copy-on-write fragment children storage.
///
/// Fragment nodes clone cheaply by sharing their children vectors through an `Arc`. Mutable access
/// uses [`Arc::make_mut`], so call sites can mutate through `&mut FragmentChildren` (or the
/// convenience [`FragmentNode::children_mut`] helper) without risking aliasing.
#[derive(Debug, Clone, Default)]
pub struct FragmentChildren(Arc<Vec<FragmentNode>>);

impl FragmentChildren {
  pub fn new(children: Vec<FragmentNode>) -> Self {
    Self(Arc::new(children))
  }

  /// Returns the number of strong references pointing at this child slice.
  pub fn strong_count(&self) -> usize {
    Arc::strong_count(&self.0)
  }

  /// Returns true when two child lists point to the same allocation.
  pub fn ptr_eq(&self, other: &Self) -> bool {
    Arc::ptr_eq(&self.0, &other.0)
  }

  /// Produces a deep clone of all descendants, breaking any structural sharing.
  pub fn deep_clone(&self) -> Self {
    Self(Arc::new(
      self.iter().map(FragmentNode::deep_clone).collect(),
    ))
  }

  /// Returns a shallow clone of the underlying vector.
  pub fn to_vec(&self) -> Vec<FragmentNode> {
    self.iter().cloned().collect()
  }
}

impl From<Vec<FragmentNode>> for FragmentChildren {
  fn from(children: Vec<FragmentNode>) -> Self {
    Self::new(children)
  }
}

impl Deref for FragmentChildren {
  type Target = Vec<FragmentNode>;

  fn deref(&self) -> &Self::Target {
    &self.0
  }
}

impl DerefMut for FragmentChildren {
  fn deref_mut(&mut self) -> &mut Self::Target {
    Arc::make_mut(&mut self.0)
  }
}

impl<'a> IntoIterator for &'a FragmentChildren {
  type Item = &'a FragmentNode;
  type IntoIter = std::slice::Iter<'a, FragmentNode>;

  fn into_iter(self) -> Self::IntoIter {
    self.0.iter()
  }
}

impl<'a> IntoIterator for &'a mut FragmentChildren {
  type Item = &'a mut FragmentNode;
  type IntoIter = std::slice::IterMut<'a, FragmentNode>;

  fn into_iter(self) -> Self::IntoIter {
    let vec = Arc::make_mut(&mut self.0);
    vec.iter_mut()
  }
}

impl IntoIterator for FragmentChildren {
  type Item = FragmentNode;
  type IntoIter = std::vec::IntoIter<FragmentNode>;

  fn into_iter(self) -> Self::IntoIter {
    match Arc::try_unwrap(self.0) {
      Ok(vec) => vec.into_iter(),
      Err(shared) => (*shared).clone().into_iter(),
    }
  }
}

impl FromIterator<FragmentNode> for FragmentChildren {
  fn from_iter<T: IntoIterator<Item = FragmentNode>>(iter: T) -> Self {
    Self::new(iter.into_iter().collect())
  }
}

/// Extra offset to apply to text emphasis marks.
///
/// Offsets are expressed in CSS pixels along the block axis (perpendicular to the inline text
/// direction). The `over` field applies when marks are placed on the "over" side of the text, while
/// `under` applies for the "under" side.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct TextEmphasisOffset {
  pub over: f32,
  pub under: f32,
}

/// Byte range in the original source text represented by a text fragment.
///
/// Pagination uses this to build stable continuation tokens that can resume inside a text node even
/// when line wrapping changes between pages.
///
/// This is stored in a compact packed representation so it can be carried on every text fragment
/// without bloating the fragment tree (important for large tables).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(transparent)]
pub struct TextSourceRange(NonZeroU64);

impl TextSourceRange {
  /// Convert a byte range into a packed range.
  ///
  /// Returns `None` when the offsets don't fit into the packed representation (extremely large
  /// source strings).
  pub fn new(range: Range<usize>) -> Option<Self> {
    let start = u32::try_from(range.start).ok()?;
    let end = u32::try_from(range.end).ok()?;

    // Store +1 so (0..0) is representable and 0 can remain the `Option` sentinel.
    let start = start.checked_add(1)?;
    let end = end.checked_add(1)?;

    let encoded = ((start as u64) << 32) | end as u64;
    NonZeroU64::new(encoded).map(TextSourceRange)
  }

  #[inline]
  pub fn start(self) -> usize {
    let encoded = self.0.get();
    (((encoded >> 32) as u32) - 1) as usize
  }

  #[inline]
  pub fn end(self) -> usize {
    let encoded = self.0.get();
    (((encoded & 0xffff_ffff) as u32) - 1) as usize
  }

  #[inline]
  pub fn to_range(self) -> Range<usize> {
    self.start()..self.end()
  }
}

/// Content type of a fragment
///
/// Fragments can contain different types of content, each requiring
/// different paint and hit-testing logic.
#[derive(Debug, Clone)]
pub enum FragmentContent {
  /// Block-level content
  ///
  /// A positioned block box. Children are other block or line fragments.
  Block {
    /// Index or ID of source BoxNode
    /// For now, just store an optional ID
    box_id: Option<usize>,
  },

  /// Inline-level content (possibly split from inline box)
  ///
  /// A fragment of an inline box. One inline box can generate multiple
  /// inline fragments when it wraps across lines.
  Inline {
    /// Index or ID of source BoxNode
    box_id: Option<usize>,

    /// Which fragment this is (0 = first, 1 = second, etc.)
    /// Used when a single inline box splits across multiple lines
    fragment_index: usize,
  },

  /// Text content with shaped glyphs
  ///
  /// Actual text that has been shaped (font, positions, etc.).
  Text {
    /// The text content
    text: Arc<str>,

    /// Index or ID of source BoxNode (TextBox)
    box_id: Option<usize>,

    /// Byte range in the original source text represented by this fragment.
    ///
    /// This is used by pagination to build stable break tokens that can resume within a text node
    /// even when line wrapping changes between pages.
    ///
    /// When `None`, the fragment has no stable source mapping (e.g. synthesized text in tests).
    source_range: Option<TextSourceRange>,

    /// Baseline offset from fragment top
    /// Used for text alignment within line
    baseline_offset: f32,

    /// Pre-shaped runs for this text, if available
    ///
    /// Carrying shaped runs from layout allows painting to reuse the exact
    /// glyph positions and fonts chosen during layout instead of reshaping
    /// with potentially different fallback results.
    shaped: Option<Arc<Vec<ShapedRun>>>,

    /// True when this fragment represents a list marker (::marker)
    is_marker: bool,

    /// Extra offset to apply to text emphasis marks (e.g. when ruby annotations occupy the same
    /// side as the emphasis marks).
    emphasis_offset: TextEmphasisOffset,
  },

  /// Line box containing inline and text fragments
  ///
  /// Line boxes are generated during inline layout. They contain
  /// inline-level and text fragments arranged horizontally.
  Line {
    /// Baseline position relative to line box top
    baseline: f32,
  },

  /// Replaced element content
  ///
  /// A positioned replaced element (img, canvas, video, etc.)
  Replaced {
    /// Type of replaced content
    replaced_type: ReplacedType,

    /// Index or ID of source BoxNode
    box_id: Option<usize>,
  },

  /// Anchor for `position: running()` elements.
  ///
  /// Captures the rendered subtree of the running element without affecting in-flow layout.
  RunningAnchor {
    /// Name provided to `position: running(<name>)`.
    name: Arc<str>,
    /// Snapshot of the laid-out running element subtree.
    snapshot: Arc<FragmentNode>,
  },

  /// Anchor for `float: footnote` call sites.
  ///
  /// Captures the rendered subtree of the footnote body without affecting in-flow layout.
  FootnoteAnchor {
    /// Snapshot of the laid-out footnote body subtree.
    snapshot: Arc<FragmentNode>,
  },
}

// ReplacedType is imported from box_tree to avoid duplication

impl FragmentContent {
  /// Returns true if this is a block fragment
  pub fn is_block(&self) -> bool {
    matches!(self, FragmentContent::Block { .. })
  }

  /// Returns true if this is an inline fragment
  pub fn is_inline(&self) -> bool {
    matches!(self, FragmentContent::Inline { .. })
  }

  /// Returns true if this is a text fragment
  pub fn is_text(&self) -> bool {
    matches!(self, FragmentContent::Text { .. })
  }

  /// Returns true if this is a line fragment
  pub fn is_line(&self) -> bool {
    matches!(self, FragmentContent::Line { .. })
  }

  /// Returns true if this is a replaced element
  pub fn is_replaced(&self) -> bool {
    matches!(self, FragmentContent::Replaced { .. })
  }

  /// Gets the text content if this is a text fragment
  pub fn text(&self) -> Option<&str> {
    match self {
      FragmentContent::Text { text, .. } => Some(text),
      _ => None,
    }
  }
}

/// Resolved border segment for collapsed-border tables.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CollapsedBorderSegment {
  pub width: f32,
  pub style: BorderStyle,
  pub color: Rgba,
}

impl CollapsedBorderSegment {
  pub fn none() -> Self {
    Self {
      width: 0.0,
      style: BorderStyle::None,
      color: Rgba::TRANSPARENT,
    }
  }

  pub fn is_visible(&self) -> bool {
    self.width > 0.0 && !matches!(self.style, BorderStyle::None | BorderStyle::Hidden)
  }
}

/// Compact paint-time representation of a table's collapsed borders.
#[derive(Debug, Clone)]
pub struct TableCollapsedBorders {
  pub column_count: usize,
  pub row_count: usize,
  pub column_line_positions: Vec<f32>,
  pub row_line_positions: Vec<f32>,
  pub vertical_borders: Vec<CollapsedBorderSegment>,
  pub horizontal_borders: Vec<CollapsedBorderSegment>,
  pub corner_borders: Vec<CollapsedBorderSegment>,
  pub vertical_line_base: Vec<f32>,
  pub horizontal_line_base: Vec<f32>,
  /// Bounds covering all collapsed border strokes (relative to the table fragment origin).
  pub paint_bounds: Rect,
}

impl TableCollapsedBorders {
  #[inline]
  pub fn vertical_segment(&self, column: usize, row: usize) -> Option<CollapsedBorderSegment> {
    if row >= self.row_count || column > self.column_count {
      return None;
    }
    let idx = column.checked_mul(self.row_count)?.checked_add(row)?;
    self.vertical_borders.get(idx).copied()
  }

  #[inline]
  pub fn horizontal_segment(&self, row: usize, column: usize) -> Option<CollapsedBorderSegment> {
    if column >= self.column_count || row > self.row_count {
      return None;
    }
    let idx = row.checked_mul(self.column_count)?.checked_add(column)?;
    self.horizontal_borders.get(idx).copied()
  }

  #[inline]
  pub fn corner(&self, row: usize, column: usize) -> Option<CollapsedBorderSegment> {
    if column > self.column_count || row > self.row_count {
      return None;
    }
    let stride = self.column_count + 1;
    let idx = row.checked_mul(stride)?.checked_add(column)?;
    self.corner_borders.get(idx).copied()
  }

  #[inline]
  pub fn vertical_line_width(&self, index: usize) -> f32 {
    *self.vertical_line_base.get(index).unwrap_or(&0.0)
  }

  #[inline]
  pub fn horizontal_line_width(&self, index: usize) -> f32 {
    *self.horizontal_line_base.get(index).unwrap_or(&0.0)
  }
}

/// Physical track ranges for a grid container, used by fragmentation.
///
/// The ranges are stored in the grid container's local coordinate space:
/// - `rows` are physical **Y** intervals.
/// - `columns` are physical **X** intervals.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct GridTrackRanges {
  pub rows: Vec<(f32, f32)>,
  pub columns: Vec<(f32, f32)>,
}

/// Identifies the fragmentainer (page/column) a fragment belongs to.
///
/// Pagination yields distinct pages (`page_index`), while multi-column layout can further
/// partition content into column sets (`column_set_index`) and individual columns
/// (`column_index`). Fields are optional so a non-paginated, non-column layout can still use
/// the default `page_index = 0` with `None` for the column components.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FragmentainerPath {
  /// Zero-based page index within the paginated flow.
  pub page_index: usize,
  /// Which column set this fragment is part of (e.g., the nth multi-column segment).
  pub column_set_index: Option<usize>,
  /// Which column within the active column set this fragment occupies.
  pub column_index: Option<usize>,
}

impl FragmentainerPath {
  /// Creates a new path for the given page with no column information.
  pub fn new(page_index: usize) -> Self {
    Self {
      page_index,
      column_set_index: None,
      column_index: None,
    }
  }

  /// Updates the page index while preserving column information.
  pub fn with_page_index(mut self, page_index: usize) -> Self {
    self.page_index = page_index;
    self
  }

  /// Sets the column set and column indices.
  pub fn with_columns(mut self, column_set_index: usize, column_index: usize) -> Self {
    self.column_set_index = Some(column_set_index);
    self.column_index = Some(column_index);
    self
  }

  /// Combines this path with an existing one, filling in any missing column metadata.
  ///
  /// The supplied path always wins for page/column fields that are present; existing column
  /// metadata is used as a fallback when the new path leaves them unset.
  pub fn inherit_from(self, existing: &FragmentainerPath) -> Self {
    Self {
      page_index: self.page_index,
      column_set_index: self.column_set_index.or(existing.column_set_index),
      column_index: self.column_index.or(existing.column_index),
    }
  }

  /// Returns a flattened index representing the innermost fragmentainer.
  pub fn flattened_index(&self) -> usize {
    self
      .column_index
      .or(self.column_set_index)
      .unwrap_or(self.page_index)
  }
}

impl Default for FragmentainerPath {
  fn default() -> Self {
    Self::new(0)
  }
}

/// Block-level metadata useful for fragmentation adjustments.
///
/// Margins are stored as resolved pixel values from layout. The clipped flags
/// indicate whether the fragment was sliced at the top/bottom during
/// fragmentation, in which case margins should not be re-applied.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct BlockFragmentMetadata {
  pub margin_top: f32,
  pub margin_bottom: f32,
  pub clipped_top: bool,
  pub clipped_bottom: bool,
}

/// Grid placement metadata used to support spec-correct fragmentation behaviour.
///
/// When a grid container fragments across pages, grid items in the same row form
/// parallel fragmentation flows (CSS Break 4 §Parallel Fragmentation Flows). We
/// record per-item placement so the fragmentation phase can decide when it is
/// safe to treat a grid item as an independent flow (e.g. items that do not span
/// multiple rows in the fragmentation axis).
#[derive(Debug, Clone, PartialEq)]
pub struct GridItemFragmentationData {
  pub box_id: usize,
  pub row_start: u16,
  pub row_end: u16,
  pub column_start: u16,
  pub column_end: u16,
}

#[derive(Debug, Clone, PartialEq)]
pub struct GridFragmentationInfo {
  /// In-flow grid item placements, in the same order as the grid container
  /// fragment's in-flow children.
  pub items: Vec<GridItemFragmentationData>,
}

/// Space reserved inside a scroll container for classic (non-overlay) scrollbars.
///
/// Values are expressed in CSS pixels and correspond to physical edges in the fragment's
/// coordinate space. When non-zero, the reserved area behaves like additional padding for
/// layout sizing (i.e., it reduces the content box size).
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct ScrollbarReservation {
  pub left: f32,
  pub right: f32,
  pub top: f32,
  pub bottom: f32,
}

/// A single fragment in the fragment tree
///
/// Represents a laid-out box with a definite position and size.
///
/// # Examples
///
/// ```
/// use fastrender::{FragmentNode, FragmentContent};
/// use fastrender::Rect;
///
/// let fragment = FragmentNode::new_block(
///     Rect::from_xywh(0.0, 0.0, 100.0, 50.0),
///     vec![],
/// );
///
/// assert_eq!(fragment.bounds.width(), 100.0);
/// assert!(fragment.content.is_block());
/// ```
#[derive(Debug)]
pub struct FragmentNode {
  /// The positioned rectangle of this fragment
  ///
  /// This is the final computed position and size after layout.
  /// All coordinates are in the coordinate space of the containing fragment.
  pub bounds: Rect,
  /// Optional block-level metadata used for fragmentation adjustments.
  pub block_metadata: Option<BlockFragmentMetadata>,

  /// Optional logical bounds used for fragmentation decisions.
  ///
  /// When absent, logical bounds match `bounds`.
  pub logical_override: Option<Rect>,

  /// The content type of this fragment
  pub content: FragmentContent,

  /// Optional collapsed border data for tables.
  pub table_borders: Option<Arc<TableCollapsedBorders>>,

  /// Optional physical grid track ranges (row/column bands) for grid containers.
  pub grid_tracks: Option<Arc<GridTrackRanges>>,

  /// Optional baseline offset from the fragment's top edge.
  ///
  /// Useful for fragments that need to participate in baseline alignment
  /// even when they don't contain explicit line/text children (e.g., tables).
  pub baseline: Option<f32>,

  /// Child fragments
  ///
  /// Children are stored in an `Arc<Vec<...>>` to allow fragment clones to share
  /// the same subtree cheaply while still permitting copy-on-write mutation via
  /// [`Arc::make_mut`]. A `Vec` (not slice) backing preserves ergonomic random
  /// access and mutation when a unique reference is required.
  ///
  /// For block fragments: block and line children
  /// For line fragments: inline and text children
  /// For inline/text/replaced: typically empty
  pub children: FragmentChildren,

  /// Computed style for painting
  ///
  /// Contains color, background, border, font and other paint-relevant properties.
  /// Optional for backwards compatibility with tests.
  pub style: Option<Arc<ComputedStyle>>,
  /// Starting style snapshot (pre-transition) when available.
  pub starting_style: Option<Arc<ComputedStyle>>,

  /// Paint-time stacking context override for this fragment.
  pub stacking_context: FragmentStackingContext,

  /// Index of this fragment within a fragmented sequence for the same box
  /// (e.g., when flowing across pages or columns).
  pub fragment_index: usize,

  /// Total number of fragments generated for the originating box.
  pub fragment_count: usize,

  /// Which fragmentainer (page/column) this fragment occupies.
  ///
  /// This remains as a flattened index of the innermost fragmentainer (column takes precedence
  /// over page) for backwards compatibility.
  pub fragmentainer_index: usize,

  /// Structured fragmentainer metadata (page/column set/column).
  pub fragmentainer: FragmentainerPath,

  /// Metadata about how this fragment relates to other fragments of the same box.
  ///
  /// Used for `box-decoration-break: slice`.
  pub slice_info: FragmentSliceInfo,

  /// Scrollable overflow area for this fragment (including descendants),
  /// expressed in the fragment's local coordinate space.
  ///
  /// When propagating descendant overflow into ancestors, intermediate overflow clipping
  /// (`overflow: hidden/scroll/auto/clip`) is respected so clipped descendants do not inflate
  /// ancestor scroll ranges or paint bounds.
  pub scroll_overflow: Rect,

  /// Space reserved for scrollbars inside this fragment's scrollport.
  pub scrollbar_reservation: ScrollbarReservation,

  /// Fragmentation metadata for nested fragmentainers (e.g., multi-column containers).
  pub fragmentation: Option<FragmentationInfo>,

  /// Grid-specific metadata used during fragmentation.
  pub grid_fragmentation: Option<Arc<GridFragmentationInfo>>,
}

impl Clone for FragmentNode {
  fn clone(&self) -> Self {
    record_shallow_clone();
    Self {
      bounds: self.bounds,
      block_metadata: self.block_metadata.clone(),
      logical_override: self.logical_override,
      content: self.content.clone(),
      table_borders: self.table_borders.clone(),
      grid_tracks: self.grid_tracks.clone(),
      baseline: self.baseline,
      children: self.children.clone(),
      style: self.style.clone(),
      starting_style: self.starting_style.clone(),
      stacking_context: self.stacking_context,
      fragment_index: self.fragment_index,
      fragment_count: self.fragment_count,
      fragmentainer_index: self.fragmentainer_index,
      fragmentainer: self.fragmentainer,
      slice_info: self.slice_info,
      scroll_overflow: self.scroll_overflow,
      scrollbar_reservation: self.scrollbar_reservation,
      fragmentation: self.fragmentation.clone(),
      grid_fragmentation: self.grid_fragmentation.clone(),
    }
  }
}

impl Drop for FragmentNode {
  fn drop(&mut self) {
    // Dropping deeply-nested fragment trees with Rust's default recursive drop can overflow the
    // stack. Drain descendants iteratively. Since fragment trees structurally share child vectors,
    // only descend into a child list when it is uniquely owned (`Arc::try_unwrap` succeeds).
    if self.children.is_empty() {
      return;
    }

    let empty_children = FragmentChildren::new(Vec::new());

    let children = std::mem::replace(&mut self.children, empty_children.clone());
    let mut stack = match Arc::try_unwrap(children.0) {
      Ok(children) => children,
      Err(_shared) => return,
    };

    while let Some(mut node) = stack.pop() {
      if node.children.is_empty() {
        continue;
      }
      let children = std::mem::replace(&mut node.children, empty_children.clone());
      if let Ok(mut children) = Arc::try_unwrap(children.0) {
        stack.append(&mut children);
      }
      // `node` is dropped here with an empty `children` list, so this `Drop` implementation is a
      // cheap no-op for drained descendants.
    }
  }
}

/// Fragmentation metadata for a fragment.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FragmentSliceInfo {
  /// Whether this fragment starts at the box's block-start edge.
  pub is_first: bool,
  /// Whether this fragment ends at the box's block-end edge.
  pub is_last: bool,
  /// Distance from the original box's block-start edge to this fragment slice's start.
  pub slice_offset: f32,
  /// Block-size of the unfragmented box.
  pub original_block_size: f32,
}

impl FragmentSliceInfo {
  pub fn single(block_size: f32) -> Self {
    Self {
      is_first: true,
      is_last: true,
      slice_offset: 0.0,
      original_block_size: block_size,
    }
  }
}

impl FragmentNode {
  /// Creates a new fragment with the given bounds, content, and children
  ///
  /// # Examples
  ///
  /// ```
  /// use fastrender::{FragmentNode, FragmentContent};
  /// use fastrender::Rect;
  ///
  /// let bounds = Rect::from_xywh(10.0, 20.0, 100.0, 50.0);
  /// let content = FragmentContent::Block { box_id: None };
  /// let fragment = FragmentNode::new(bounds, content, vec![]);
  ///
  /// assert_eq!(fragment.bounds.x(), 10.0);
  /// ```
  pub fn new(bounds: Rect, content: FragmentContent, children: Vec<FragmentNode>) -> Self {
    let scroll_overflow = Rect::from_xywh(0.0, 0.0, bounds.width(), bounds.height());
    let fragmentainer = FragmentainerPath::default();
    Self {
      bounds,
      block_metadata: None,
      logical_override: None,
      content,
      table_borders: None,
      grid_tracks: None,
      baseline: None,
      children: children.into(),
      style: None,
      starting_style: None,
      stacking_context: FragmentStackingContext::Normal,
      fragment_index: 0,
      fragment_count: 1,
      fragmentainer_index: fragmentainer.flattened_index(),
      fragmentainer,
      slice_info: FragmentSliceInfo::single(bounds.height()),
      scroll_overflow,
      scrollbar_reservation: ScrollbarReservation::default(),
      fragmentation: None,
      grid_fragmentation: None,
    }
  }

  /// Creates a new fragment with style information
  pub fn new_with_style(
    bounds: Rect,
    content: FragmentContent,
    children: Vec<FragmentNode>,
    style: Arc<ComputedStyle>,
  ) -> Self {
    let scroll_overflow = Rect::from_xywh(0.0, 0.0, bounds.width(), bounds.height());
    let fragmentainer = FragmentainerPath::default();
    Self {
      bounds,
      block_metadata: None,
      logical_override: None,
      content,
      table_borders: None,
      grid_tracks: None,
      baseline: None,
      children: children.into(),
      style: Some(style),
      starting_style: None,
      stacking_context: FragmentStackingContext::Normal,
      fragment_index: 0,
      fragment_count: 1,
      fragmentainer_index: fragmentainer.flattened_index(),
      fragmentainer,
      slice_info: FragmentSliceInfo::single(bounds.height()),
      scroll_overflow,
      scrollbar_reservation: ScrollbarReservation::default(),
      fragmentation: None,
      grid_fragmentation: None,
    }
  }

  pub fn force_stacking_context_with_z_index(&mut self, z_index: i32) {
    self.stacking_context = FragmentStackingContext::Forced { z_index };
  }

  /// Creates a new block fragment
  ///
  /// # Examples
  ///
  /// ```
  /// use fastrender::FragmentNode;
  /// use fastrender::Rect;
  ///
  /// let fragment = FragmentNode::new_block(
  ///     Rect::from_xywh(0.0, 0.0, 200.0, 100.0),
  ///     vec![],
  /// );
  ///
  /// assert!(fragment.content.is_block());
  /// ```
  pub fn new_block(bounds: Rect, children: Vec<FragmentNode>) -> Self {
    Self::new(bounds, FragmentContent::Block { box_id: None }, children)
  }

  /// Creates a new block fragment with style
  pub fn new_block_styled(
    bounds: Rect,
    children: Vec<FragmentNode>,
    style: Arc<ComputedStyle>,
  ) -> Self {
    Self::new_with_style(
      bounds,
      FragmentContent::Block { box_id: None },
      children,
      style,
    )
  }

  /// Returns a copy of this fragment with an explicit baseline offset.
  pub fn with_baseline(mut self, baseline: f32) -> Self {
    self.baseline = Some(baseline);
    self
  }

  /// Creates a new block fragment with a box ID
  pub fn new_block_with_id(bounds: Rect, box_id: usize, children: Vec<FragmentNode>) -> Self {
    Self::new(
      bounds,
      FragmentContent::Block {
        box_id: Some(box_id),
      },
      children,
    )
  }

  /// Creates a new inline fragment
  pub fn new_inline(bounds: Rect, fragment_index: usize, children: Vec<FragmentNode>) -> Self {
    Self::new(
      bounds,
      FragmentContent::Inline {
        box_id: None,
        fragment_index,
      },
      children,
    )
  }

  /// Creates a new inline fragment with style
  pub fn new_inline_styled(
    bounds: Rect,
    fragment_index: usize,
    children: Vec<FragmentNode>,
    style: Arc<ComputedStyle>,
  ) -> Self {
    Self::new_with_style(
      bounds,
      FragmentContent::Inline {
        box_id: None,
        fragment_index,
      },
      children,
      style,
    )
  }

  /// Returns the originating box identifier when available.
  pub fn box_id(&self) -> Option<usize> {
    match &self.content {
      FragmentContent::Block { box_id } => *box_id,
      FragmentContent::Inline { box_id, .. } => *box_id,
      FragmentContent::Text { box_id, .. } => *box_id,
      FragmentContent::Replaced { box_id, .. } => *box_id,
      FragmentContent::Line { .. }
      | FragmentContent::RunningAnchor { .. }
      | FragmentContent::FootnoteAnchor { .. } => None,
    }
  }

  /// Creates a new text fragment
  ///
  /// # Examples
  ///
  /// ```
  /// use fastrender::FragmentNode;
  /// use fastrender::Rect;
  ///
  /// let fragment = FragmentNode::new_text(
  ///     Rect::from_xywh(0.0, 0.0, 50.0, 20.0),
  ///     "Hello".to_string(),
  ///     16.0, // baseline offset
  /// );
  ///
  /// assert!(fragment.content.is_text());
  /// assert_eq!(fragment.content.text(), Some("Hello"));
  /// ```
  pub fn new_text(bounds: Rect, text: impl Into<Arc<str>>, baseline_offset: f32) -> Self {
    Self::new(
      bounds,
      FragmentContent::Text {
        text: text.into(),
        box_id: None,
        source_range: None,
        baseline_offset,
        shaped: None,
        is_marker: false,
        emphasis_offset: TextEmphasisOffset::default(),
      },
      vec![],
    )
  }

  /// Creates a new text fragment with style
  pub fn new_text_styled(
    bounds: Rect,
    text: impl Into<Arc<str>>,
    baseline_offset: f32,
    style: Arc<ComputedStyle>,
  ) -> Self {
    Self::new_with_style(
      bounds,
      FragmentContent::Text {
        text: text.into(),
        box_id: None,
        source_range: None,
        baseline_offset,
        shaped: None,
        is_marker: false,
        emphasis_offset: TextEmphasisOffset::default(),
      },
      vec![],
      style,
    )
  }

  /// Creates a new text fragment with pre-shaped runs and style
  pub fn new_text_shaped(
    bounds: Rect,
    text: impl Into<Arc<str>>,
    baseline_offset: f32,
    shaped: impl Into<Arc<Vec<ShapedRun>>>,
    style: Arc<ComputedStyle>,
  ) -> Self {
    Self::new_with_style(
      bounds,
      FragmentContent::Text {
        text: text.into(),
        box_id: None,
        source_range: None,
        baseline_offset,
        shaped: Some(shaped.into()),
        is_marker: false,
        emphasis_offset: TextEmphasisOffset::default(),
      },
      vec![],
      style,
    )
  }

  /// Creates a new line fragment
  pub fn new_line(bounds: Rect, baseline: f32, children: Vec<FragmentNode>) -> Self {
    Self::new(bounds, FragmentContent::Line { baseline }, children)
  }

  /// Creates a new replaced element fragment
  pub fn new_replaced(bounds: Rect, replaced_type: ReplacedType) -> Self {
    Self::new(
      bounds,
      FragmentContent::Replaced {
        replaced_type,
        box_id: None,
      },
      vec![],
    )
  }

  /// Creates a new running anchor fragment with a captured snapshot.
  pub fn new_running_anchor(bounds: Rect, name: String, snapshot: FragmentNode) -> Self {
    Self::new(
      bounds,
      FragmentContent::RunningAnchor {
        name: Arc::from(name),
        snapshot: Arc::new(snapshot),
      },
      vec![],
    )
  }

  /// Creates a new footnote anchor fragment with a captured snapshot.
  pub fn new_footnote_anchor(bounds: Rect, snapshot: FragmentNode) -> Self {
    Self::new(
      bounds,
      FragmentContent::FootnoteAnchor {
        snapshot: Arc::new(snapshot),
      },
      vec![],
    )
  }

  /// Gets the style for this fragment, if available
  pub fn get_style(&self) -> Option<&ComputedStyle> {
    self.style.as_ref().map(|s| s.as_ref())
  }

  /// Returns the number of children
  pub fn child_count(&self) -> usize {
    self.children.len()
  }

  /// Returns true if this fragment has no children
  pub fn is_leaf(&self) -> bool {
    self.children.is_empty()
  }

  /// Computes the bounding box of this fragment and all its children
  ///
  /// Returns the minimal rectangle that contains this fragment and all
  /// descendants in the coordinate space of this fragment's parent (the same
  /// space as `bounds`). Useful for paint invalidation and scrolling.
  ///
  /// # Examples
  ///
  /// ```
  /// use fastrender::FragmentNode;
  /// use fastrender::Rect;
  ///
  /// let child1 = FragmentNode::new_block(
  ///     Rect::from_xywh(0.0, 0.0, 50.0, 50.0),
  ///     vec![],
  /// );
  /// let child2 = FragmentNode::new_block(
  ///     Rect::from_xywh(60.0, 0.0, 50.0, 50.0),
  ///     vec![],
  /// );
  /// let parent = FragmentNode::new_block(
  ///     Rect::from_xywh(0.0, 0.0, 200.0, 100.0),
  ///     vec![child1, child2],
  /// );
  ///
  /// let bbox = parent.bounding_box();
  /// // Should encompass parent and both children
  /// assert_eq!(bbox.min_x(), 0.0);
  /// assert_eq!(bbox.max_x(), 200.0);
  /// ```
  pub fn bounding_box(&self) -> Rect {
    struct Frame<'a> {
      node: &'a FragmentNode,
      next_child: usize,
      bbox: Rect,
    }

    impl<'a> Frame<'a> {
      fn new(node: &'a FragmentNode) -> Self {
        let mut bbox = Rect::from_xywh(0.0, 0.0, node.bounds.width(), node.bounds.height());
        if let Some(borders) = &node.table_borders {
          bbox = bbox.union(borders.paint_bounds);
        }
        Self {
          node,
          next_child: 0,
          bbox,
        }
      }
    }

    // Stack-safe post-order traversal computing each node's bbox in its parent's coordinate space.
    let mut stack: Vec<Frame<'_>> = Vec::new();
    stack.push(Frame::new(self));

    while let Some(frame) = stack.last_mut() {
      if frame.next_child < frame.node.children.len() {
        let child = &frame.node.children[frame.next_child];
        frame.next_child += 1;
        stack.push(Frame::new(child));
        continue;
      }

      let bbox_in_parent_space = frame.bbox.translate(frame.node.bounds.origin);
      stack.pop();
      if let Some(parent) = stack.last_mut() {
        parent.bbox = parent.bbox.union(bbox_in_parent_space);
      } else {
        return bbox_in_parent_space;
      }
    }

    // The traversal always returns when the root frame is popped; fall back to the fragment bounds
    // to satisfy the type checker without panicking.
    self.bounds
  }

  /// Returns the logical bounds used for fragmentation decisions.
  ///
  /// When no override is set, this matches `bounds`.
  pub fn logical_bounds(&self) -> Rect {
    self.logical_override.unwrap_or(self.bounds)
  }

  /// Computes a bounding box using logical bounds for this fragment and descendants.
  pub fn logical_bounding_box(&self) -> Rect {
    let mut bbox = self.logical_bounds();
    if let Some(borders) = &self.table_borders {
      bbox = bbox.union(borders.paint_bounds.translate(Point::new(
        self.logical_bounds().x(),
        self.logical_bounds().y(),
      )));
    }
    for child in self.children.iter() {
      let child_bbox = child.logical_bounding_box();
      bbox = bbox.union(child_bbox.translate(Point::new(
        self.logical_bounds().x(),
        self.logical_bounds().y(),
      )));
    }
    bbox
  }

  /// Translates this fragment's bounds by the given offset.
  ///
  /// Offsets are applied in the coordinate space of the containing fragment.
  /// Child fragments remain in their existing local coordinate space; this
  /// preserves relative positioning within the subtree. This returns a new
  /// fragment and clones the full subtree; when shifting an owned fragment,
  /// prefer [`translate_root_in_place`] to avoid the extra clone.
  ///
  /// # Examples
  ///
  /// ```
  /// use fastrender::FragmentNode;
  /// use fastrender::{Rect, Point};
  ///
  /// let fragment = FragmentNode::new_block(
  ///     Rect::from_xywh(10.0, 20.0, 100.0, 50.0),
  ///     vec![],
  /// );
  ///
  /// let translated = fragment.translate(Point::new(5.0, 10.0));
  /// assert_eq!(translated.bounds.x(), 15.0);
  /// assert_eq!(translated.bounds.y(), 30.0);
  /// ```
  pub fn translate(&self, offset: Point) -> Self {
    let mut translated = self.clone();
    translated.translate_root_in_place(offset);
    translated
  }

  /// Translates this fragment's absolute position in place.
  ///
  /// This updates the fragment's own bounds and logical override (if present) without cloning or
  /// touching children, preserving their local coordinate space. When the fragment represents a
  /// running anchor, its snapshot is translated recursively to match the root movement. Starting
  /// style snapshots are cleared to mirror [`translate`]'s cloning semantics.
  pub fn translate_root_in_place(&mut self, offset: Point) {
    self.bounds = self.bounds.translate(offset);
    if let Some(logical) = self.logical_override {
      self.logical_override = Some(logical.translate(offset));
    }
    self.starting_style = None;
    match &mut self.content {
      FragmentContent::RunningAnchor { snapshot, .. }
      | FragmentContent::FootnoteAnchor { snapshot } => {
        Arc::make_mut(snapshot).translate_root_in_place(offset);
      }
      _ => {}
    }
  }

  /// Translates this fragment and all descendants by the given offset.
  ///
  /// This applies the offset in absolute space, adjusting every fragment in the
  /// subtree. Use sparingly; most callers should prefer [`translate`], which
  /// keeps child coordinates relative to their parent.
  pub fn translate_subtree_absolute(&self, offset: Point) -> Self {
    record_fragment_traversal(self.node_count());
    let content = match &self.content {
      FragmentContent::RunningAnchor { name, snapshot } => FragmentContent::RunningAnchor {
        name: name.clone(),
        snapshot: Arc::new(snapshot.translate_subtree_absolute(offset)),
      },
      FragmentContent::FootnoteAnchor { snapshot } => FragmentContent::FootnoteAnchor {
        snapshot: Arc::new(snapshot.translate_subtree_absolute(offset)),
      },
      other => other.clone(),
    };
    Self {
      bounds: self.bounds.translate(offset),
      block_metadata: self.block_metadata.clone(),
      logical_override: self.logical_override.map(|r| r.translate(offset)),
      content,
      table_borders: self.table_borders.clone(),
      grid_tracks: self.grid_tracks.clone(),
      baseline: self.baseline,
      children: self
        .children
        .iter()
        .map(|child| child.translate_subtree_absolute(offset))
        .collect::<Vec<_>>()
        .into(),
      style: self.style.clone(),
      starting_style: None,
      stacking_context: self.stacking_context,
      fragment_index: self.fragment_index,
      fragment_count: self.fragment_count,
      fragmentainer_index: self.fragmentainer_index,
      fragmentainer: self.fragmentainer,
      slice_info: self.slice_info,
      scroll_overflow: self.scroll_overflow,
      scrollbar_reservation: self.scrollbar_reservation,
      fragmentation: self.fragmentation.clone(),
      grid_fragmentation: self.grid_fragmentation.clone(),
    }
  }

  /// Returns true if this fragment contains the given point
  ///
  /// Checks only this fragment's bounds, not children. The point is expected to be in the
  /// coordinate space of this fragment's parent (the same space as `bounds`).
  ///
  /// # Examples
  ///
  /// ```
  /// use fastrender::FragmentNode;
  /// use fastrender::{Rect, Point};
  ///
  /// let fragment = FragmentNode::new_block(
  ///     Rect::from_xywh(10.0, 10.0, 100.0, 100.0),
  ///     vec![],
  /// );
  ///
  /// assert!(fragment.contains_point(Point::new(50.0, 50.0)));
  /// assert!(!fragment.contains_point(Point::new(5.0, 5.0)));
  /// ```
  pub fn contains_point(&self, point: Point) -> bool {
    self.bounds.contains_point(point)
  }

  /// Finds all fragments at the given point
  ///
  /// Returns fragments in reverse paint order (topmost first).
  /// This is useful for hit testing and event handling.
  ///
  /// # Examples
  ///
  /// ```
  /// use fastrender::FragmentNode;
  /// use fastrender::{Rect, Point};
  ///
  /// let child = FragmentNode::new_block(
  ///     Rect::from_xywh(20.0, 20.0, 30.0, 30.0),
  ///     vec![],
  /// );
  /// let parent = FragmentNode::new_block(
  ///     Rect::from_xywh(0.0, 0.0, 100.0, 100.0),
  ///     vec![child],
  /// );
  ///
  /// let hits = parent.fragments_at_point(Point::new(25.0, 25.0));
  /// // Should find both child and parent
  /// assert_eq!(hits.len(), 2);
  /// ```
  pub fn fragments_at_point(&self, point: Point) -> Vec<&FragmentNode> {
    #[derive(Clone, Copy)]
    enum VisitState {
      Enter,
      Exit,
    }

    struct Frame<'a> {
      node: &'a FragmentNode,
      point: Point,
      state: VisitState,
    }

    let mut hits = Vec::new();
    let mut stack = Vec::new();
    stack.push(Frame {
      node: self,
      point,
      state: VisitState::Enter,
    });

    while let Some(frame) = stack.pop() {
      match frame.state {
        VisitState::Enter => {
          stack.push(Frame {
            node: frame.node,
            point: frame.point,
            state: VisitState::Exit,
          });

          let (clip_x, clip_y) = frame
            .node
            .style
            .as_ref()
            .map(|style| {
              (
                style.overflow_x != Overflow::Visible,
                style.overflow_y != Overflow::Visible,
              )
            })
            .unwrap_or((false, false));
          let within_x = frame.point.x >= frame.node.bounds.min_x()
            && frame.point.x <= frame.node.bounds.max_x();
          let within_y = frame.point.y >= frame.node.bounds.min_y()
            && frame.point.y <= frame.node.bounds.max_y();

          if (!clip_x || within_x) && (!clip_y || within_y) {
            let local_point = Point::new(
              frame.point.x - frame.node.bounds.x(),
              frame.point.y - frame.node.bounds.y(),
            );

            for child in frame.node.children.iter() {
              stack.push(Frame {
                node: child,
                point: local_point,
                state: VisitState::Enter,
              });
            }
          }
        }
        VisitState::Exit => {
          if frame.node.contains_point(frame.point) {
            hits.push(frame.node);
          }
        }
      }
    }

    hits
  }

  /// Iterates over all fragments in paint order (depth-first, pre-order)
  ///
  /// # Examples
  ///
  /// ```
  /// use fastrender::FragmentNode;
  /// use fastrender::Rect;
  ///
  /// let child1 = FragmentNode::new_block(
  ///     Rect::from_xywh(0.0, 0.0, 50.0, 50.0),
  ///     vec![],
  /// );
  /// let child2 = FragmentNode::new_block(
  ///     Rect::from_xywh(0.0, 50.0, 50.0, 50.0),
  ///     vec![],
  /// );
  /// let parent = FragmentNode::new_block(
  ///     Rect::from_xywh(0.0, 0.0, 100.0, 100.0),
  ///     vec![child1, child2],
  /// );
  ///
  /// let all_fragments: Vec<_> = parent.iter_fragments().collect();
  /// assert_eq!(all_fragments.len(), 3); // parent + 2 children
  /// ```
  pub fn iter_fragments(&self) -> FragmentIterator<'_> {
    FragmentIterator::new(vec![self])
  }

  /// Returns a slice of direct children.
  pub fn children_ref(&self) -> &[FragmentNode] {
    self.children.as_ref()
  }

  /// Returns a mutable handle to the children, triggering copy-on-write when shared.
  pub fn children_mut(&mut self) -> &mut FragmentChildren {
    &mut self.children
  }

  /// Returns an iterator over direct children
  pub fn children(&self) -> impl Iterator<Item = &FragmentNode> {
    self.children.iter()
  }

  /// Creates a shallow clone of this fragment without cloning children.
  ///
  /// This is useful when callers need to rebuild the child list (e.g., during
  /// fragmentation) without incurring an unnecessary deep clone of the existing
  /// subtree.
  pub(crate) fn clone_without_children(&self) -> Self {
    Self {
      bounds: self.bounds,
      block_metadata: self.block_metadata.clone(),
      logical_override: self.logical_override,
      content: self.content.clone(),
      table_borders: self.table_borders.clone(),
      grid_tracks: self.grid_tracks.clone(),
      baseline: self.baseline,
      children: FragmentChildren::default(),
      style: self.style.clone(),
      starting_style: self.starting_style.clone(),
      stacking_context: self.stacking_context,
      fragment_index: self.fragment_index,
      fragment_count: self.fragment_count,
      fragmentainer_index: self.fragmentainer_index,
      fragmentainer: self.fragmentainer,
      slice_info: self.slice_info,
      scroll_overflow: self.scroll_overflow,
      scrollbar_reservation: self.scrollbar_reservation,
      fragmentation: self.fragmentation.clone(),
      grid_fragmentation: self.grid_fragmentation.clone(),
    }
  }

  /// Replaces the current children vector.
  pub fn set_children(&mut self, children: Vec<FragmentNode>) {
    self.children = children.into();
  }

  /// Recursively clones this fragment and its descendants, ensuring unique child storage.
  ///
  /// [`Clone`] on [`FragmentNode`] is intentionally shallow for fast copies during layout and
  /// painting. Use this when a caller needs to mutate the cloned tree without affecting other
  /// sharers.
  pub fn deep_clone(&self) -> Self {
    record_deep_clone();
    let content = match &self.content {
      FragmentContent::RunningAnchor { name, snapshot } => FragmentContent::RunningAnchor {
        name: name.clone(),
        snapshot: Arc::new(snapshot.deep_clone()),
      },
      FragmentContent::FootnoteAnchor { snapshot } => FragmentContent::FootnoteAnchor {
        snapshot: Arc::new(snapshot.deep_clone()),
      },
      other => other.clone(),
    };

    Self {
      bounds: self.bounds,
      block_metadata: self.block_metadata.clone(),
      logical_override: self.logical_override,
      content,
      table_borders: self.table_borders.clone(),
      grid_tracks: self.grid_tracks.clone(),
      baseline: self.baseline,
      children: self.children.deep_clone(),
      style: self.style.clone(),
      starting_style: self.starting_style.clone(),
      stacking_context: self.stacking_context,
      fragment_index: self.fragment_index,
      fragment_count: self.fragment_count,
      fragmentainer_index: self.fragmentainer_index,
      fragmentainer: self.fragmentainer,
      slice_info: self.slice_info,
      scroll_overflow: self.scroll_overflow,
      scrollbar_reservation: self.scrollbar_reservation,
      fragmentation: self.fragmentation.clone(),
      grid_fragmentation: self.grid_fragmentation.clone(),
    }
  }

  /// Counts the total number of nodes in this subtree, including nested running anchors.
  pub fn node_count(&self) -> usize {
    let anchor_count = match &self.content {
      FragmentContent::RunningAnchor { snapshot, .. } => snapshot.node_count(),
      FragmentContent::FootnoteAnchor { snapshot } => snapshot.node_count(),
      _ => 0,
    };
    1 + anchor_count
      + self
        .children
        .iter()
        .map(FragmentNode::node_count)
        .sum::<usize>()
  }
}

/// Metadata describing nested fragmentation contexts (e.g., multi-column containers).
#[derive(Debug, Clone)]
pub struct FragmentationInfo {
  pub column_count: usize,
  pub column_gap: f32,
  pub column_width: f32,
  pub flow_height: f32,
}

/// Iterator over fragments in paint order (depth-first, pre-order)
pub struct FragmentIterator<'a> {
  stack: Vec<&'a FragmentNode>,
}

impl<'a> FragmentIterator<'a> {
  pub fn new(stack: Vec<&'a FragmentNode>) -> Self {
    Self { stack }
  }
}

impl<'a> Iterator for FragmentIterator<'a> {
  type Item = &'a FragmentNode;

  fn next(&mut self) -> Option<Self::Item> {
    if let Some(fragment) = self.stack.pop() {
      // Push children in reverse order so they're processed in correct order
      for child in fragment.children.iter().rev() {
        self.stack.push(child);
      }
      Some(fragment)
    } else {
      None
    }
  }
}

/// Identifies which root fragment was hit by [`FragmentTree::hit_test_path`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HitTestRoot {
  /// The primary fragment root stored in [`FragmentTree::root`].
  Root,
  /// One of [`FragmentTree::additional_fragments`].
  Additional(usize),
}

/// The fragment tree - output of layout
///
/// Contains the root fragment and provides tree-level operations.
///
/// # Examples
///
/// ```
/// use fastrender::{FragmentTree, FragmentNode};
/// use fastrender::Rect;
///
/// let root = FragmentNode::new_block(
///     Rect::from_xywh(0.0, 0.0, 800.0, 600.0),
///     vec![],
/// );
/// let tree = FragmentTree::new(root);
///
/// assert_eq!(tree.viewport_size().width, 800.0);
/// ```
#[derive(Debug, Clone)]
pub struct FragmentTree {
  /// The root fragment (usually the viewport or document root)
  pub root: FragmentNode,

  /// Additional root fragments produced by pagination/column fragmentation.
  /// The first fragment is always stored in `root` for backwards compatibility.
  pub additional_fragments: Vec<FragmentNode>,

  /// Collected @keyframes rules active for this tree.
  pub keyframes: HashMap<String, KeyframesRule>,

  /// Persistent CSS transition state carried across layout/style recomputations.
  ///
  /// When present, paint-time transition sampling (`animation::apply_transitions`) uses this state
  /// to animate between previous and current computed styles across multi-frame renders.
  ///
  /// Stored in an `Arc` so cloning a [`FragmentTree`] (which is designed to be cheap via structural
  /// sharing) does not deep-clone the transition state.
  pub transition_state: Option<Arc<TransitionState>>,

  /// SVG filter definitions serialized from the DOM (document-level registry).
  pub svg_filter_defs: Option<Arc<HashMap<String, String>>>,

  /// SVG defs elements (by id) serialized from the DOM (document-level registry).
  pub svg_id_defs: Option<Arc<HashMap<String, String>>>,

  /// The viewport size (may differ from root fragment bounds)
  viewport: Option<Size>,

  /// Scroll snap and overflow metadata derived from layout.
  pub scroll_metadata: Option<ScrollMetadata>,
}

impl FragmentTree {
  /// Creates a new fragment tree with the given root
  pub fn new(root: FragmentNode) -> Self {
    Self {
      root,
      additional_fragments: Vec::new(),
      keyframes: HashMap::new(),
      transition_state: None,
      svg_filter_defs: None,
      svg_id_defs: None,
      viewport: None,
      scroll_metadata: None,
    }
  }

  /// Creates a new fragment tree with explicit viewport size
  ///
  /// Use this when the viewport size should be tracked separately
  /// from the root fragment's bounds (e.g., for scrollable content).
  pub fn with_viewport(root: FragmentNode, viewport: Size) -> Self {
    Self {
      root,
      additional_fragments: Vec::new(),
      keyframes: HashMap::new(),
      transition_state: None,
      svg_filter_defs: None,
      svg_id_defs: None,
      viewport: Some(viewport),
      scroll_metadata: None,
    }
  }

  /// Creates a fragment tree from multiple root fragments (e.g., pages/columns).
  ///
  /// The first fragment is stored in `root`; the remainder are placed in
  /// `additional_fragments`.
  pub fn from_fragments(mut roots: Vec<FragmentNode>, viewport: Size) -> Self {
    let root = roots
      .drain(0..1)
      .next()
      .expect("at least one fragment root required");
    Self {
      root,
      additional_fragments: roots,
      keyframes: HashMap::new(),
      transition_state: None,
      svg_filter_defs: None,
      svg_id_defs: None,
      viewport: Some(viewport),
      scroll_metadata: None,
    }
  }

  /// Returns the viewport size
  ///
  /// If an explicit viewport was set, returns that; otherwise returns
  /// the root fragment's size.
  pub fn viewport_size(&self) -> Size {
    self.viewport.unwrap_or(self.root.bounds.size)
  }

  /// Returns true when the tree tracks an explicit viewport size separate from the root fragment.
  pub fn has_explicit_viewport(&self) -> bool {
    self.viewport.is_some()
  }

  /// Computes the total bounding box of all content
  pub fn content_size(&self) -> Rect {
    let mut bbox = self.root.bounding_box();
    for root in &self.additional_fragments {
      bbox = bbox.union(root.bounding_box());
    }
    bbox
  }

  /// Finds all fragments at the given point
  pub fn hit_test(&self, point: Point) -> Vec<&FragmentNode> {
    let mut hits = self.root.fragments_at_point(point);
    for root in &self.additional_fragments {
      hits.extend(root.fragments_at_point(point));
    }
    hits
  }

  /// Returns the child-index path to the topmost fragment at `point`.
  ///
  /// The returned path is equivalent to the fragment returned by
  /// `self.hit_test(point).first()`, but encoded as an ancestor chain of child indices. The path is
  /// relative to the returned [`HitTestRoot`]. An empty path means the chosen root fragment itself.
  ///
  /// Hit testing mirrors [`FragmentNode::fragments_at_point`] semantics:
  /// - Later siblings are considered "on top" (reverse child order).
  /// - Deepest descendants are preferred.
  /// - Overflow clipping only applies on axes where `overflow_{x,y} != visible`.
  ///
  /// Non-finite coordinates (NaN / ±inf) are treated as "no hit".
  pub fn hit_test_path(&self, point: Point) -> Option<(HitTestRoot, Vec<usize>)> {
    if !point.x.is_finite() || !point.y.is_finite() {
      return None;
    }

    if let Some(path) = Self::hit_test_path_within_root(&self.root, point) {
      return Some((HitTestRoot::Root, path));
    }

    for (idx, root) in self.additional_fragments.iter().enumerate() {
      if let Some(path) = Self::hit_test_path_within_root(root, point) {
        return Some((HitTestRoot::Additional(idx), path));
      }
    }

    None
  }

  fn hit_test_path_within_root(root: &FragmentNode, point: Point) -> Option<Vec<usize>> {
    struct Frame<'a> {
      node: &'a FragmentNode,
      point: Point,
      next_child: usize,
      index_in_parent: Option<usize>,
    }

    fn should_descend(node: &FragmentNode, point: Point) -> bool {
      let (clip_x, clip_y) = node
        .style
        .as_ref()
        .map(|style| {
          (
            style.overflow_x != Overflow::Visible,
            style.overflow_y != Overflow::Visible,
          )
        })
        .unwrap_or((false, false));

      let within_x = point.x >= node.bounds.min_x() && point.x <= node.bounds.max_x();
      let within_y = point.y >= node.bounds.min_y() && point.y <= node.bounds.max_y();

      (!clip_x || within_x) && (!clip_y || within_y)
    }

    fn make_frame<'a>(
      node: &'a FragmentNode,
      point: Point,
      index_in_parent: Option<usize>,
    ) -> Frame<'a> {
      let next_child = if should_descend(node, point) {
        node.children.len()
      } else {
        0
      };
      Frame {
        node,
        point,
        next_child,
        index_in_parent,
      }
    }

    // Stack-safe DFS that mirrors `FragmentNode::fragments_at_point` ordering. The stack always
    // contains the current ancestor chain, so the path can be materialized from indices stored in
    // each frame without cloning partial paths.
    let mut stack = Vec::new();
    stack.push(make_frame(root, point, None));

    while !stack.is_empty() {
      let child = {
        let frame = stack
          .last_mut()
          .expect("stack non-empty in hit_test_path_within_root");
        if frame.next_child == 0 {
          None
        } else {
          frame.next_child -= 1;
          let child_index = frame.next_child;
          let node = frame.node;
          let local_point = Point::new(
            frame.point.x - node.bounds.x(),
            frame.point.y - node.bounds.y(),
          );
          let child = &node.children[child_index];
          Some(make_frame(child, local_point, Some(child_index)))
        }
      };

      if let Some(child) = child {
        stack.push(child);
        continue;
      }

      let is_hit = {
        let frame = stack
          .last()
          .expect("stack non-empty in hit_test_path_within_root");
        frame.node.contains_point(frame.point)
      };

      if is_hit {
        let path = stack
          .iter()
          .filter_map(|frame| frame.index_in_parent)
          .collect();
        return Some(path);
      }

      stack.pop();
    }

    None
  }

  /// Returns an iterator over all fragments in paint order
  pub fn iter_fragments(&self) -> FragmentIterator<'_> {
    let mut stack: Vec<&FragmentNode> = Vec::new();
    for root in self.additional_fragments.iter().rev() {
      stack.push(root);
    }
    stack.push(&self.root);
    FragmentIterator::new(stack)
  }

  /// Counts total number of fragments in the tree
  pub fn fragment_count(&self) -> usize {
    self.iter_fragments().count()
  }

  /// Ensures scroll metadata (overflow bounds and snap targets) are computed.
  ///
  /// Layout populates this for renderer-produced trees, but helper code and
  /// tests that manually construct fragment trees can call this to enable
  /// scroll snapping without re-running layout.
  pub fn ensure_scroll_metadata(&mut self) {
    if self.scroll_metadata.is_none() {
      self.scroll_metadata = Some(crate::scroll::build_scroll_metadata(self));
    }
  }

  /// Propagates starting-style snapshots from the box tree onto fragments.
  ///
  /// Fragmentation can clone fragments and drop their starting-style handles; this
  /// function reattaches the snapshots so transition sampling has access to both
  /// the starting and final computed values.
  pub(crate) fn attach_starting_styles_from_boxes(&mut self, box_tree: &BoxTree) {
    let mut map: HashMap<usize, Arc<ComputedStyle>> = HashMap::new();
    fn collect(node: &BoxNode, out: &mut HashMap<usize, Arc<ComputedStyle>>) {
      if let Some(start) = &node.starting_style {
        out.insert(node.id, start.clone());
      }
      for child in node.children.iter() {
        collect(child, out);
      }
      if let Some(body) = node.footnote_body.as_deref() {
        collect(body, out);
      }
    }
    collect(&box_tree.root, &mut map);
    if map.is_empty() {
      return;
    }

    fn apply(fragment: &mut FragmentNode, map: &HashMap<usize, Arc<ComputedStyle>>) {
      if fragment.starting_style.is_none() {
        if let Some(id) = fragment.box_id() {
          if let Some(start) = map.get(&id) {
            fragment.starting_style = Some(start.clone());
          }
        }
      }
      match &mut fragment.content {
        FragmentContent::RunningAnchor { snapshot, .. }
        | FragmentContent::FootnoteAnchor { snapshot } => {
          apply(Arc::make_mut(snapshot), map);
        }
        _ => {}
      }
      for child in fragment.children_mut() {
        apply(child, map);
      }
    }

    apply(&mut self.root, &map);
    for root in &mut self.additional_fragments {
      apply(root, &map);
    }
  }
}

impl fmt::Display for FragmentTree {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    write!(f, "FragmentTree(fragments: {})", self.fragment_count())
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::layout::fragmentation::{fragment_tree as split_fragment_tree, FragmentationOptions};
  use crate::style::ComputedStyle;
  use crate::text::pipeline::ShapedRun;
  use crate::tree::box_tree::CrossOriginAttribute;
  use std::sync::Arc;

  // Constructor tests
  #[test]
  fn test_new_block_fragment() {
    let fragment = FragmentNode::new_block(Rect::from_xywh(10.0, 20.0, 100.0, 50.0), vec![]);

    assert_eq!(fragment.bounds.x(), 10.0);
    assert_eq!(fragment.bounds.y(), 20.0);
    assert!(fragment.content.is_block());
    assert_eq!(fragment.child_count(), 0);
  }

  #[test]
  fn test_new_text_fragment() {
    let fragment = FragmentNode::new_text(
      Rect::from_xywh(0.0, 0.0, 50.0, 20.0),
      "Hello World".to_string(),
      16.0,
    );

    assert!(fragment.content.is_text());
    assert_eq!(fragment.content.text(), Some("Hello World"));
  }

  #[test]
  fn test_new_inline_fragment() {
    let fragment = FragmentNode::new_inline(Rect::from_xywh(0.0, 0.0, 100.0, 20.0), 0, vec![]);

    assert!(fragment.content.is_inline());
  }

  #[test]
  fn test_new_line_fragment() {
    let text = FragmentNode::new_text(
      Rect::from_xywh(0.0, 0.0, 50.0, 20.0),
      "Text".to_string(),
      16.0,
    );
    let line = FragmentNode::new_line(Rect::from_xywh(0.0, 0.0, 200.0, 20.0), 16.0, vec![text]);

    assert!(line.content.is_line());
    assert_eq!(line.child_count(), 1);
  }

  // Bounding box tests
  #[test]
  fn test_bounding_box_single() {
    let fragment = FragmentNode::new_block(Rect::from_xywh(10.0, 20.0, 100.0, 50.0), vec![]);

    let bbox = fragment.bounding_box();
    assert_eq!(bbox, fragment.bounds);
  }

  #[test]
  fn test_bounding_box_with_children() {
    let child1 = FragmentNode::new_block(Rect::from_xywh(0.0, 0.0, 50.0, 50.0), vec![]);
    let child2 = FragmentNode::new_block(Rect::from_xywh(60.0, 0.0, 50.0, 50.0), vec![]);
    let parent = FragmentNode::new_block(
      Rect::from_xywh(0.0, 0.0, 200.0, 100.0),
      vec![child1, child2],
    );

    let bbox = parent.bounding_box();
    assert_eq!(bbox.min_x(), 0.0);
    assert_eq!(bbox.max_x(), 200.0);
    assert_eq!(bbox.min_y(), 0.0);
    assert_eq!(bbox.max_y(), 100.0);
  }

  #[test]
  fn test_bounding_box_children_apart_spans_both() {
    let child1 = FragmentNode::new_block(Rect::from_xywh(0.0, 0.0, 10.0, 10.0), vec![]);
    let child2 = FragmentNode::new_block(Rect::from_xywh(100.0, 50.0, 10.0, 10.0), vec![]);
    // Keep the parent small so the resulting bbox depends on the children.
    let parent = FragmentNode::new_block(Rect::from_xywh(0.0, 0.0, 1.0, 1.0), vec![child1, child2]);

    let bbox = parent.bounding_box();
    assert_eq!(bbox.min_x(), 0.0);
    assert_eq!(bbox.max_x(), 110.0);
    assert_eq!(bbox.min_y(), 0.0);
    assert_eq!(bbox.max_y(), 60.0);
  }

  #[test]
  fn test_bounding_box_includes_overflowing_child() {
    let child = FragmentNode::new_block(Rect::from_xywh(-50.0, -25.0, 5.0, 5.0), vec![]);
    let parent = FragmentNode::new_block(Rect::from_xywh(0.0, 0.0, 10.0, 10.0), vec![child]);

    let bbox = parent.bounding_box();
    assert_eq!(bbox.min_x(), -50.0);
    assert_eq!(bbox.min_y(), -25.0);
    assert_eq!(bbox.max_x(), 10.0);
    assert_eq!(bbox.max_y(), 10.0);
  }

  #[test]
  fn test_bounding_box_unions_table_borders_paint_bounds() {
    let mut fragment = FragmentNode::new_block(Rect::from_xywh(10.0, 20.0, 100.0, 50.0), vec![]);
    fragment.table_borders = Some(Arc::new(TableCollapsedBorders {
      column_count: 0,
      row_count: 0,
      column_line_positions: Vec::new(),
      row_line_positions: Vec::new(),
      vertical_borders: Vec::new(),
      horizontal_borders: Vec::new(),
      corner_borders: Vec::new(),
      vertical_line_base: Vec::new(),
      horizontal_line_base: Vec::new(),
      paint_bounds: Rect::from_xywh(-5.0, -5.0, 110.0, 60.0),
    }));

    let bbox = fragment.bounding_box();
    assert_eq!(bbox.min_x(), 5.0);
    assert_eq!(bbox.min_y(), 15.0);
    assert_eq!(bbox.max_x(), 115.0);
    assert_eq!(bbox.max_y(), 75.0);
  }

  #[test]
  fn test_bounding_box_deep_tree_stack_safe() {
    // Keep this large enough to overflow with recursion in the test harness thread stack.
    let mut fragment = FragmentNode::new_block(Rect::from_xywh(0.0, 0.0, 1.0, 1.0), vec![]);
    for _ in 0..30_000 {
      fragment = FragmentNode::new_block(Rect::from_xywh(0.0, 0.0, 1.0, 1.0), vec![fragment]);
    }

    let bbox = fragment.bounding_box();
    assert_eq!(bbox.width(), 1.0);
    assert_eq!(bbox.height(), 1.0);

    // Dropping a deep `FragmentNode` chain is recursive and can overflow the test harness thread
    // stack, so tear it down iteratively.
    let mut node = fragment;
    loop {
      let mut children = std::mem::take(&mut node.children);
      let mut iter = children.into_iter();
      if let Some(child) = iter.next() {
        node = child;
      } else {
        break;
      }
    }
  }

  #[test]
  fn test_bounding_box_nested() {
    let grandchild = FragmentNode::new_block(Rect::from_xywh(150.0, 150.0, 50.0, 50.0), vec![]);
    let child =
      FragmentNode::new_block(Rect::from_xywh(50.0, 50.0, 100.0, 100.0), vec![grandchild]);
    let parent = FragmentNode::new_block(Rect::from_xywh(0.0, 0.0, 200.0, 200.0), vec![child]);

    let bbox = parent.bounding_box();
    // Should include grandchild at (150, 150) with size (50, 50)
    assert_eq!(bbox.min_x(), 0.0);
    assert_eq!(bbox.max_x(), 250.0);
    assert_eq!(bbox.max_y(), 250.0);
  }

  #[test]
  fn test_bounding_box_accumulates_offsets() {
    let grandchild = FragmentNode::new_block(Rect::from_xywh(0.0, 30.0, 10.0, 5.0), vec![]);
    let child = FragmentNode::new_block(Rect::from_xywh(0.0, 50.0, 20.0, 20.0), vec![grandchild]);
    let parent = FragmentNode::new_block(Rect::from_xywh(0.0, 0.0, 40.0, 40.0), vec![child]);

    let bbox = parent.bounding_box();
    assert_eq!(bbox.min_y(), 0.0);
    assert_eq!(bbox.max_y(), 85.0);
  }

  #[test]
  fn test_bounding_box_with_parent_offset() {
    let child = FragmentNode::new_block(Rect::from_xywh(10.0, 10.0, 30.0, 30.0), vec![]);
    let parent = FragmentNode::new_block(Rect::from_xywh(50.0, 50.0, 20.0, 20.0), vec![child]);

    let bbox = parent.bounding_box();
    assert_eq!(bbox.min_x(), 50.0);
    assert_eq!(bbox.min_y(), 50.0);
    assert_eq!(bbox.max_x(), 90.0);
    assert_eq!(bbox.max_y(), 90.0);
  }

  // Translation tests
  #[test]
  fn test_translate_single() {
    let fragment = FragmentNode::new_block(Rect::from_xywh(10.0, 20.0, 100.0, 50.0), vec![]);

    let translated = fragment.translate(Point::new(5.0, 10.0));
    assert_eq!(translated.bounds.x(), 15.0);
    assert_eq!(translated.bounds.y(), 30.0);
    assert_eq!(translated.bounds.width(), 100.0);
  }

  #[test]
  fn test_translate_preserves_children_positions() {
    let child = FragmentNode::new_block(Rect::from_xywh(10.0, 10.0, 30.0, 30.0), vec![]);
    let parent = FragmentNode::new_block(Rect::from_xywh(0.0, 0.0, 100.0, 100.0), vec![child]);

    let translated = parent.translate(Point::new(5.0, 0.0));
    assert_eq!(translated.bounds.x(), 5.0);
    assert_eq!(translated.bounds.y(), 0.0);
    // Children remain in the parent's coordinate space.
    assert_eq!(translated.children[0].bounds.x(), 10.0);
    assert_eq!(translated.children[0].bounds.y(), 10.0);
  }

  #[test]
  fn test_translate_subtree_absolute_shifts_descendants() {
    let child = FragmentNode::new_block(Rect::from_xywh(10.0, 10.0, 30.0, 30.0), vec![]);
    let parent = FragmentNode::new_block(Rect::from_xywh(0.0, 0.0, 100.0, 100.0), vec![child]);

    let translated = parent.translate_subtree_absolute(Point::new(50.0, 50.0));
    assert_eq!(translated.bounds.x(), 50.0);
    assert_eq!(translated.bounds.y(), 50.0);
    assert_eq!(translated.children[0].bounds.x(), 60.0);
    assert_eq!(translated.children[0].bounds.y(), 60.0);
  }

  #[test]
  fn test_fragmentation_keeps_children_and_translates_running_anchor_snapshot() {
    let snapshot_child = FragmentNode::new_block(Rect::from_xywh(3.0, 4.0, 2.0, 2.0), Vec::new());
    let snapshot =
      FragmentNode::new_block(Rect::from_xywh(5.0, 5.0, 10.0, 10.0), vec![snapshot_child]);
    let mut root = FragmentNode::new_running_anchor(
      Rect::from_xywh(0.0, 0.0, 20.0, 120.0),
      "running".to_string(),
      snapshot,
    );
    let child_first_fragment =
      FragmentNode::new_block(Rect::from_xywh(0.0, 10.0, 5.0, 5.0), Vec::new());
    let child_second_fragment =
      FragmentNode::new_block(Rect::from_xywh(0.0, 70.0, 5.0, 5.0), Vec::new());
    root.set_children(vec![child_first_fragment, child_second_fragment]);

    let options = FragmentationOptions::new(60.0).with_gap(10.0);
    let fragments = split_fragment_tree(&root, &options).expect("split fragments");
    assert_eq!(fragments.len(), 2);

    assert_eq!(fragments[0].bounds.y(), 0.0);
    assert_eq!(fragments[1].bounds.y(), 70.0);

    assert_eq!(fragments[0].children.len(), 1);
    assert_eq!(fragments[0].children[0].bounds.y(), 10.0);

    let second = &fragments[1];
    assert_eq!(second.children.len(), 1);
    assert_eq!(second.children[0].bounds.y(), 10.0);

    if let FragmentContent::RunningAnchor { snapshot, .. } = &second.content {
      assert_eq!(snapshot.bounds.y(), 75.0);
      assert_eq!(snapshot.children.len(), 1);
      assert_eq!(snapshot.children[0].bounds.y(), 4.0);
    } else {
      panic!("second fragment should remain a running anchor");
    }
  }

  // Hit testing tests
  #[test]
  fn test_contains_point() {
    let fragment = FragmentNode::new_block(Rect::from_xywh(10.0, 10.0, 100.0, 100.0), vec![]);

    assert!(fragment.contains_point(Point::new(50.0, 50.0)));
    assert!(fragment.contains_point(Point::new(10.0, 10.0))); // Boundary
    assert!(fragment.contains_point(Point::new(110.0, 110.0))); // Boundary
    assert!(!fragment.contains_point(Point::new(5.0, 5.0)));
    assert!(!fragment.contains_point(Point::new(120.0, 120.0)));
  }

  #[test]
  fn test_fragments_at_point_single() {
    let fragment = FragmentNode::new_block(Rect::from_xywh(0.0, 0.0, 100.0, 100.0), vec![]);

    let hits = fragment.fragments_at_point(Point::new(50.0, 50.0));
    assert_eq!(hits.len(), 1);
  }

  #[test]
  fn test_fragments_at_point_with_children() {
    let child = FragmentNode::new_block(Rect::from_xywh(20.0, 20.0, 30.0, 30.0), vec![]);
    let parent = FragmentNode::new_block(Rect::from_xywh(0.0, 0.0, 100.0, 100.0), vec![child]);

    // Point in child
    let hits = parent.fragments_at_point(Point::new(25.0, 25.0));
    assert_eq!(hits.len(), 2); // Both child and parent
    assert!(std::ptr::eq(hits[0], &parent.children[0]));
    assert!(std::ptr::eq(hits[1], &parent));

    // Point only in parent
    let hits = parent.fragments_at_point(Point::new(5.0, 5.0));
    assert_eq!(hits.len(), 1); // Only parent
    assert!(std::ptr::eq(hits[0], &parent));
  }

  #[test]
  fn test_fragments_at_point_with_translated_parent() {
    let child = FragmentNode::new_block(Rect::from_xywh(10.0, 10.0, 10.0, 10.0), vec![]);
    let parent = FragmentNode::new_block(Rect::from_xywh(100.0, 100.0, 50.0, 50.0), vec![child]);

    // Global point inside both parent and child after applying parent origin.
    let hits = parent.fragments_at_point(Point::new(110.0, 110.0));
    assert_eq!(hits.len(), 2);
  }

  #[test]
  fn test_fragments_at_point_overlapping() {
    let child1 = FragmentNode::new_block(Rect::from_xywh(10.0, 10.0, 50.0, 50.0), vec![]);
    let child2 = FragmentNode::new_block(Rect::from_xywh(30.0, 30.0, 50.0, 50.0), vec![]);
    let parent = FragmentNode::new_block(
      Rect::from_xywh(0.0, 0.0, 100.0, 100.0),
      vec![child1, child2],
    );

    // Point in overlapping region
    let hits = parent.fragments_at_point(Point::new(40.0, 40.0));
    assert_eq!(hits.len(), 3); // Both children and parent
    assert!(std::ptr::eq(hits[0], &parent.children[1]));
    assert!(std::ptr::eq(hits[1], &parent.children[0]));
    assert!(std::ptr::eq(hits[2], &parent));
  }

  #[test]
  fn test_fragments_at_point_deep_chain_stack_safe() {
    const DEPTH: usize = 20_000;
    let handle = std::thread::Builder::new()
      .stack_size(256 * 1024)
      .spawn(|| {
        let mut root = FragmentNode::new_block(Rect::from_xywh(0.0, 0.0, 1.0, 1.0), vec![]);
        for _ in 0..DEPTH {
          root = FragmentNode::new_block(Rect::from_xywh(0.0, 0.0, 1.0, 1.0), vec![root]);
        }

        let hits_len = {
          let hits = root.fragments_at_point(Point::new(0.5, 0.5));
          hits.len()
        };
        assert_eq!(hits_len, DEPTH + 1);

        std::mem::forget(root);
      })
      .expect("spawn hit-test thread");
    handle.join().expect("hit-test thread join");
  }

  #[test]
  fn test_fragments_at_point_overflow_hidden_clips_both_axes() {
    let child = FragmentNode::new_block(Rect::from_xywh(90.0, 90.0, 30.0, 30.0), vec![]);

    let mut style = ComputedStyle::default();
    style.overflow_x = Overflow::Hidden;
    style.overflow_y = Overflow::Hidden;

    let parent = FragmentNode::new_block_styled(
      Rect::from_xywh(0.0, 0.0, 100.0, 100.0),
      vec![child],
      Arc::new(style),
    );

    // Point is inside the child, but outside the parent bounds.
    let hits = parent.fragments_at_point(Point::new(110.0, 110.0));
    assert!(hits.is_empty());
  }

  #[test]
  fn test_fragments_at_point_overflow_clips_x_only() {
    let child = FragmentNode::new_block(Rect::from_xywh(10.0, 90.0, 50.0, 50.0), vec![]);

    let mut style = ComputedStyle::default();
    style.overflow_x = Overflow::Hidden;
    style.overflow_y = Overflow::Visible;

    let parent = FragmentNode::new_block_styled(
      Rect::from_xywh(0.0, 0.0, 100.0, 100.0),
      vec![child],
      Arc::new(style),
    );

    // Point is outside the parent Y range, but inside its X range.
    let hits = parent.fragments_at_point(Point::new(20.0, 120.0));
    assert_eq!(hits.len(), 1);
    assert!(std::ptr::eq(hits[0], &parent.children[0]));
  }

  #[test]
  fn test_fragments_at_point_overflow_clips_y_only() {
    let child = FragmentNode::new_block(Rect::from_xywh(90.0, 10.0, 50.0, 50.0), vec![]);

    let mut style = ComputedStyle::default();
    style.overflow_x = Overflow::Visible;
    style.overflow_y = Overflow::Hidden;

    let parent = FragmentNode::new_block_styled(
      Rect::from_xywh(0.0, 0.0, 100.0, 100.0),
      vec![child],
      Arc::new(style),
    );

    // Point is outside the parent X range, but inside its Y range.
    let hits = parent.fragments_at_point(Point::new(120.0, 20.0));
    assert_eq!(hits.len(), 1);
    assert!(std::ptr::eq(hits[0], &parent.children[0]));
  }

  #[test]
  fn test_hit_test_path_overlapping_prefers_later_sibling() {
    let child1 = FragmentNode::new_block(Rect::from_xywh(10.0, 10.0, 50.0, 50.0), vec![]);
    let child2 = FragmentNode::new_block(Rect::from_xywh(30.0, 30.0, 50.0, 50.0), vec![]);
    let root = FragmentNode::new_block(
      Rect::from_xywh(0.0, 0.0, 100.0, 100.0),
      vec![child1, child2],
    );
    let tree = FragmentTree::new(root);
    let point = Point::new(40.0, 40.0);

    let hits = tree.hit_test(point);
    assert!(std::ptr::eq(hits[0], &tree.root.children[1]));
    assert_eq!(
      tree.hit_test_path(point),
      Some((HitTestRoot::Root, vec![1]))
    );
  }

  #[test]
  fn test_hit_test_path_overflow_clips_x_only() {
    let child = FragmentNode::new_block(Rect::from_xywh(10.0, 90.0, 50.0, 50.0), vec![]);

    let mut style = ComputedStyle::default();
    style.overflow_x = Overflow::Hidden;
    style.overflow_y = Overflow::Visible;

    let root = FragmentNode::new_block_styled(
      Rect::from_xywh(0.0, 0.0, 100.0, 100.0),
      vec![child],
      Arc::new(style),
    );
    let tree = FragmentTree::new(root);

    assert_eq!(
      tree.hit_test_path(Point::new(20.0, 120.0)),
      Some((HitTestRoot::Root, vec![0]))
    );
  }

  #[test]
  fn test_hit_test_path_overflow_clips_y_only() {
    let child = FragmentNode::new_block(Rect::from_xywh(90.0, 10.0, 50.0, 50.0), vec![]);

    let mut style = ComputedStyle::default();
    style.overflow_x = Overflow::Visible;
    style.overflow_y = Overflow::Hidden;

    let root = FragmentNode::new_block_styled(
      Rect::from_xywh(0.0, 0.0, 100.0, 100.0),
      vec![child],
      Arc::new(style),
    );
    let tree = FragmentTree::new(root);

    assert_eq!(
      tree.hit_test_path(Point::new(120.0, 20.0)),
      Some((HitTestRoot::Root, vec![0]))
    );
  }

  #[test]
  fn test_hit_test_path_deep_chain_stack_safe() {
    const DEPTH: usize = 20_000;
    let handle = std::thread::Builder::new()
      .stack_size(256 * 1024)
      .spawn(|| {
        let mut root = FragmentNode::new_block(Rect::from_xywh(0.0, 0.0, 1.0, 1.0), vec![]);
        for _ in 0..DEPTH {
          root = FragmentNode::new_block(Rect::from_xywh(0.0, 0.0, 1.0, 1.0), vec![root]);
        }

        let tree = FragmentTree::new(root);
        let (hit_root, path) = tree
          .hit_test_path(Point::new(0.5, 0.5))
          .expect("expected a hit path");
        assert_eq!(hit_root, HitTestRoot::Root);
        assert_eq!(path.len(), DEPTH);
        assert!(path.iter().all(|&idx| idx == 0));

        std::mem::forget(tree);
      })
      .expect("spawn hit-test path thread");
    handle.join().expect("hit-test path thread join");
  }

  #[test]
  fn test_hit_test_nested_offsets() {
    let grandchild = FragmentNode::new_block(Rect::from_xywh(5.0, 30.0, 10.0, 10.0), vec![]);
    let child = FragmentNode::new_block(Rect::from_xywh(10.0, 50.0, 40.0, 40.0), vec![grandchild]);
    let root = FragmentNode::new_block(Rect::from_xywh(0.0, 0.0, 200.0, 200.0), vec![child]);
    let tree = FragmentTree::new(root);

    // Point should hit grandchild (5+10+?, etc.)
    let hits = tree.hit_test(Point::new(20.0, 90.0));
    assert_eq!(hits.len(), 3);
    assert!(std::ptr::eq(hits[0], &tree.root.children[0].children[0]));
  }

  // Tree traversal tests
  #[test]
  fn test_iter_fragments_single() {
    let fragment = FragmentNode::new_block(Rect::from_xywh(0.0, 0.0, 100.0, 100.0), vec![]);

    let count = fragment.iter_fragments().count();
    assert_eq!(count, 1);
  }

  #[test]
  fn test_iter_fragments_with_children() {
    let child1 = FragmentNode::new_block(Rect::from_xywh(0.0, 0.0, 50.0, 50.0), vec![]);
    let child2 = FragmentNode::new_block(Rect::from_xywh(0.0, 50.0, 50.0, 50.0), vec![]);
    let parent = FragmentNode::new_block(
      Rect::from_xywh(0.0, 0.0, 100.0, 100.0),
      vec![child1, child2],
    );

    let fragments: Vec<_> = parent.iter_fragments().collect();
    assert_eq!(fragments.len(), 3); // parent + 2 children
                                    // First should be parent (pre-order)
    assert_eq!(fragments[0].bounds, parent.bounds);
  }

  // FragmentTree tests
  #[test]
  fn test_fragment_tree_creation() {
    let root = FragmentNode::new_block(Rect::from_xywh(0.0, 0.0, 800.0, 600.0), vec![]);
    let tree = FragmentTree::new(root);

    assert_eq!(tree.viewport_size().width, 800.0);
    assert_eq!(tree.viewport_size().height, 600.0);
  }

  #[test]
  fn test_fragment_tree_hit_test() {
    let child = FragmentNode::new_block(Rect::from_xywh(100.0, 100.0, 50.0, 50.0), vec![]);
    let root = FragmentNode::new_block(Rect::from_xywh(0.0, 0.0, 800.0, 600.0), vec![child]);
    let tree = FragmentTree::new(root);

    let hits = tree.hit_test(Point::new(120.0, 120.0));
    assert_eq!(hits.len(), 2); // child and root
  }

  #[test]
  fn test_fragmentation_stacks_roots_without_offsetting_children() {
    let child = FragmentNode::new_block(Rect::from_xywh(0.0, 60.0, 10.0, 10.0), vec![]);
    let root = FragmentNode::new_block(Rect::from_xywh(0.0, 0.0, 20.0, 120.0), vec![child]);
    let options = FragmentationOptions::new(60.0).with_gap(20.0);

    let fragments = split_fragment_tree(&root, &options).unwrap();
    assert_eq!(fragments.len(), 2);

    let second = &fragments[1];
    assert_eq!(second.bounds.y(), 80.0); // 60 fragment height + 20 gap
    assert_eq!(second.children.len(), 1);
    assert_eq!(second.children[0].bounds.y(), 0.0); // child clipped into second fragment starts at top
    assert!((second.bounds.y() + second.children[0].bounds.y() - 80.0).abs() < 0.001);
  }

  #[test]
  fn test_fragment_tree_count() {
    let child1 = FragmentNode::new_block(Rect::from_xywh(0.0, 0.0, 100.0, 100.0), vec![]);
    let child2 = FragmentNode::new_block(Rect::from_xywh(0.0, 100.0, 100.0, 100.0), vec![]);
    let root = FragmentNode::new_block(
      Rect::from_xywh(0.0, 0.0, 800.0, 600.0),
      vec![child1, child2],
    );
    let tree = FragmentTree::new(root);

    assert_eq!(tree.fragment_count(), 3);
  }

  #[test]
  fn test_fragment_node_shallow_clone_shares_children() {
    let child = FragmentNode::new_block(Rect::from_xywh(0.0, 0.0, 10.0, 10.0), vec![]);
    let parent = FragmentNode::new_block(Rect::from_xywh(0.0, 0.0, 20.0, 20.0), vec![child]);

    let clone = parent.clone();
    assert_eq!(parent.children.strong_count(), 2);
    assert_eq!(clone.children.len(), 1);
  }

  #[test]
  fn test_deep_clone_detaches_children() {
    let child = FragmentNode::new_block(Rect::from_xywh(1.0, 2.0, 3.0, 4.0), vec![]);
    let parent = FragmentNode::new_block(Rect::from_xywh(0.0, 0.0, 10.0, 10.0), vec![child]);

    let mut clone = parent.deep_clone();
    assert_eq!(parent.children.strong_count(), 1);
    assert_eq!(clone.children.strong_count(), 1);

    clone.children_mut()[0].bounds = clone.children[0].bounds.translate(Point::new(5.0, 0.0));
    assert_ne!(clone.children[0].bounds.x(), parent.children[0].bounds.x());
  }

  // Edge case tests
  #[test]
  fn test_empty_tree_traversal() {
    let fragment = FragmentNode::new_block(Rect::from_xywh(0.0, 0.0, 100.0, 100.0), vec![]);

    assert_eq!(fragment.iter_fragments().count(), 1);
  }

  #[test]
  fn test_is_leaf() {
    let leaf = FragmentNode::new_block(Rect::from_xywh(0.0, 0.0, 100.0, 100.0), vec![]);
    assert!(leaf.is_leaf());

    let child = FragmentNode::new_block(Rect::from_xywh(0.0, 0.0, 50.0, 50.0), vec![]);
    let parent = FragmentNode::new_block(Rect::from_xywh(0.0, 0.0, 100.0, 100.0), vec![child]);
    assert!(!parent.is_leaf());
  }

  #[test]
  fn test_replaced_fragment() {
    let replaced = FragmentNode::new_replaced(
      Rect::from_xywh(0.0, 0.0, 100.0, 100.0),
      ReplacedType::Image {
        src: "test.png".to_string(),
        alt: None,
        sizes: None,
        srcset: Vec::new(),
        picture_sources: Vec::new(),
        crossorigin: CrossOriginAttribute::None,
        referrer_policy: None,
      },
    );

    assert!(replaced.content.is_replaced());
    assert!(replaced.is_leaf());
  }

  #[test]
  fn test_fragment_content_type_checks() {
    let block = FragmentContent::Block { box_id: None };
    assert!(block.is_block());
    assert!(!block.is_inline());
    assert!(!block.is_text());
    assert!(!block.is_line());
    assert!(!block.is_replaced());

    let text = FragmentContent::Text {
      text: "test".into(),
      box_id: None,
      source_range: None,
      baseline_offset: 0.0,
      shaped: None,
      is_marker: false,
      emphasis_offset: Default::default(),
    };
    assert!(text.is_text());
    assert_eq!(text.text(), Some("test"));
  }

  #[test]
  fn text_fragment_clone_shares_payloads() {
    let fragment = FragmentNode::new_text_shaped(
      Rect::from_xywh(0.0, 0.0, 10.0, 10.0),
      "abc",
      4.0,
      Arc::new(Vec::<ShapedRun>::new()),
      Arc::new(ComputedStyle::default()),
    );
    let cloned = fragment.clone();

    if let (
      FragmentContent::Text {
        text: a_text,
        shaped: a_shaped,
        ..
      },
      FragmentContent::Text {
        text: b_text,
        shaped: b_shaped,
        ..
      },
    ) = (&fragment.content, &cloned.content)
    {
      assert!(Arc::ptr_eq(a_text, b_text));
      let a_runs = a_shaped.as_ref().expect("original shaped runs");
      let b_runs = b_shaped.as_ref().expect("cloned shaped runs");
      assert!(Arc::ptr_eq(a_runs, b_runs));
    } else {
      panic!("expected text fragments");
    }
  }

  #[test]
  fn test_block_with_id() {
    let fragment =
      FragmentNode::new_block_with_id(Rect::from_xywh(0.0, 0.0, 100.0, 100.0), 42, vec![]);

    assert!(fragment.content.is_block());
    match fragment.content {
      FragmentContent::Block { box_id } => assert_eq!(box_id, Some(42)),
      _ => panic!("Expected block content"),
    }
  }

  #[test]
  fn test_children_iterator() {
    let child1 = FragmentNode::new_block(Rect::from_xywh(0.0, 0.0, 50.0, 50.0), vec![]);
    let child2 = FragmentNode::new_block(Rect::from_xywh(50.0, 0.0, 50.0, 50.0), vec![]);
    let parent = FragmentNode::new_block(
      Rect::from_xywh(0.0, 0.0, 100.0, 100.0),
      vec![child1, child2],
    );

    assert_eq!(parent.children().count(), 2);
  }

  #[test]
  fn test_fragment_clone_shares_children() {
    let child = FragmentNode::new_text(
      Rect::from_xywh(0.0, 0.0, 10.0, 10.0),
      Arc::<str>::from("a"),
      8.0,
    );
    let parent = FragmentNode::new_block(Rect::from_xywh(0.0, 0.0, 20.0, 20.0), vec![child]);

    let cloned = parent.clone();
    assert!(parent.children.ptr_eq(&cloned.children));
    assert_eq!(parent.children.strong_count(), 2);
  }

  #[test]
  fn test_fragment_deep_clone_breaks_sharing() {
    let child = FragmentNode::new_text(
      Rect::from_xywh(0.0, 0.0, 10.0, 10.0),
      Arc::<str>::from("a"),
      8.0,
    );
    let parent = FragmentNode::new_block(Rect::from_xywh(0.0, 0.0, 20.0, 20.0), vec![child]);

    let cloned = parent.deep_clone();
    assert!(!parent.children.ptr_eq(&cloned.children));
    assert_eq!(parent.children.len(), cloned.children.len());
  }

  #[test]
  fn test_content_size() {
    let child = FragmentNode::new_block(Rect::from_xywh(50.0, 50.0, 100.0, 100.0), vec![]);
    let root = FragmentNode::new_block(Rect::from_xywh(0.0, 0.0, 800.0, 600.0), vec![child]);
    let tree = FragmentTree::new(root);

    let content = tree.content_size();
    assert_eq!(content.min_x(), 0.0);
    assert_eq!(content.max_x(), 800.0);
    assert_eq!(content.max_y(), 600.0);
  }

  #[test]
  fn test_content_size_unions_additional_fragments() {
    let root = FragmentNode::new_block(Rect::from_xywh(0.0, 0.0, 10.0, 10.0), vec![]);
    let additional = FragmentNode::new_block(Rect::from_xywh(0.0, 50.0, 25.0, 25.0), vec![]);
    let mut tree = FragmentTree::new(root);
    tree.additional_fragments.push(additional);

    let content = tree.content_size();
    assert_eq!(content.min_x(), 0.0);
    assert_eq!(content.min_y(), 0.0);
    assert_eq!(content.max_x(), 25.0);
    assert_eq!(content.max_y(), 75.0);
  }

  #[test]
  fn test_drop_is_stack_safe_for_deep_trees() {
    const DEPTH: usize = 30_000;
    let bounds = Rect::from_xywh(0.0, 0.0, 1.0, 1.0);

    let mut node = FragmentNode::new_block(bounds, Vec::new());
    for _ in 0..DEPTH {
      node = FragmentNode::new_block(bounds, vec![node]);
    }

    drop(node);
  }
}
