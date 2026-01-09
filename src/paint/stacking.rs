//! Stacking Context Tree
//!
//! This module implements CSS stacking contexts for determining paint order.
//! Stacking contexts control how elements with z-index, opacity, transforms,
//! and other properties are layered during painting.
//!
//! # CSS Specification
//!
//! - CSS 2.1 Appendix E: Elaborate description of Stacking Contexts
//!   <https://www.w3.org/TR/CSS21/zindex.html>
//! - CSS 2.1 Section 9.9: Layered presentation
//!   <https://www.w3.org/TR/CSS21/visuren.html#layered-presentation>
//!
//! # The 7-Layer Paint Order Algorithm
//!
//! Within each stacking context, elements are painted in this order:
//!
//! 1. Background and borders of the stacking context root
//! 2. Child stacking contexts with negative z-index (most negative first)
//! 3. In-flow, non-inline-level descendants (block boxes in tree order)
//! 4. Non-positioned floats (tree order)
//! 5. In-flow, inline-level descendants (inline boxes and text in tree order)
//! 6. Positioned descendants with z-index 0 or auto (tree order)
//! 7. Child stacking contexts with positive z-index (least positive first)
//!
//! # Stacking Context Creation
//!
//! An element creates a stacking context if it satisfies ANY of these conditions:
//!
//! 1. Root element (`<html>`)
//! 2. Positioned element with z-index ≠ auto (relative/absolute/fixed/sticky + z-index: `<integer>`)
//! 3. Fixed or sticky positioning (even without z-index)
//! 4. Opacity < 1
//! 5. Any transform (except none)
//! 6. Filter property (except none)
//! 7. Clip-path property (except none)
//! 8. Mask properties
//! 9. Mix-blend-mode (except normal)
//! 10. Isolation: isolate
//! 11. Perspective property (except none)
//! 12. Backdrop-filter property (except none)
//! 13. Containment properties (contain: layout|paint|strict|content)
//! 14. Flex items with z-index (child of flex container with z-index)
//! 15. Grid items with z-index (child of grid container with z-index)
//! 16. Will-change set to property that creates stacking context
//! 17. Container type (size or inline-size)
//! 18. Top layer elements (fullscreen, popover, dialog)
//!
//! # Usage
//!
//! ```ignore
//! use fastrender::paint::stacking::{StackingContext, build_stacking_tree};
//! use fastrender::tree::FragmentTree;
//!
//! let fragment_tree = /* ... */;
//! let stacking_tree = build_stacking_tree(&fragment_tree.root, None, true);
//! ```

use crate::error::{Error, RenderStage, Result};
use crate::geometry::Point;
use crate::geometry::Rect;
use crate::paint::display_list::ResolvedFilter;
use crate::paint::display_list::Transform3D;
use crate::paint::filter_outset::filter_outset_with_bounds;
use crate::paint::paint_bounds;
use crate::paint::svg_filter::SvgFilterResolver;
use crate::render_control::check_active_periodic;
use crate::scroll::ScrollState;
use crate::style::display::Display;
use crate::style::position::Position;
use crate::style::types::{FilterColor, FilterFunction};
use crate::style::types::Overflow;
use crate::style::ComputedStyle;
use crate::tree::fragment_tree::FragmentContent;
use crate::tree::fragment_tree::FragmentNode;
use crate::tree::fragment_tree::FragmentTree;
use std::cmp::Ordering;
use std::sync::Arc;

const DEADLINE_STRIDE: usize = 256;

fn resolve_filter_outset_for_bounds(
  style: &ComputedStyle,
  bbox: Rect,
  viewport: Option<(f32, f32)>,
  mut svg_filters: Option<&mut SvgFilterResolver>,
) -> Option<crate::paint::filter_outset::FilterOutset> {
  if style.filter.is_empty() {
    return None;
  }

  let base_x = bbox.width().abs();
  let base_y = bbox.height().abs();
  let base = base_x.max(base_y);

  let resolve_length = |len: &crate::style::values::Length, percentage_base: f32| -> f32 {
    paint_bounds::resolve_length_for_paint(
      len,
      style.font_size,
      style.root_font_size,
      percentage_base,
      viewport,
    )
  };

  let mut resolved_filters = Vec::with_capacity(style.filter.len());
  for filter in &style.filter {
    let resolved = match filter {
      FilterFunction::Blur(radius) => Some(ResolvedFilter::Blur(resolve_length(radius, base).max(0.0))),
      FilterFunction::Brightness(v) => Some(ResolvedFilter::Brightness(*v)),
      FilterFunction::Contrast(v) => Some(ResolvedFilter::Contrast(*v)),
      FilterFunction::Grayscale(v) => Some(ResolvedFilter::Grayscale(*v)),
      FilterFunction::Sepia(v) => Some(ResolvedFilter::Sepia(*v)),
      FilterFunction::Saturate(v) => Some(ResolvedFilter::Saturate(*v)),
      FilterFunction::HueRotate(v) => Some(ResolvedFilter::HueRotate(*v)),
      FilterFunction::Invert(v) => Some(ResolvedFilter::Invert(*v)),
      FilterFunction::Opacity(v) => Some(ResolvedFilter::Opacity(*v)),
      FilterFunction::DropShadow(shadow) => {
        let shadow = shadow.as_ref();
        let color = match shadow.color {
          FilterColor::CurrentColor => style.color,
          FilterColor::Color(c) => c,
        };
        Some(ResolvedFilter::DropShadow {
          offset_x: resolve_length(&shadow.offset_x, base_x),
          offset_y: resolve_length(&shadow.offset_y, base_y),
          blur_radius: resolve_length(&shadow.blur_radius, base).max(0.0),
          spread: resolve_length(&shadow.spread, base),
          color,
        })
      }
      FilterFunction::Url(url) => svg_filters
        .as_deref_mut()
        .and_then(|resolver| resolver.resolve(url))
        .map(ResolvedFilter::SvgFilter),
    };
    if let Some(resolved) = resolved {
      resolved_filters.push(resolved);
    }
  }

  if resolved_filters.is_empty() {
    return None;
  }

  Some(filter_outset_with_bounds(&resolved_filters, 1.0, Some(bbox)))
}

/// A reference to a fragment with associated style information
///
/// Since FragmentNode doesn't carry style information directly,
/// we use this wrapper to associate fragments with their computed styles
/// for stacking context operations.
#[derive(Debug, Clone)]
pub struct StyledFragmentRef<'a> {
  /// The fragment node
  pub fragment: &'a FragmentNode,

  /// The computed style for this fragment (if available)
  pub style: Option<Arc<ComputedStyle>>,

  /// Tree order index (for sorting tie-breaking)
  pub tree_order: usize,
}

impl<'a> StyledFragmentRef<'a> {
  /// Creates a new styled fragment reference
  pub fn new(
    fragment: &'a FragmentNode,
    style: Option<Arc<ComputedStyle>>,
    tree_order: usize,
  ) -> Self {
    Self {
      fragment,
      style,
      tree_order,
    }
  }
}

/// A clipping scope inherited from non-stacking ancestors.
///
/// Some properties (e.g. `overflow: hidden` or `clip`) establish clipping for descendants without
/// creating a stacking context. When stacking contexts are promoted to their nearest ancestor
/// stacking context, we still need to apply those clips during painting.
#[derive(Debug, Clone)]
pub struct ClipChainLink {
  /// The ancestor fragment's border box in the coordinate space of the containing stacking
  /// context.
  pub rect: Rect,
  /// The ancestor fragment's `scroll_overflow` in the fragment's local coordinate space.
  pub scroll_overflow: Rect,
  /// The ancestor fragment's computed style (needed to resolve border radii and clip geometry).
  pub style: Arc<ComputedStyle>,
  /// Whether the fragment represents a replaced element (replaced elements clip their own
  /// contents separately).
  pub is_replaced: bool,
}

/// A fragment paired with its preorder position in the fragment tree.
#[derive(Debug, Clone)]
pub struct OrderedFragment {
  pub fragment: FragmentNode,
  pub tree_order: usize,
  pub clip_chain: Vec<ClipChainLink>,
  /// Number of `backface-visibility: hidden` ancestors (that did *not* create stacking contexts)
  /// between the containing stacking context and this fragment.
  ///
  /// These ancestors need to be re-established during display-list building because positioned
  /// fragments can be promoted out of their non-stacking ancestors for correct z-index ordering.
  pub backface_visibility_depth: usize,
}

impl OrderedFragment {
  pub fn new(fragment: FragmentNode, tree_order: usize) -> Self {
    Self {
      fragment,
      tree_order,
      clip_chain: Vec::new(),
      backface_visibility_depth: 0,
    }
  }

  pub fn new_with_clip_chain(
    fragment: FragmentNode,
    tree_order: usize,
    clip_chain: Vec<ClipChainLink>,
    backface_visibility_depth: usize,
  ) -> Self {
    Self {
      fragment,
      tree_order,
      clip_chain,
      backface_visibility_depth,
    }
  }
}

/// Reasons why a stacking context was created
///
/// Used for debugging and understanding the stacking context tree structure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StackingContextReason {
  /// Root element of the document
  Root,

  /// Positioned element (relative/absolute/fixed/sticky) with z-index != auto
  PositionedWithZIndex,

  /// Fixed positioning (always creates stacking context)
  FixedPositioning,

  /// Sticky positioning (always creates stacking context)
  StickyPositioning,

  /// Opacity < 1.0
  Opacity,

  /// Has CSS transform
  Transform,

  /// Has CSS filter
  Filter,

  /// Has CSS clip-path
  ClipPath,

  /// Has CSS mask
  Mask,

  /// mix-blend-mode != normal
  MixBlendMode,

  /// isolation: isolate
  Isolation,

  /// Has CSS perspective
  Perspective,

  /// Has backdrop-filter
  BackdropFilter,

  /// CSS containment (layout, paint, etc.)
  Containment,

  /// Flex item with z-index
  FlexItemWithZIndex,

  /// Grid item with z-index
  GridItemWithZIndex,

  /// will-change triggers stacking context
  WillChange,

  /// container-type creates stacking context
  ContainerType,

  /// Top layer element (fullscreen, popover, dialog)
  TopLayer,

  /// Overflow hidden/scroll/auto (in some contexts)
  OverflowClip,
}

/// A stacking context in the stacking context tree
///
/// Represents a layer in the paint order hierarchy. Child stacking contexts
/// are sorted by z-index, and descendants within a stacking context are
/// organized into paint layers.
///
/// # Example
///
/// ```ignore
/// let sc = StackingContext::new(0);
/// assert_eq!(sc.z_index, 0);
/// assert!(sc.children.is_empty());
/// ```
#[derive(Debug, Clone)]
pub struct StackingContext {
  /// Z-index value for this stacking context
  ///
  /// - For root: 0
  /// - For positioned elements with z-index: the z-index value
  /// - For auto-created contexts (opacity, transform): 0
  pub z_index: i32,

  /// Child stacking contexts (will be sorted by z-index for painting)
  pub children: Vec<StackingContext>,

  /// Fragments that belong directly to this stacking context
  /// (organized by paint layer)
  pub fragments: Vec<FragmentNode>,

  /// Layer 3: In-flow block-level descendants (tree order)
  pub layer3_blocks: Vec<FragmentNode>,

  /// Layer 4: Non-positioned floats (tree order)
  pub layer4_floats: Vec<FragmentNode>,

  /// Layer 5: In-flow inline-level descendants (tree order)
  pub layer5_inlines: Vec<FragmentNode>,

  /// Layer 6: Positioned descendants with z-index 0 or auto (tree order)
  pub layer6_positioned: Vec<OrderedFragment>,

  /// Offset from the parent stacking context to this context's origin.
  pub offset_from_parent_context: Point,

  /// Bounds of this stacking context, expressed in the parent stacking context's coordinate space.
  ///
  /// These bounds are conservative paint bounds: they include scrollable overflow as well as paint
  /// overflow from effects like outlines and shadows. They are used for culling and to size bounded
  /// offscreen layers created by `PushStackingContext`.
  pub bounds: Rect,

  /// Why this stacking context was created (for debugging)
  pub reason: StackingContextReason,

  /// Clip scopes from non-stacking ancestors between the parent stacking context and this
  /// stacking context.
  pub clip_chain: Vec<ClipChainLink>,

  /// Number of `backface-visibility: hidden` ancestors (that did *not* create stacking contexts)
  /// between the parent stacking context and this stacking context.
  ///
  /// Like [`Self::clip_chain`], this exists because stacking contexts are promoted to their
  /// nearest ancestor stacking context for correct z-index ordering. `backface-visibility` must
  /// still be respected for those promoted descendants without introducing a real stacking context
  /// boundary.
  pub backface_visibility_depth: usize,

  /// Tree order index for stable sorting
  pub tree_order: usize,
}

impl StackingContext {
  fn compare_children_for_paint(a: &StackingContext, b: &StackingContext) -> Ordering {
    match a.z_index.cmp(&b.z_index) {
      Ordering::Equal => {
        let a_top_layer = matches!(a.reason, StackingContextReason::TopLayer);
        let b_top_layer = matches!(b.reason, StackingContextReason::TopLayer);
        match a_top_layer.cmp(&b_top_layer) {
          Ordering::Equal => {
            if a_top_layer {
              // Top layer elements render above all other stacking contexts. They are ordered
              // by tree order, but the earliest element should be painted last (on top).
              b.tree_order.cmp(&a.tree_order)
            } else {
              a.tree_order.cmp(&b.tree_order)
            }
          }
          // Ensure top-layer contexts always sort above non-top-layer contexts, even if the
          // author picked a maximal z-index value.
          other => other,
        }
      }
      other => other,
    }
  }

  /// Creates a new stacking context with the given z-index
  pub fn new(z_index: i32) -> Self {
    Self {
      z_index,
      children: Vec::new(),
      fragments: Vec::new(),
      layer3_blocks: Vec::new(),
      layer4_floats: Vec::new(),
      layer5_inlines: Vec::new(),
      layer6_positioned: Vec::new(),
      offset_from_parent_context: Point::ZERO,
      bounds: Rect::from_xywh(0.0, 0.0, 0.0, 0.0),
      reason: StackingContextReason::Root,
      clip_chain: Vec::new(),
      backface_visibility_depth: 0,
      tree_order: 0,
    }
  }

  /// Creates a new stacking context with reason
  pub fn with_reason(z_index: i32, reason: StackingContextReason, tree_order: usize) -> Self {
    Self {
      z_index,
      children: Vec::new(),
      fragments: Vec::new(),
      layer3_blocks: Vec::new(),
      layer4_floats: Vec::new(),
      layer5_inlines: Vec::new(),
      layer6_positioned: Vec::new(),
      offset_from_parent_context: Point::ZERO,
      bounds: Rect::from_xywh(0.0, 0.0, 0.0, 0.0),
      reason,
      clip_chain: Vec::new(),
      backface_visibility_depth: 0,
      tree_order,
    }
  }

  /// Creates a root stacking context
  pub fn root() -> Self {
    Self::with_reason(0, StackingContextReason::Root, 0)
  }

  /// Adds a child stacking context
  pub fn add_child(&mut self, child: StackingContext) {
    self.children.push(child);
  }

  /// Adds a fragment to the appropriate layer based on its properties
  pub fn add_fragment_to_layer(
    &mut self,
    fragment: FragmentNode,
    style: Option<&ComputedStyle>,
    tree_order: usize,
  ) {
    if let Some(style) = style {
      // Determine which layer this fragment belongs to
      if is_positioned(style) && !creates_stacking_context(style, None, false) {
        // Layer 6: Positioned with z-index 0 or auto
        self
          .layer6_positioned
          .push(OrderedFragment::new(fragment, tree_order));
      } else if is_float(style) {
        // Layer 4: Floats
        self.layer4_floats.push(fragment);
      } else if is_inline_level(style, &fragment) {
        // Layer 5: Inline-level
        self.layer5_inlines.push(fragment);
      } else {
        // Layer 3: Block-level
        self.layer3_blocks.push(fragment);
      }
    } else {
      // No style info - classify based on fragment content
      match &fragment.content {
        FragmentContent::Text { .. } | FragmentContent::Inline { .. } => {
          self.layer5_inlines.push(fragment);
        }
        FragmentContent::Line { .. } => {
          self.layer5_inlines.push(fragment);
        }
        _ => {
          self.layer3_blocks.push(fragment);
        }
      }
    }
  }

  /// Returns child stacking contexts with negative z-index, sorted (most negative first)
  pub fn negative_z_children(&self) -> Vec<&StackingContext> {
    let mut negative: Vec<_> = self.children.iter().filter(|c| c.z_index < 0).collect();
    negative.sort_by(|a, b| Self::compare_children_for_paint(a, b));
    negative
  }

  /// Returns child stacking contexts with zero z-index, sorted by tree order
  pub fn zero_z_children(&self) -> Vec<&StackingContext> {
    let mut zero: Vec<_> = self.children.iter().filter(|c| c.z_index == 0).collect();
    zero.sort_by_key(|c| c.tree_order);
    zero
  }

  /// Returns child stacking contexts with positive z-index, sorted (least positive first)
  pub fn positive_z_children(&self) -> Vec<&StackingContext> {
    let mut positive: Vec<_> = self.children.iter().filter(|c| c.z_index > 0).collect();
    positive.sort_by(|a, b| Self::compare_children_for_paint(a, b));
    positive
  }

  /// Returns layer 6 items (positioned fragments + z-index: 0 child stacking contexts)
  /// in tree order, without allocating.
  pub fn layer6_iter(&self) -> Layer6Iter<'_> {
    Layer6Iter::new(self)
  }

  /// Sorts all child stacking contexts by z-index (for paint order)
  pub fn sort_children(&mut self) {
    self.layer6_positioned.sort_by_key(|frag| frag.tree_order);
    self
      .children
      .sort_by(Self::compare_children_for_paint);

    // Recursively sort grandchildren
    for child in &mut self.children {
      child.sort_children();
    }
  }

  /// Computes bounds from all fragments in this context
  pub fn compute_bounds(
    &mut self,
    viewport: Option<(f32, f32)>,
    mut svg_filters: Option<&mut SvgFilterResolver>,
  ) {
    // Compute child bounds first so they contribute accurately.
    for child in &mut self.children {
      child.compute_bounds(viewport, svg_filters.as_deref_mut());
    }

    let mut bounds: Option<Rect> = None;
    let accumulate = |rect: Rect, current: &mut Option<Rect>| match current {
      Some(existing) => *existing = existing.union(rect),
      None => *current = Some(rect),
    };
    let translate = |rect: Rect| rect.translate(self.offset_from_parent_context);
    let map_rect_with_transform = |rect: Rect, transform: &Transform3D| -> Option<Rect> {
      if transform.is_identity() {
        return Some(rect);
      }
      let corners = [
        (rect.min_x(), rect.min_y()),
        (rect.max_x(), rect.min_y()),
        (rect.max_x(), rect.max_y()),
        (rect.min_x(), rect.max_y()),
      ];
      let mut min_x = f32::INFINITY;
      let mut min_y = f32::INFINITY;
      let mut max_x = f32::NEG_INFINITY;
      let mut max_y = f32::NEG_INFINITY;

      for (x, y) in corners {
        let (tx, ty, _tz, tw) = transform.transform_point(x, y, 0.0);
        if !tx.is_finite()
          || !ty.is_finite()
          || !tw.is_finite()
          || tw.abs() < Transform3D::MIN_PROJECTIVE_W
          || tw < 0.0
        {
          return None;
        }
        let px = tx / tw;
        let py = ty / tw;
        min_x = min_x.min(px);
        min_y = min_y.min(py);
        max_x = max_x.max(px);
        max_y = max_y.max(py);
      }

      let width = max_x - min_x;
      let height = max_y - min_y;
      if width <= 0.0 || height <= 0.0 {
        return None;
      }
      Some(Rect::from_xywh(min_x, min_y, width, height))
    };
    let resolve_self_transform = |context: &StackingContext| -> Option<Transform3D> {
      let root_fragment = context.fragments.first()?;
      let style = root_fragment.style.as_deref()?;
      if !style.has_transform() {
        return None;
      }
      let transform_bounds =
        Rect::new(context.offset_from_parent_context, root_fragment.bounds.size);
      crate::paint::transform_resolver::resolve_transforms(style, transform_bounds, viewport)
        .self_transform
    };
    let resolve_child_perspective = || -> Option<Transform3D> {
      let root_fragment = self.fragments.first()?;
      let style = root_fragment.style.as_deref()?;
      if style.perspective.is_none() {
        return None;
      }
      // Perspective is applied to this context's children in the stacking context's local
      // coordinate space (i.e. relative to the root fragment at (0,0)).
      let transform_bounds = Rect::new(Point::ZERO, root_fragment.bounds.size);
      crate::paint::transform_resolver::resolve_transforms(style, transform_bounds, viewport)
        .child_perspective
    };
    let child_perspective = resolve_child_perspective();
    let map_with_transform = |rect: Rect, transform: &Transform3D| -> Rect {
      match map_rect_with_transform(rect, transform) {
        Some(mapped) => rect.union(mapped),
        None => rect,
      }
    };

    // Union fragment paint bounds from all layers in the parent stacking context's coordinate
    // space.
    //
    // The stacking tree only stores the top-level fragments for each paint layer; many descendants
    // (e.g. wrappers that don't create stacking contexts) are painted via fragment-tree recursion.
    // We must therefore traverse those descendant fragments here, otherwise paint-only effects like
    // box-shadow/outline can be clipped when a stacking context is rendered into a bounded layer
    // surface.
    let mut include_fragment = |frag: &FragmentNode, origin: Point| {
      let border_rect = Rect::new(origin, frag.bounds.size);
      let mut fragment_bounds = paint_bounds::fragment_paint_bounds(
        frag,
        border_rect,
        frag.style.as_deref(),
        viewport,
      );
      fragment_bounds = fragment_bounds.union(frag.scroll_overflow.translate(origin));
      accumulate(translate(fragment_bounds), &mut bounds);
    };

    let mut stack: Vec<(&FragmentNode, Point, bool)> = Vec::new();
    for (idx, fragment) in self.fragments.iter().enumerate() {
      let origin = if idx == 0 {
        Point::ZERO
      } else {
        fragment.bounds.origin
      };
      // Root fragments are painted without descending into children (layered paint handles them),
      // so treat them as shallow for bounds collection too.
      stack.push((fragment, origin, false));
    }
    for fragment in &self.layer3_blocks {
      stack.push((fragment, fragment.bounds.origin, true));
    }
    for fragment in &self.layer4_floats {
      stack.push((fragment, fragment.bounds.origin, true));
    }
    for fragment in &self.layer5_inlines {
      stack.push((fragment, fragment.bounds.origin, true));
    }
    for fragment in &self.layer6_positioned {
      stack.push((&fragment.fragment, fragment.fragment.bounds.origin, true));
    }

    while let Some((fragment, origin, recurse_children)) = stack.pop() {
      include_fragment(fragment, origin);

      if !recurse_children {
        continue;
      }

      let parent_style = fragment.style.as_deref();
      for child in fragment.children.iter() {
        if let Some(child_style) = child.style.as_deref() {
          if creates_stacking_context(child_style, parent_style, false) {
            continue;
          }
          if !matches!(child_style.position, Position::Static)
            && !creates_stacking_context(child_style, None, false)
          {
            continue;
          }
        }

        let child_origin = Point::new(
          origin.x + child.bounds.origin.x,
          origin.y + child.bounds.origin.y,
        );
        stack.push((child, child_origin, true));
      }
    }

    // Union child stacking context bounds
    for child in &self.children {
      let mut rect = child.bounds;
      let child_filter_outset = child
        .fragments
        .first()
        .and_then(|fragment| fragment.style.as_deref())
        .and_then(|style| {
          resolve_filter_outset_for_bounds(style, rect, viewport, svg_filters.as_deref_mut())
        });
      let child_transform = resolve_self_transform(child);
      match (child_perspective.as_ref(), child_transform.as_ref()) {
        (None, None) => {}
        (Some(perspective), None) => {
          rect = map_with_transform(rect, perspective);
        }
        (None, Some(self_transform)) => {
          rect = map_with_transform(rect, self_transform);
        }
        (Some(perspective), Some(self_transform)) => {
          let combined = perspective.multiply(self_transform);
          rect = map_with_transform(rect, &combined);
        }
      }

      if let Some(outset) = child_filter_outset {
        rect = Rect::from_xywh(
          rect.x() - outset.left,
          rect.y() - outset.top,
          rect.width() + outset.left + outset.right,
          rect.height() + outset.top + outset.bottom,
        );
      }

      accumulate(translate(rect), &mut bounds);
    }

    self.bounds = bounds.unwrap_or_else(|| {
      Rect::from_xywh(
        self.offset_from_parent_context.x,
        self.offset_from_parent_context.y,
        0.0,
        0.0,
      )
    });
  }

  /// Returns total fragment count across all layers
  pub fn fragment_count(&self) -> usize {
    self.fragments.len()
      + self.layer3_blocks.len()
      + self.layer4_floats.len()
      + self.layer5_inlines.len()
      + self.layer6_positioned.len()
  }

  /// Returns total count including children (recursive)
  pub fn total_fragment_count(&self) -> usize {
    let mut count = self.fragment_count();
    for child in &self.children {
      count += child.total_fragment_count();
    }
    count
  }
}

/// Checks if an element creates a stacking context
///
/// Implements the comprehensive check for the conditions that create stacking contexts in CSS.
///
/// # Arguments
///
/// * `style` - The computed style for the element
/// * `parent_style` - The parent element's computed style (for flex/grid item checks)
/// * `is_root` - Whether this is the root element
///
/// # Returns
///
/// `true` if the element creates a stacking context
///
/// # Example
///
/// ```ignore
/// use fastrender::paint::stacking::creates_stacking_context;
/// use fastrender::ComputedStyle;
///
/// let mut style = ComputedStyle::default();
/// style.opacity = 0.5;
///
/// assert!(creates_stacking_context(&style, None, false));
/// ```
pub fn creates_stacking_context(
  style: &ComputedStyle,
  parent_style: Option<&ComputedStyle>,
  is_root: bool,
) -> bool {
  // 1. Root element always creates stacking context
  if is_root {
    return true;
  }

  // Top layer elements always create their own stacking context.
  if style.top_layer.is_some() {
    return true;
  }

  // 2. Positioned element with z-index != auto
  if is_positioned(style) && style.z_index.is_some() {
    return true;
  }

  // 3. Fixed positioning always creates stacking context
  if matches!(style.position, Position::Fixed) {
    return true;
  }

  // 4. Sticky positioning always creates stacking context
  if matches!(style.position, Position::Sticky) {
    return true;
  }

  // 5. Opacity < 1.0
  if style.opacity < 1.0 {
    return true;
  }

  // 6. Has CSS transform (including individual transform properties)
  if style.has_transform() {
    return true;
  }

  if style.perspective.is_some() {
    return true;
  }

  // 6b. Has CSS filter (filter list is non-empty)
  if !style.filter.is_empty() {
    return true;
  }

  // 6c. Backdrop filter
  if !style.backdrop_filter.is_empty() {
    return true;
  }

  // 6d. Clip-path
  if !matches!(style.clip_path, crate::style::types::ClipPath::None) {
    return true;
  }

  // 7. Mix-blend-mode or isolation
  if !matches!(
    style.mix_blend_mode,
    crate::style::types::MixBlendMode::Normal
  ) {
    return true;
  }
  if matches!(style.isolation, crate::style::types::Isolation::Isolate) {
    return true;
  }

  if style.mask_border || style.mask_layers.iter().any(|layer| layer.image.is_some()) {
    return true;
  }

  // 7b. Will-change on a stacking-context-creating property
  if style.will_change.creates_stacking_context() {
    return true;
  }

  // 7c. paint containment (or strict/content which imply paint)
  if style.containment.creates_stacking_context() {
    return true;
  }

  // 14/15. Flex/Grid items with z-index
  // If parent is flex/grid container and this element has z-index != 0
  if let Some(parent) = parent_style {
    let parent_is_flex_or_grid = matches!(
      parent.display,
      Display::Flex | Display::InlineFlex | Display::Grid | Display::InlineGrid
    );
    if parent_is_flex_or_grid && style.z_index.is_some() {
      return true;
    }
  }

  // 18. container-type: size/inline-size creates a stacking context.
  if !matches!(
    style.container_type,
    crate::style::types::ContainerType::Normal
  ) {
    return true;
  }

  false
}

/// Gets the reason why an element creates a stacking context
///
/// Returns `None` if the element doesn't create a stacking context.
pub fn get_stacking_context_reason(
  style: &ComputedStyle,
  parent_style: Option<&ComputedStyle>,
  is_root: bool,
) -> Option<StackingContextReason> {
  if is_root {
    return Some(StackingContextReason::Root);
  }

  if style.top_layer.is_some() {
    return Some(StackingContextReason::TopLayer);
  }

  if is_positioned(style) && style.z_index.is_some() {
    return Some(StackingContextReason::PositionedWithZIndex);
  }

  if matches!(style.position, Position::Fixed) {
    return Some(StackingContextReason::FixedPositioning);
  }

  if matches!(style.position, Position::Sticky) {
    return Some(StackingContextReason::StickyPositioning);
  }

  if style.opacity < 1.0 {
    return Some(StackingContextReason::Opacity);
  }

  if style.mask_border || style.mask_layers.iter().any(|layer| layer.image.is_some()) {
    return Some(StackingContextReason::Mask);
  }

  if style.has_transform() {
    return Some(StackingContextReason::Transform);
  }

  if style.perspective.is_some() {
    return Some(StackingContextReason::Perspective);
  }

  if !style.filter.is_empty() {
    return Some(StackingContextReason::Filter);
  }

  if !style.backdrop_filter.is_empty() {
    return Some(StackingContextReason::BackdropFilter);
  }

  if !matches!(style.clip_path, crate::style::types::ClipPath::None) {
    return Some(StackingContextReason::ClipPath);
  }

  if !matches!(
    style.mix_blend_mode,
    crate::style::types::MixBlendMode::Normal
  ) {
    return Some(StackingContextReason::MixBlendMode);
  }

  if matches!(style.isolation, crate::style::types::Isolation::Isolate) {
    return Some(StackingContextReason::Isolation);
  }

  if style.will_change.creates_stacking_context() {
    return Some(StackingContextReason::WillChange);
  }

  if style.containment.creates_stacking_context() {
    return Some(StackingContextReason::Containment);
  }

  if let Some(parent) = parent_style {
    let parent_is_flex = matches!(parent.display, Display::Flex | Display::InlineFlex);
    let parent_is_grid = matches!(parent.display, Display::Grid | Display::InlineGrid);

    if parent_is_flex && style.z_index.is_some() {
      return Some(StackingContextReason::FlexItemWithZIndex);
    }
    if parent_is_grid && style.z_index.is_some() {
      return Some(StackingContextReason::GridItemWithZIndex);
    }
  }

  if !matches!(
    style.container_type,
    crate::style::types::ContainerType::Normal
  ) {
    return Some(StackingContextReason::ContainerType);
  }

  None
}

/// Checks if an element is positioned (not static)
fn is_positioned(style: &ComputedStyle) -> bool {
  !matches!(style.position, Position::Static)
}

/// Checks if an element is a float
///
/// Floats participate in layer 4 of the stacking order (between blocks and
/// inlines). Spec-wise floats are ignored for absolutely/fixed positioned
/// elements because their used value becomes `none`; we mirror that so
/// positioned elements stay in the positioned layer.
fn is_float(style: &ComputedStyle) -> bool {
  if matches!(style.position, Position::Absolute | Position::Fixed) {
    return false;
  }
  style.float.is_floating()
}

/// Checks if an element is inline-level
fn is_inline_level(style: &ComputedStyle, fragment: &FragmentNode) -> bool {
  // Check display property
  let is_inline_display = matches!(
    style.display,
    Display::Inline
      | Display::InlineBlock
      | Display::InlineFlex
      | Display::InlineGrid
      | Display::InlineTable
  );

  // Also check fragment content type
  let is_inline_content = matches!(
    fragment.content,
    FragmentContent::Inline { .. } | FragmentContent::Text { .. } | FragmentContent::Line { .. }
  );

  is_inline_display || is_inline_content
}

fn overflow_axis_clips(overflow: Overflow) -> bool {
  matches!(
    overflow,
    Overflow::Hidden | Overflow::Scroll | Overflow::Auto | Overflow::Clip
  )
}

fn clip_chain_link_for_fragment(
  fragment: &FragmentNode,
  style: &Arc<ComputedStyle>,
  offset_from_parent_context: Point,
) -> Option<ClipChainLink> {
  let is_replaced = matches!(fragment.content, FragmentContent::Replaced { .. });
  let clips_overflow =
    !is_replaced && (overflow_axis_clips(style.overflow_x) || overflow_axis_clips(style.overflow_y));
  // CSS 2.1 `clip` only applies to absolutely positioned elements.
  let clips_rect =
    matches!(style.position, Position::Absolute | Position::Fixed) && style.clip.is_some();
  if !(clips_overflow || clips_rect) {
    return None;
  }

  Some(ClipChainLink {
    rect: Rect::new(offset_from_parent_context, fragment.bounds.size),
    scroll_overflow: fragment.scroll_overflow,
    style: style.clone(),
    is_replaced,
  })
}

#[inline]
fn element_scroll_offset(fragment: &FragmentNode, scroll_state: Option<&ScrollState>) -> Point {
  scroll_state
    .and_then(|state| fragment.box_id().map(|id| state.element_offset(id)))
    .unwrap_or(Point::ZERO)
}

/// Builds a stacking context tree from a fragment tree
///
/// This function traverses the fragment tree and builds a corresponding
/// stacking context tree that can be used for correct paint ordering.
///
/// # Arguments
///
/// * `root` - The root fragment node
/// * `root_style` - Optional style for the root element
/// * `is_root_context` - Whether this is the document root
///
/// # Returns
///
/// A `StackingContext` representing the stacking context tree
///
/// # Example
///
/// ```ignore
/// use fastrender::paint::stacking::build_stacking_tree;
///
/// let root_fragment = /* ... */;
/// let stacking_tree = build_stacking_tree(&root_fragment, None, true);
/// ```
pub fn build_stacking_tree(
  root: &FragmentNode,
  root_style: Option<&ComputedStyle>,
  is_root_context: bool,
) -> StackingContext {
  let mut tree_order_counter = 0;
  let mut context = build_stacking_tree_internal(
    root,
    root_style,
    None,
    is_root_context,
    &mut tree_order_counter,
    Point::ZERO,
  );

  // Sort all children by z-index
  context.sort_children();

  // Compute bounds
  context.compute_bounds(Some((root.bounds.width(), root.bounds.height())), None);

  context
}

/// Internal recursive function to build stacking context tree
fn build_stacking_tree_internal(
  fragment: &FragmentNode,
  style: Option<&ComputedStyle>,
  parent_style: Option<&ComputedStyle>,
  is_root: bool,
  tree_order: &mut usize,
  offset_from_parent_context: Point,
) -> StackingContext {
  let current_order = *tree_order;
  *tree_order += 1;

  // Check if this fragment creates a stacking context
  let creates_context = if let Some(s) = style {
    creates_stacking_context(s, parent_style, is_root)
  } else {
    is_root
  };

  if creates_context {
    // Create a new stacking context
    let z_index = style
      .map(|s| {
        if s.top_layer.is_some() {
          i32::MAX
        } else {
          s.z_index.unwrap_or(0)
        }
      })
      .unwrap_or(0);
    let reason = style
      .and_then(|s| get_stacking_context_reason(s, parent_style, is_root))
      .unwrap_or(StackingContextReason::Root);

    let mut context = StackingContext::with_reason(z_index, reason, current_order);
    context.offset_from_parent_context = offset_from_parent_context;

    // Add the root fragment
    context.fragments.push(fragment.clone());

    // Process children
    let base_offset = Point::ZERO;
    for child in fragment.children.iter() {
      let child_offset = Point::new(
        base_offset.x + child.bounds.origin.x,
        base_offset.y + child.bounds.origin.y,
      );
      let child_context = build_stacking_tree_internal(
        child,
        None, // We don't have style for children without external mapping
        style,
        false,
        tree_order,
        child_offset,
      );

      let child_creates_context =
        child_context.reason != StackingContextReason::Root || child_context.z_index != 0;

      if child_creates_context {
        // Child has its own stacking context structure
        context.add_child(child_context);
      } else {
        // Propagate any nested child contexts upward
        if !child_context.children.is_empty() {
          context.children.extend(child_context.children);
        }
        // Keep the direct child in the appropriate layer; children will be painted via recursion
        context.add_fragment_to_layer(child.clone(), None, child_context.tree_order);
      }
    }

    context
  } else {
    // Don't create a new stacking context, but still process for layer classification
    let mut context = StackingContext::new(0);
    context.tree_order = current_order;

    context.offset_from_parent_context = offset_from_parent_context;

    // Classify this fragment into appropriate layer
    if let Some(s) = style {
      context.add_fragment_to_layer(fragment.clone(), Some(s), current_order);
    } else {
      // Classify based on fragment content
      match &fragment.content {
        FragmentContent::Text { .. }
        | FragmentContent::Inline { .. }
        | FragmentContent::Line { .. } => {
          context.layer5_inlines.push(fragment.clone());
        }
        _ => {
          context.layer3_blocks.push(fragment.clone());
        }
      }
    }

    // Process children
    let base_offset = offset_from_parent_context;
    for child in fragment.children.iter() {
      let child_offset = Point::new(
        base_offset.x + child.bounds.origin.x,
        base_offset.y + child.bounds.origin.y,
      );
      let child_context =
        build_stacking_tree_internal(child, None, style, false, tree_order, child_offset);

      let child_creates_context =
        child_context.reason != StackingContextReason::Root || child_context.z_index != 0;

      if child_creates_context {
        context.add_child(child_context);
      } else {
        if !child_context.children.is_empty() {
          context.children.extend(child_context.children);
        }
        context.add_fragment_to_layer(child.clone(), None, child_context.tree_order);
      }
    }

    context
  }
}

/// Builds a stacking context tree with style information from a styled tree
///
/// This version takes a style lookup function to get ComputedStyle for each fragment.
///
/// # Arguments
///
/// * `root` - The root fragment node
/// * `get_style` - Function to look up style for a fragment (by box_id or other means)
///
/// # Returns
///
/// A `StackingContext` representing the stacking context tree
pub fn build_stacking_tree_with_styles<F>(root: &FragmentNode, get_style: F) -> StackingContext
where
  F: Fn(&FragmentNode) -> Option<Arc<ComputedStyle>> + Clone,
{
  let mut tree_order_counter = 0;
  build_stacking_tree_with_styles_and_counter(
    root,
    &get_style,
    &mut tree_order_counter,
    Some((root.bounds.width(), root.bounds.height())),
    None,
  )
}

/// Builds a stacking context tree with cooperative cancellation.
///
/// This is the deadline-aware variant used by the display list pipeline so the
/// builder can fall back to a different paint backend instead of getting
/// hard-killed after the render timeout expires.
pub fn build_stacking_tree_with_styles_checked<F>(
  root: &FragmentNode,
  get_style: F,
) -> Result<StackingContext>
where
  F: Fn(&FragmentNode) -> Option<Arc<ComputedStyle>> + Clone,
{
  let mut tree_order_counter = 0;
  let mut deadline_counter = 0usize;
  build_stacking_tree_with_styles_and_counter_checked(
    root,
    &get_style,
    &mut tree_order_counter,
    &mut deadline_counter,
    Some((root.bounds.width(), root.bounds.height())),
    None,
  )
}

fn build_stacking_tree_with_styles_and_counter<F>(
  root: &FragmentNode,
  get_style: &F,
  tree_order_counter: &mut usize,
  viewport: Option<(f32, f32)>,
  scroll_state: Option<&ScrollState>,
) -> StackingContext
where
  F: Fn(&FragmentNode) -> Option<Arc<ComputedStyle>> + Clone,
{
  let root_style = get_style(root);
  let mut clip_stack = Vec::new();
  let mut backface_depth = 0usize;
  let mut context = build_stacking_tree_with_styles_internal(
    root,
    root_style,
    None,
    true,
    tree_order_counter,
    root.bounds.origin,
    &mut clip_stack,
    &mut backface_depth,
    get_style,
    false,
    scroll_state,
  );

  context.sort_children();
  context.compute_bounds(viewport, None);
  context
}

fn build_stacking_tree_with_styles_and_counter_checked<F>(
  root: &FragmentNode,
  get_style: &F,
  tree_order_counter: &mut usize,
  deadline_counter: &mut usize,
  viewport: Option<(f32, f32)>,
  scroll_state: Option<&ScrollState>,
) -> Result<StackingContext>
where
  F: Fn(&FragmentNode) -> Option<Arc<ComputedStyle>> + Clone,
{
  let root_style = get_style(root);
  let mut clip_stack = Vec::new();
  let mut backface_depth = 0usize;
  let mut context = build_stacking_tree_with_styles_internal_checked(
    root,
    root_style,
    None,
    true,
    tree_order_counter,
    root.bounds.origin,
    &mut clip_stack,
    &mut backface_depth,
    get_style,
    false,
    deadline_counter,
    scroll_state,
  )?;

  context.sort_children();
  context.compute_bounds(viewport, None);
  Ok(context)
}

/// Builds a stacking context tree from a fragment tree using the fragment's embedded styles.
pub fn build_stacking_tree_from_fragment_tree(root: &FragmentNode) -> StackingContext {
  build_stacking_tree_with_styles(root, |fragment| fragment.style.clone())
}

/// Builds a stacking context tree from a fragment tree using the fragment's embedded styles while
/// surfacing timeouts.
pub fn build_stacking_tree_from_fragment_tree_checked(
  root: &FragmentNode,
) -> Result<StackingContext> {
  build_stacking_tree_with_styles_checked(root, |fragment| fragment.style.clone())
}

pub fn build_stacking_tree_from_fragment_tree_checked_with_scroll(
  root: &FragmentNode,
  scroll_state: &ScrollState,
) -> Result<StackingContext> {
  let mut tree_order_counter = 0;
  let mut deadline_counter = 0usize;
  build_stacking_tree_with_styles_and_counter_checked(
    root,
    &|fragment| fragment.style.clone(),
    &mut tree_order_counter,
    &mut deadline_counter,
    Some((root.bounds.width(), root.bounds.height())),
    Some(scroll_state),
  )
}

/// Builds stacking context trees for every root in a FragmentTree.
///
/// Returns contexts in fragmentainer/page order: the primary root first followed by
/// `additional_fragments` in order.
pub fn build_stacking_tree_from_tree(tree: &FragmentTree) -> Vec<StackingContext> {
  let viewport = tree.viewport_size();
  let viewport = Some((viewport.width, viewport.height));
  let mut tree_order_counter = 0;
  let mut contexts = Vec::with_capacity(1 + tree.additional_fragments.len());
  contexts.push(build_stacking_tree_with_styles_and_counter(
    &tree.root,
    &|fragment| fragment.style.clone(),
    &mut tree_order_counter,
    viewport,
    None,
  ));
  for fragment in &tree.additional_fragments {
    contexts.push(build_stacking_tree_with_styles_and_counter(
      fragment,
      &|node| node.style.clone(),
      &mut tree_order_counter,
      viewport,
      None,
    ));
  }
  contexts
}

/// Builds stacking context trees for every root in a FragmentTree while surfacing timeouts.
pub fn build_stacking_tree_from_tree_checked(tree: &FragmentTree) -> Result<Vec<StackingContext>> {
  let viewport = tree.viewport_size();
  let viewport = Some((viewport.width, viewport.height));
  let mut tree_order_counter = 0;
  let mut deadline_counter = 0usize;
  let mut contexts = Vec::with_capacity(1 + tree.additional_fragments.len());
  contexts.push(build_stacking_tree_with_styles_and_counter_checked(
    &tree.root,
    &|fragment| fragment.style.clone(),
    &mut tree_order_counter,
    &mut deadline_counter,
    viewport,
    None,
  )?);
  for fragment in &tree.additional_fragments {
    contexts.push(build_stacking_tree_with_styles_and_counter_checked(
      fragment,
      &|node| node.style.clone(),
      &mut tree_order_counter,
      &mut deadline_counter,
      viewport,
      None,
    )?);
  }
  Ok(contexts)
}

pub fn build_stacking_tree_from_tree_checked_with_scroll(
  tree: &FragmentTree,
  scroll_state: &ScrollState,
) -> Result<Vec<StackingContext>> {
  let viewport = tree.viewport_size();
  let viewport = Some((viewport.width, viewport.height));
  let mut tree_order_counter = 0;
  let mut deadline_counter = 0usize;
  let mut contexts = Vec::with_capacity(1 + tree.additional_fragments.len());
  contexts.push(build_stacking_tree_with_styles_and_counter_checked(
    &tree.root,
    &|fragment| fragment.style.clone(),
    &mut tree_order_counter,
    &mut deadline_counter,
    viewport,
    Some(scroll_state),
  )?);
  for fragment in &tree.additional_fragments {
    contexts.push(build_stacking_tree_with_styles_and_counter_checked(
      fragment,
      &|node| node.style.clone(),
      &mut tree_order_counter,
      &mut deadline_counter,
      viewport,
      Some(scroll_state),
    )?);
  }
  Ok(contexts)
}

fn build_stacking_tree_with_styles_internal_checked<F>(
  fragment: &FragmentNode,
  style: Option<Arc<ComputedStyle>>,
  parent_style: Option<&ComputedStyle>,
  is_root: bool,
  tree_order: &mut usize,
  offset_from_parent_context: Point,
  clip_stack: &mut Vec<ClipChainLink>,
  backface_depth: &mut usize,
  get_style: &F,
  skip_viewport_scroll_cancel: bool,
  deadline_counter: &mut usize,
  scroll_state: Option<&ScrollState>,
) -> Result<StackingContext>
where
  F: Fn(&FragmentNode) -> Option<Arc<ComputedStyle>>,
{
  check_active_periodic(deadline_counter, DEADLINE_STRIDE, RenderStage::Paint)
    .map_err(Error::Render)?;

  let current_order = *tree_order;
  *tree_order += 1;

  let creates_context = if let Some(s) = style.as_deref() {
    creates_stacking_context(s, parent_style, is_root)
  } else {
    is_root
  };
  let establishes_fixed_cb = style
    .as_deref()
    .is_some_and(|style| style.establishes_fixed_containing_block());
  let needs_viewport_scroll_cancel = style
    .as_deref()
    .is_some_and(|style| matches!(style.position, Position::Fixed))
    && !skip_viewport_scroll_cancel;
  let skip_viewport_scroll_cancel_for_children =
    skip_viewport_scroll_cancel || establishes_fixed_cb || needs_viewport_scroll_cancel;
  let viewport_scroll = scroll_state
    .map(|state| state.viewport)
    .filter(|scroll| scroll.x.is_finite() && scroll.y.is_finite())
    .unwrap_or(Point::ZERO);

  if creates_context {
    let z_index = style
      .as_deref()
      .map(|s| {
        if s.top_layer.is_some() {
          i32::MAX
        } else {
          s.z_index.unwrap_or(0)
        }
      })
      .unwrap_or(0);
    let reason = style
      .as_deref()
      .and_then(|s| get_stacking_context_reason(s, parent_style, is_root))
      .unwrap_or(StackingContextReason::Root);

    let mut context = StackingContext::with_reason(z_index, reason, current_order);
    context.clip_chain = clip_stack.clone();
    context.backface_visibility_depth = *backface_depth;
    context.offset_from_parent_context = if needs_viewport_scroll_cancel {
      Point::new(
        offset_from_parent_context.x + viewport_scroll.x,
        offset_from_parent_context.y + viewport_scroll.y,
      )
    } else {
      offset_from_parent_context
    };
    context.fragments.push(fragment.clone());

    let base_offset = Point::ZERO;
    let mut child_clip_stack = Vec::new();
    let mut child_backface_depth = 0usize;
    for child in fragment.children.iter() {
      let child_style = get_style(child);
      let child_offset = Point::new(
        base_offset.x + child.bounds.origin.x,
        base_offset.y + child.bounds.origin.y,
      );
      let mut child_context = build_stacking_tree_with_styles_internal_checked(
        child,
        child_style.clone(),
        style.as_deref(),
        false,
        tree_order,
        child_offset,
        &mut child_clip_stack,
        &mut child_backface_depth,
        get_style,
        skip_viewport_scroll_cancel_for_children,
        deadline_counter,
        scroll_state,
      )?;

      let child_creates_context =
        child_context.reason != StackingContextReason::Root || child_context.z_index != 0;

      if child_creates_context {
        context.add_child(child_context);
      } else {
        context.children.append(&mut child_context.children);
        context
          .layer6_positioned
          .append(&mut child_context.layer6_positioned);

        let child_is_positioned = child_style.as_deref().is_some_and(|style| {
          is_positioned(style) && !creates_stacking_context(style, None, false)
        });
        if !child_is_positioned {
          context.add_fragment_to_layer(
            child.clone(),
            child_style.as_deref(),
            child_context.tree_order,
          );
        }
      }
    }

    Ok(context)
  } else {
    let mut context = StackingContext::new(0);
    context.tree_order = current_order;
    context.offset_from_parent_context = offset_from_parent_context;

    if let Some(s) = style.as_deref() {
      if is_positioned(s) && !creates_stacking_context(s, None, false) {
        let mut translated = fragment.clone();
        translated.translate_root_in_place(Point::new(
          offset_from_parent_context.x - translated.bounds.origin.x,
          offset_from_parent_context.y - translated.bounds.origin.y,
        ));
        context
          .layer6_positioned
          .push(OrderedFragment::new_with_clip_chain(
            translated,
            current_order,
            clip_stack.clone(),
            *backface_depth,
          ));
      } else {
        context.add_fragment_to_layer(fragment.clone(), Some(s), current_order);
      }
    } else {
      match &fragment.content {
        FragmentContent::Text { .. }
        | FragmentContent::Inline { .. }
        | FragmentContent::Line { .. } => {
          context.layer5_inlines.push(fragment.clone());
        }
        _ => {
          context.layer3_blocks.push(fragment.clone());
        }
      }
    }

    let clip_pushed = style
      .as_ref()
      .and_then(|style| clip_chain_link_for_fragment(fragment, style, offset_from_parent_context))
      .map(|link| {
        clip_stack.push(link);
      })
      .is_some();
    let backface_pushed = style
      .as_deref()
      .is_some_and(|style| matches!(style.backface_visibility, crate::style::types::BackfaceVisibility::Hidden));
    if backface_pushed {
      *backface_depth += 1;
    }

    let element_scroll = element_scroll_offset(fragment, scroll_state);
    let base_offset = Point::new(
      offset_from_parent_context.x - element_scroll.x,
      offset_from_parent_context.y - element_scroll.y,
    );
    for child in fragment.children.iter() {
      let child_style = get_style(child);
      let child_offset = Point::new(
        base_offset.x + child.bounds.origin.x,
        base_offset.y + child.bounds.origin.y,
      );
      let mut child_context = build_stacking_tree_with_styles_internal_checked(
        child,
        child_style.clone(),
        style.as_deref(),
        false,
        tree_order,
        child_offset,
        clip_stack,
        backface_depth,
        get_style,
        skip_viewport_scroll_cancel_for_children,
        deadline_counter,
        scroll_state,
      )?;

      let child_creates_context =
        child_context.reason != StackingContextReason::Root || child_context.z_index != 0;

      if child_creates_context {
        context.add_child(child_context);
      } else {
        context.children.append(&mut child_context.children);
        context
          .layer6_positioned
          .append(&mut child_context.layer6_positioned);

        let child_is_positioned = child_style.as_deref().is_some_and(|style| {
          is_positioned(style) && !creates_stacking_context(style, None, false)
        });
        if !child_is_positioned {
          context.add_fragment_to_layer(
            child.clone(),
            child_style.as_deref(),
            child_context.tree_order,
          );
        }
      }
    }

    if clip_pushed {
      clip_stack.pop();
    }
    if backface_pushed {
      *backface_depth = backface_depth.saturating_sub(1);
    }

    Ok(context)
  }
}

/// Internal recursive function to build stacking context tree with styles
fn build_stacking_tree_with_styles_internal<F>(
  fragment: &FragmentNode,
  style: Option<Arc<ComputedStyle>>,
  parent_style: Option<&ComputedStyle>,
  is_root: bool,
  tree_order: &mut usize,
  offset_from_parent_context: Point,
  clip_stack: &mut Vec<ClipChainLink>,
  backface_depth: &mut usize,
  get_style: &F,
  skip_viewport_scroll_cancel: bool,
  scroll_state: Option<&ScrollState>,
) -> StackingContext
where
  F: Fn(&FragmentNode) -> Option<Arc<ComputedStyle>>,
{
  let current_order = *tree_order;
  *tree_order += 1;

  let creates_context = if let Some(s) = style.as_deref() {
    creates_stacking_context(s, parent_style, is_root)
  } else {
    is_root
  };
  let establishes_fixed_cb = style
    .as_deref()
    .is_some_and(|style| style.establishes_fixed_containing_block());
  let needs_viewport_scroll_cancel = style
    .as_deref()
    .is_some_and(|style| matches!(style.position, Position::Fixed))
    && !skip_viewport_scroll_cancel;
  let skip_viewport_scroll_cancel_for_children =
    skip_viewport_scroll_cancel || establishes_fixed_cb || needs_viewport_scroll_cancel;
  let viewport_scroll = scroll_state
    .map(|state| state.viewport)
    .filter(|scroll| scroll.x.is_finite() && scroll.y.is_finite())
    .unwrap_or(Point::ZERO);

  if creates_context {
    let z_index = style
      .as_deref()
      .map(|s| {
        if s.top_layer.is_some() {
          i32::MAX
        } else {
          s.z_index.unwrap_or(0)
        }
      })
      .unwrap_or(0);
    let reason = style
      .as_deref()
      .and_then(|s| get_stacking_context_reason(s, parent_style, is_root))
      .unwrap_or(StackingContextReason::Root);

    let mut context = StackingContext::with_reason(z_index, reason, current_order);
    context.clip_chain = clip_stack.clone();
    context.backface_visibility_depth = *backface_depth;
    context.offset_from_parent_context = if needs_viewport_scroll_cancel {
      Point::new(
        offset_from_parent_context.x + viewport_scroll.x,
        offset_from_parent_context.y + viewport_scroll.y,
      )
    } else {
      offset_from_parent_context
    };
    context.fragments.push(fragment.clone());

    let base_offset = Point::ZERO;
    let mut child_clip_stack = Vec::new();
    let mut child_backface_depth = 0usize;
    for child in fragment.children.iter() {
      let child_style = get_style(child);
      let child_offset = Point::new(
        base_offset.x + child.bounds.origin.x,
        base_offset.y + child.bounds.origin.y,
      );
      let mut child_context = build_stacking_tree_with_styles_internal(
        child,
        child_style.clone(),
        style.as_deref(),
        false,
        tree_order,
        child_offset,
        &mut child_clip_stack,
        &mut child_backface_depth,
        get_style,
        skip_viewport_scroll_cancel_for_children,
        scroll_state,
      );

      let child_creates_context =
        child_context.reason != StackingContextReason::Root || child_context.z_index != 0;

      if child_creates_context {
        context.add_child(child_context);
      } else {
        context.children.append(&mut child_context.children);
        context
          .layer6_positioned
          .append(&mut child_context.layer6_positioned);

        let child_is_positioned = child_style.as_deref().is_some_and(|style| {
          is_positioned(style) && !creates_stacking_context(style, None, false)
        });
        if !child_is_positioned {
          context.add_fragment_to_layer(
            child.clone(),
            child_style.as_deref(),
            child_context.tree_order,
          );
        }
      }
    }

    context
  } else {
    let mut context = StackingContext::new(0);
    context.tree_order = current_order;
    context.offset_from_parent_context = offset_from_parent_context;

    if let Some(s) = style.as_deref() {
      if is_positioned(s) && !creates_stacking_context(s, None, false) {
        let mut translated = fragment.clone();
        translated.translate_root_in_place(Point::new(
          offset_from_parent_context.x - translated.bounds.origin.x,
          offset_from_parent_context.y - translated.bounds.origin.y,
        ));
        context
          .layer6_positioned
          .push(OrderedFragment::new_with_clip_chain(
            translated,
            current_order,
            clip_stack.clone(),
            *backface_depth,
          ));
      } else {
        context.add_fragment_to_layer(fragment.clone(), Some(s), current_order);
      }
    } else {
      match &fragment.content {
        FragmentContent::Text { .. }
        | FragmentContent::Inline { .. }
        | FragmentContent::Line { .. } => {
          context.layer5_inlines.push(fragment.clone());
        }
        _ => {
          context.layer3_blocks.push(fragment.clone());
        }
      }
    }

    let clip_pushed = style
      .as_ref()
      .and_then(|style| clip_chain_link_for_fragment(fragment, style, offset_from_parent_context))
      .map(|link| {
        clip_stack.push(link);
      })
      .is_some();
    let backface_pushed = style
      .as_deref()
      .is_some_and(|style| matches!(style.backface_visibility, crate::style::types::BackfaceVisibility::Hidden));
    if backface_pushed {
      *backface_depth += 1;
    }

    let element_scroll = element_scroll_offset(fragment, scroll_state);
    let base_offset = Point::new(
      offset_from_parent_context.x - element_scroll.x,
      offset_from_parent_context.y - element_scroll.y,
    );
    for child in fragment.children.iter() {
      let child_style = get_style(child);
      let child_offset = Point::new(
        base_offset.x + child.bounds.origin.x,
        base_offset.y + child.bounds.origin.y,
      );
      let mut child_context = build_stacking_tree_with_styles_internal(
        child,
        child_style.clone(),
        style.as_deref(),
        false,
        tree_order,
        child_offset,
        clip_stack,
        backface_depth,
        get_style,
        skip_viewport_scroll_cancel_for_children,
        scroll_state,
      );

      let child_creates_context =
        child_context.reason != StackingContextReason::Root || child_context.z_index != 0;

      if child_creates_context {
        context.add_child(child_context);
      } else {
        context.children.append(&mut child_context.children);
        context
          .layer6_positioned
          .append(&mut child_context.layer6_positioned);

        let child_is_positioned = child_style.as_deref().is_some_and(|style| {
          is_positioned(style) && !creates_stacking_context(style, None, false)
        });
        if !child_is_positioned {
          context.add_fragment_to_layer(
            child.clone(),
            child_style.as_deref(),
            child_context.tree_order,
          );
        }
      }
    }

    if clip_pushed {
      clip_stack.pop();
    }
    if backface_pushed {
      *backface_depth = backface_depth.saturating_sub(1);
    }

    context
  }
}

/// Represents an item in layer 6 when determining paint order.
#[derive(Clone)]
pub enum Layer6Item<'a> {
  Positioned(&'a OrderedFragment),
  ZeroContext(&'a StackingContext),
}

impl<'a> Layer6Item<'a> {
  pub fn tree_order(&self) -> usize {
    match self {
      Layer6Item::Positioned(frag) => frag.tree_order,
      Layer6Item::ZeroContext(ctx) => ctx.tree_order,
    }
  }
}

/// Iterator for the layer 6 paint order merge (positioned fragments + z-index: 0 contexts).
///
/// This is a hot path: display list building calls it for every stacking context. It performs
/// a zero-allocation merge of two already-sorted sequences by `tree_order`.
#[derive(Clone, Copy)]
pub struct Layer6Iter<'a> {
  positioned: &'a [OrderedFragment],
  positioned_idx: usize,
  zero_contexts: &'a [StackingContext],
  zero_idx: usize,
}

impl<'a> Layer6Iter<'a> {
  fn new(context: &'a StackingContext) -> Self {
    let children = context.children.as_slice();
    // Children are already sorted by (z_index, tree_order) via `StackingContext::sort_children()`.
    // The z-index==0 subsequence is thus also in tree order and can be merged directly.
    let first_non_neg = children.partition_point(|c| c.z_index < 0);
    let first_pos = children.partition_point(|c| c.z_index <= 0);
    let zero_contexts = &children[first_non_neg..first_pos];

    Self {
      positioned: context.layer6_positioned.as_slice(),
      positioned_idx: 0,
      zero_contexts,
      zero_idx: 0,
    }
  }

  fn is_done(&self) -> bool {
    self.positioned_idx >= self.positioned.len() && self.zero_idx >= self.zero_contexts.len()
  }
}

impl<'a> Iterator for Layer6Iter<'a> {
  type Item = Layer6Item<'a>;

  fn next(&mut self) -> Option<Self::Item> {
    let next_positioned = self.positioned.get(self.positioned_idx);
    let next_zero = self.zero_contexts.get(self.zero_idx);

    match (next_positioned, next_zero) {
      (Some(positioned), Some(zero_ctx)) => {
        // If two items ever share a tree order (unlikely), preserve the prior stable-sort behavior:
        // positioned fragments were inserted before contexts into the combined list, so they win ties.
        if positioned.tree_order <= zero_ctx.tree_order {
          self.positioned_idx += 1;
          Some(Layer6Item::Positioned(positioned))
        } else {
          self.zero_idx += 1;
          Some(Layer6Item::ZeroContext(zero_ctx))
        }
      }
      (Some(positioned), None) => {
        self.positioned_idx += 1;
        Some(Layer6Item::Positioned(positioned))
      }
      (None, Some(zero_ctx)) => {
        self.zero_idx += 1;
        Some(Layer6Item::ZeroContext(zero_ctx))
      }
      (None, None) => None,
    }
  }

  fn size_hint(&self) -> (usize, Option<usize>) {
    let remaining = self
      .positioned
      .len()
      .saturating_sub(self.positioned_idx)
      .saturating_add(self.zero_contexts.len().saturating_sub(self.zero_idx));
    (remaining, Some(remaining))
  }
}

impl<'a> ExactSizeIterator for Layer6Iter<'a> {}

/// Iterator for traversing stacking context in paint order
///
/// Yields fragments in the correct 7-layer paint order.
pub struct PaintOrderIterator<'a> {
  stack: Vec<PaintOrderItem<'a>>,
}

#[derive(Clone)]
enum PaintOrderItem<'a> {
  Context(&'a StackingContext),
  Layer3(&'a [FragmentNode], usize),
  Layer4(&'a [FragmentNode], usize),
  Layer5(&'a [FragmentNode], usize),
  Layer6(Layer6Iter<'a>),
  Fragments(&'a [FragmentNode], usize),
  NegativeChildren(Vec<&'a StackingContext>, usize),
  PositiveChildren(Vec<&'a StackingContext>, usize),
}

impl<'a> PaintOrderIterator<'a> {
  /// Creates a new paint order iterator for a stacking context
  pub fn new(context: &'a StackingContext) -> Self {
    Self {
      stack: vec![PaintOrderItem::Context(context)],
    }
  }
}

impl<'a> Iterator for PaintOrderIterator<'a> {
  type Item = &'a FragmentNode;

  fn next(&mut self) -> Option<Self::Item> {
    while let Some(item) = self.stack.pop() {
      match item {
        PaintOrderItem::Context(ctx) => {
          // Push items in reverse order (last pushed = first processed)
          // Layer 7: Positive z-index children
          let positive = ctx.positive_z_children();
          if !positive.is_empty() {
            self
              .stack
              .push(PaintOrderItem::PositiveChildren(positive, 0));
          }

          // Layer 6: Positioned with z-index 0 or auto
          let layer6 = ctx.layer6_iter();
          if !layer6.is_done() {
            self.stack.push(PaintOrderItem::Layer6(layer6));
          }

          // Layer 5: Inline-level
          if !ctx.layer5_inlines.is_empty() {
            self
              .stack
              .push(PaintOrderItem::Layer5(&ctx.layer5_inlines, 0));
          }

          // Layer 4: Floats
          if !ctx.layer4_floats.is_empty() {
            self
              .stack
              .push(PaintOrderItem::Layer4(&ctx.layer4_floats, 0));
          }

          // Layer 3: Block-level
          if !ctx.layer3_blocks.is_empty() {
            self
              .stack
              .push(PaintOrderItem::Layer3(&ctx.layer3_blocks, 0));
          }

          // Layer 2: Negative z-index children
          let negative = ctx.negative_z_children();
          if !negative.is_empty() {
            self
              .stack
              .push(PaintOrderItem::NegativeChildren(negative, 0));
          }

          // Layer 1: Background and borders (root fragments)
          if !ctx.fragments.is_empty() {
            self
              .stack
              .push(PaintOrderItem::Fragments(&ctx.fragments, 0));
          }
        }
        PaintOrderItem::Fragments(fragments, idx) => {
          if idx < fragments.len() {
            self
              .stack
              .push(PaintOrderItem::Fragments(fragments, idx + 1));
            return Some(&fragments[idx]);
          }
        }
        PaintOrderItem::Layer3(fragments, idx) => {
          if idx < fragments.len() {
            self.stack.push(PaintOrderItem::Layer3(fragments, idx + 1));
            return Some(&fragments[idx]);
          }
        }
        PaintOrderItem::Layer4(fragments, idx) => {
          if idx < fragments.len() {
            self.stack.push(PaintOrderItem::Layer4(fragments, idx + 1));
            return Some(&fragments[idx]);
          }
        }
        PaintOrderItem::Layer5(fragments, idx) => {
          if idx < fragments.len() {
            self.stack.push(PaintOrderItem::Layer5(fragments, idx + 1));
            return Some(&fragments[idx]);
          }
        }
        PaintOrderItem::Layer6(mut iter) => {
          let Some(item) = iter.next() else {
            continue;
          };
          if !iter.is_done() {
            self.stack.push(PaintOrderItem::Layer6(iter));
          }
          match item {
            Layer6Item::Positioned(frag) => return Some(&frag.fragment),
            Layer6Item::ZeroContext(ctx) => {
              self.stack.push(PaintOrderItem::Context(ctx));
            }
          }
        }
        PaintOrderItem::NegativeChildren(children, idx) => {
          if idx < children.len() {
            self
              .stack
              .push(PaintOrderItem::NegativeChildren(children.clone(), idx + 1));
            self.stack.push(PaintOrderItem::Context(children[idx]));
          }
        }
        PaintOrderItem::PositiveChildren(children, idx) => {
          if idx < children.len() {
            self
              .stack
              .push(PaintOrderItem::PositiveChildren(children.clone(), idx + 1));
            self.stack.push(PaintOrderItem::Context(children[idx]));
          }
        }
      }
    }
    None
  }
}

impl StackingContext {
  /// Returns an iterator that yields fragments in paint order
  pub fn iter_paint_order(&self) -> PaintOrderIterator<'_> {
    PaintOrderIterator::new(self)
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::css::types::Transform;
  use crate::geometry::Point;
  use crate::geometry::Rect;
  use crate::style::types::TransformOrigin;
  use crate::style::types::WillChange;
  use crate::style::types::WillChangeHint;
  use crate::style::values::Length;
  use std::sync::Arc;

  // Helper function to create a simple fragment
  fn create_block_fragment(x: f32, y: f32, width: f32, height: f32) -> FragmentNode {
    FragmentNode::new_block(Rect::from_xywh(x, y, width, height), vec![])
  }

  fn create_text_fragment(x: f32, y: f32, width: f32, height: f32, text: &str) -> FragmentNode {
    FragmentNode::new_text(Rect::from_xywh(x, y, width, height), text.to_string(), 12.0)
  }

  // Stacking context creation tests

  #[test]
  fn test_creates_stacking_context_root() {
    let style = ComputedStyle::default();
    assert!(creates_stacking_context(&style, None, true));
  }

  #[test]
  fn test_creates_stacking_context_not_root() {
    let style = ComputedStyle::default();
    assert!(!creates_stacking_context(&style, None, false));
  }

  #[test]
  fn test_creates_stacking_context_positioned_with_z_index() {
    let mut style = ComputedStyle::default();
    style.position = Position::Relative;
    style.z_index = Some(1);
    assert!(creates_stacking_context(&style, None, false));
  }

  #[test]
  fn test_creates_stacking_context_positioned_with_zero_z_index() {
    let mut style = ComputedStyle::default();
    style.position = Position::Relative;
    style.z_index = Some(0); // explicit zero still creates stacking context
    assert!(creates_stacking_context(&style, None, false));
  }

  #[test]
  fn test_positioned_overflow_hidden_does_not_create_stacking_context() {
    let mut style = ComputedStyle::default();
    style.position = Position::Relative;
    style.overflow_x = Overflow::Hidden;
    style.overflow_y = Overflow::Hidden;
    style.z_index = None;
    assert!(!creates_stacking_context(&style, None, false));
    assert_eq!(get_stacking_context_reason(&style, None, false), None);
  }

  #[test]
  fn test_creates_stacking_context_fixed() {
    let mut style = ComputedStyle::default();
    style.position = Position::Fixed;
    assert!(creates_stacking_context(&style, None, false));
  }

  #[test]
  fn test_creates_stacking_context_sticky() {
    let mut style = ComputedStyle::default();
    style.position = Position::Sticky;
    assert!(creates_stacking_context(&style, None, false));
  }

  #[test]
  fn test_creates_stacking_context_opacity() {
    let mut style = ComputedStyle::default();
    style.opacity = 0.5;
    assert!(creates_stacking_context(&style, None, false));
  }

  #[test]
  fn test_creates_stacking_context_opacity_zero() {
    let mut style = ComputedStyle::default();
    style.opacity = 0.0;
    assert!(creates_stacking_context(&style, None, false));
  }

  #[test]
  fn test_creates_stacking_context_opacity_one() {
    let mut style = ComputedStyle::default();
    style.opacity = 1.0;
    assert!(!creates_stacking_context(&style, None, false));
  }

  #[test]
  fn test_backface_visibility_hidden_does_not_create_stacking_context() {
    let mut style = ComputedStyle::default();
    style.backface_visibility = crate::style::types::BackfaceVisibility::Hidden;
    assert!(!creates_stacking_context(&style, None, false));
    assert_eq!(get_stacking_context_reason(&style, None, false), None);
  }

  #[test]
  fn test_creates_stacking_context_will_change_transform() {
    let mut style = ComputedStyle::default();
    style.will_change = WillChange::Hints(vec![WillChangeHint::Property("transform".into())]);
    assert!(creates_stacking_context(&style, None, false));
    assert_eq!(
      get_stacking_context_reason(&style, None, false),
      Some(StackingContextReason::WillChange)
    );
  }

  #[test]
  fn test_creates_stacking_context_containment() {
    let mut style = ComputedStyle::default();
    style.containment =
      crate::style::types::Containment::with_flags(false, false, false, false, true);
    assert!(creates_stacking_context(&style, None, false));
    assert_eq!(
      get_stacking_context_reason(&style, None, false),
      Some(StackingContextReason::Containment)
    );
  }

  #[test]
  fn test_creates_stacking_context_flex_item_with_z_index() {
    let mut parent_style = ComputedStyle::default();
    parent_style.display = Display::Flex;

    let mut child_style = ComputedStyle::default();
    child_style.z_index = Some(1);

    assert!(creates_stacking_context(
      &child_style,
      Some(&parent_style),
      false
    ));
  }

  #[test]
  fn test_creates_stacking_context_grid_item_with_z_index() {
    let mut parent_style = ComputedStyle::default();
    parent_style.display = Display::Grid;

    let mut child_style = ComputedStyle::default();
    child_style.z_index = Some(1);

    assert!(creates_stacking_context(
      &child_style,
      Some(&parent_style),
      false
    ));
  }

  // StackingContext struct tests

  #[test]
  fn test_stacking_context_new() {
    let sc = StackingContext::new(5);
    assert_eq!(sc.z_index, 5);
    assert!(sc.children.is_empty());
    assert!(sc.fragments.is_empty());
  }

  #[test]
  fn test_stacking_context_root() {
    let sc = StackingContext::root();
    assert_eq!(sc.z_index, 0);
    assert_eq!(sc.reason, StackingContextReason::Root);
  }

  #[test]
  fn test_stacking_context_with_reason() {
    let sc = StackingContext::with_reason(10, StackingContextReason::Opacity, 5);
    assert_eq!(sc.z_index, 10);
    assert_eq!(sc.reason, StackingContextReason::Opacity);
    assert_eq!(sc.tree_order, 5);
  }

  #[test]
  fn test_stacking_context_add_child() {
    let mut parent = StackingContext::new(0);
    let child = StackingContext::new(1);
    parent.add_child(child);
    assert_eq!(parent.children.len(), 1);
    assert_eq!(parent.children[0].z_index, 1);
  }

  #[test]
  fn test_stacking_context_negative_z_children() {
    let mut parent = StackingContext::new(0);
    parent.add_child(StackingContext::with_reason(
      -5,
      StackingContextReason::PositionedWithZIndex,
      1,
    ));
    parent.add_child(StackingContext::with_reason(
      -1,
      StackingContextReason::PositionedWithZIndex,
      2,
    ));
    parent.add_child(StackingContext::with_reason(
      0,
      StackingContextReason::Root,
      3,
    ));
    parent.add_child(StackingContext::with_reason(
      1,
      StackingContextReason::PositionedWithZIndex,
      4,
    ));

    let negative = parent.negative_z_children();
    assert_eq!(negative.len(), 2);
    assert_eq!(negative[0].z_index, -5); // Most negative first
    assert_eq!(negative[1].z_index, -1);
  }

  #[test]
  fn test_stacking_context_positive_z_children() {
    let mut parent = StackingContext::new(0);
    parent.add_child(StackingContext::with_reason(
      -1,
      StackingContextReason::PositionedWithZIndex,
      1,
    ));
    parent.add_child(StackingContext::with_reason(
      5,
      StackingContextReason::PositionedWithZIndex,
      2,
    ));
    parent.add_child(StackingContext::with_reason(
      1,
      StackingContextReason::PositionedWithZIndex,
      3,
    ));
    parent.add_child(StackingContext::with_reason(
      0,
      StackingContextReason::Root,
      4,
    ));

    let positive = parent.positive_z_children();
    assert_eq!(positive.len(), 2);
    assert_eq!(positive[0].z_index, 1); // Least positive first
    assert_eq!(positive[1].z_index, 5);
  }

  #[test]
  fn test_stacking_context_zero_z_children() {
    let mut parent = StackingContext::new(0);
    parent.add_child(StackingContext::with_reason(
      0,
      StackingContextReason::Opacity,
      1,
    ));
    parent.add_child(StackingContext::with_reason(
      0,
      StackingContextReason::Transform,
      2,
    ));
    parent.add_child(StackingContext::with_reason(
      1,
      StackingContextReason::PositionedWithZIndex,
      3,
    ));

    let zero = parent.zero_z_children();
    assert_eq!(zero.len(), 2);
    // Should be in tree order
    assert_eq!(zero[0].tree_order, 1);
    assert_eq!(zero[1].tree_order, 2);
  }

  #[test]
  fn test_stacking_context_sort_children() {
    let mut parent = StackingContext::new(0);
    parent.add_child(StackingContext::with_reason(
      5,
      StackingContextReason::PositionedWithZIndex,
      1,
    ));
    parent.add_child(StackingContext::with_reason(
      -2,
      StackingContextReason::PositionedWithZIndex,
      2,
    ));
    parent.add_child(StackingContext::with_reason(
      0,
      StackingContextReason::Opacity,
      3,
    ));
    parent.add_child(StackingContext::with_reason(
      1,
      StackingContextReason::PositionedWithZIndex,
      4,
    ));

    parent.sort_children();

    assert_eq!(parent.children[0].z_index, -2);
    assert_eq!(parent.children[1].z_index, 0);
    assert_eq!(parent.children[2].z_index, 1);
    assert_eq!(parent.children[3].z_index, 5);
  }

  #[test]
  fn test_stacking_context_sort_children_equal_z_index_uses_tree_order() {
    let mut parent = StackingContext::new(0);
    parent.add_child(StackingContext::with_reason(
      0,
      StackingContextReason::Opacity,
      3,
    ));
    parent.add_child(StackingContext::with_reason(
      0,
      StackingContextReason::Transform,
      1,
    ));
    parent.add_child(StackingContext::with_reason(
      0,
      StackingContextReason::FixedPositioning,
      2,
    ));

    parent.sort_children();

    // Should be sorted by tree order for equal z-index
    assert_eq!(parent.children[0].tree_order, 1);
    assert_eq!(parent.children[1].tree_order, 2);
    assert_eq!(parent.children[2].tree_order, 3);
  }

  // Build stacking tree tests

  #[test]
  fn test_build_stacking_tree_single_fragment() {
    let fragment = create_block_fragment(0.0, 0.0, 100.0, 100.0);
    let tree = build_stacking_tree(&fragment, None, true);

    assert_eq!(tree.z_index, 0);
    assert_eq!(tree.reason, StackingContextReason::Root);
    assert!(!tree.fragments.is_empty());
  }

  #[test]
  fn test_build_stacking_tree_with_children() {
    let child1 = create_block_fragment(0.0, 0.0, 50.0, 50.0);
    let child2 = create_block_fragment(50.0, 0.0, 50.0, 50.0);
    let root = FragmentNode::new_block(
      Rect::from_xywh(0.0, 0.0, 100.0, 100.0),
      vec![child1, child2],
    );

    let tree = build_stacking_tree(&root, None, true);

    assert_eq!(tree.reason, StackingContextReason::Root);
    // Root fragment + children classified
    assert!(tree.total_fragment_count() >= 3);
  }

  #[test]
  fn test_build_stacking_tree_with_style() {
    let fragment = create_block_fragment(0.0, 0.0, 100.0, 100.0);

    let mut style = ComputedStyle::default();
    style.opacity = 0.5;

    let tree = build_stacking_tree(&fragment, Some(&style), false);
    assert_eq!(tree.reason, StackingContextReason::Opacity);
  }

  #[test]
  fn build_stacking_tree_does_not_prune_visibility_hidden() {
    let root_style = {
      let mut style = ComputedStyle::default();
      style.display = Display::Block;
      Arc::new(style)
    };

    let hidden_style = {
      let mut style = ComputedStyle::default();
      style.display = Display::Block;
      style.opacity = 0.5;
      style.visibility = crate::style::computed::Visibility::Hidden;
      Arc::new(style)
    };

    let child_style = {
      let mut style = ComputedStyle::default();
      style.display = Display::Block;
      style.visibility = crate::style::computed::Visibility::Visible;
      Arc::new(style)
    };

    let visible_child =
      FragmentNode::new_block_styled(Rect::from_xywh(0.0, 0.0, 10.0, 10.0), vec![], child_style);
    let hidden_parent = FragmentNode::new_block_styled(
      Rect::from_xywh(0.0, 0.0, 10.0, 10.0),
      vec![visible_child],
      hidden_style,
    );
    let root = FragmentNode::new_block_styled(
      Rect::from_xywh(0.0, 0.0, 10.0, 10.0),
      vec![hidden_parent],
      root_style,
    );

    let tree = build_stacking_tree_from_fragment_tree(&root);
    assert_eq!(tree.children.len(), 1);
    assert_eq!(tree.children[0].reason, StackingContextReason::Opacity);
    assert!(
      tree.children[0].total_fragment_count() >= 2,
      "expected hidden context to retain visible descendants"
    );
  }

  // Fragment count tests

  #[test]
  fn test_fragment_count() {
    let mut sc = StackingContext::new(0);
    sc.fragments
      .push(create_block_fragment(0.0, 0.0, 10.0, 10.0));
    sc.layer3_blocks
      .push(create_block_fragment(0.0, 0.0, 10.0, 10.0));
    sc.layer5_inlines
      .push(create_text_fragment(0.0, 0.0, 10.0, 10.0, "test"));

    assert_eq!(sc.fragment_count(), 3);
  }

  #[test]
  fn test_total_fragment_count() {
    let mut parent = StackingContext::new(0);
    parent
      .fragments
      .push(create_block_fragment(0.0, 0.0, 10.0, 10.0));

    let mut child = StackingContext::new(1);
    child
      .fragments
      .push(create_block_fragment(0.0, 0.0, 10.0, 10.0));
    child
      .layer3_blocks
      .push(create_block_fragment(0.0, 0.0, 10.0, 10.0));

    parent.add_child(child);

    assert_eq!(parent.total_fragment_count(), 3);
  }

  // Paint order iterator tests

  #[test]
  fn test_paint_order_iterator_empty() {
    let sc = StackingContext::new(0);
    let count = sc.iter_paint_order().count();
    assert_eq!(count, 0);
  }

  #[test]
  fn test_paint_order_iterator_single_fragment() {
    let mut sc = StackingContext::new(0);
    sc.fragments
      .push(create_block_fragment(0.0, 0.0, 100.0, 100.0));

    let count = sc.iter_paint_order().count();
    assert_eq!(count, 1);
  }

  #[test]
  fn test_paint_order_iterator_multiple_layers() {
    let mut sc = StackingContext::new(0);
    sc.fragments
      .push(create_block_fragment(0.0, 0.0, 10.0, 10.0)); // Layer 1
    sc.layer3_blocks
      .push(create_block_fragment(10.0, 0.0, 10.0, 10.0)); // Layer 3
    sc.layer5_inlines
      .push(create_text_fragment(20.0, 0.0, 10.0, 10.0, "test")); // Layer 5

    let fragments: Vec<_> = sc.iter_paint_order().collect();
    assert_eq!(fragments.len(), 3);

    // Verify order: Layer 1 first, then Layer 3, then Layer 5
    assert_eq!(fragments[0].bounds.x(), 0.0); // Layer 1
    assert_eq!(fragments[1].bounds.x(), 10.0); // Layer 3
    assert_eq!(fragments[2].bounds.x(), 20.0); // Layer 5
  }

  #[test]
  fn test_paint_order_with_z_index_children() {
    let mut root = StackingContext::new(0);
    root
      .fragments
      .push(create_block_fragment(0.0, 0.0, 100.0, 100.0));

    // Negative z-index child
    let mut neg_child =
      StackingContext::with_reason(-1, StackingContextReason::PositionedWithZIndex, 1);
    neg_child
      .fragments
      .push(create_block_fragment(10.0, 10.0, 20.0, 20.0));
    root.add_child(neg_child);

    // Positive z-index child
    let mut pos_child =
      StackingContext::with_reason(1, StackingContextReason::PositionedWithZIndex, 2);
    pos_child
      .fragments
      .push(create_block_fragment(30.0, 30.0, 20.0, 20.0));
    root.add_child(pos_child);

    let fragments: Vec<_> = root.iter_paint_order().collect();
    assert_eq!(fragments.len(), 3);

    // Order should be: root background, negative child, positive child
    assert_eq!(fragments[0].bounds.x(), 0.0); // Root (Layer 1)
    assert_eq!(fragments[1].bounds.x(), 10.0); // Negative child (Layer 2)
    assert_eq!(fragments[2].bounds.x(), 30.0); // Positive child (Layer 7)
  }

  #[test]
  fn paint_order_interleaves_zero_z_contexts_with_positioned() {
    let mut root = StackingContext::new(0);
    root
      .fragments
      .push(create_block_fragment(0.0, 0.0, 10.0, 10.0));

    let mut zero_child = StackingContext::with_reason(0, StackingContextReason::Opacity, 1);
    zero_child
      .fragments
      .push(create_block_fragment(10.0, 0.0, 10.0, 10.0));

    let positioned = OrderedFragment::new(create_block_fragment(20.0, 0.0, 10.0, 10.0), 2);
    root.layer6_positioned.push(positioned);
    root.add_child(zero_child);

    let fragments: Vec<_> = root.iter_paint_order().collect();
    let origins: Vec<f32> = fragments.iter().map(|f| f.bounds.x()).collect();
    assert_eq!(origins, vec![0.0, 10.0, 20.0]);
  }

  #[test]
  fn layer6_iter_merges_positioned_and_zero_contexts_by_tree_order() {
    let mut root = StackingContext::new(0);

    // Positioned fragments are already recorded in tree order.
    root.layer6_positioned.push(OrderedFragment::new(
      create_block_fragment(0.0, 0.0, 10.0, 10.0),
      1,
    ));
    root.layer6_positioned.push(OrderedFragment::new(
      create_block_fragment(0.0, 0.0, 10.0, 10.0),
      4,
    ));
    root.layer6_positioned.push(OrderedFragment::new(
      create_block_fragment(0.0, 0.0, 10.0, 10.0),
      6,
    ));

    // Add multiple z-index: 0 child contexts whose tree_order values interleave with the
    // positioned fragments above.
    root.add_child(StackingContext::with_reason(
      0,
      StackingContextReason::Opacity,
      5,
    ));
    root.add_child(StackingContext::with_reason(
      0,
      StackingContextReason::Opacity,
      2,
    ));
    root.add_child(StackingContext::with_reason(
      0,
      StackingContextReason::Opacity,
      3,
    ));

    // The merge iterator relies on children being sorted by (z_index, tree_order).
    root.sort_children();

    let items: Vec<(char, usize)> = root
      .layer6_iter()
      .map(|item| match item {
        Layer6Item::Positioned(frag) => ('p', frag.tree_order),
        Layer6Item::ZeroContext(ctx) => ('c', ctx.tree_order),
      })
      .collect();

    assert_eq!(
      items,
      vec![('p', 1), ('c', 2), ('c', 3), ('p', 4), ('c', 5), ('p', 6)]
    );
  }

  // Reason tests

  #[test]
  fn test_get_stacking_context_reason_root() {
    let style = ComputedStyle::default();
    let reason = get_stacking_context_reason(&style, None, true);
    assert_eq!(reason, Some(StackingContextReason::Root));
  }

  #[test]
  fn test_get_stacking_context_reason_opacity() {
    let mut style = ComputedStyle::default();
    style.opacity = 0.5;
    let reason = get_stacking_context_reason(&style, None, false);
    assert_eq!(reason, Some(StackingContextReason::Opacity));
  }

  #[test]
  fn test_get_stacking_context_reason_positioned_with_z_index() {
    let mut style = ComputedStyle::default();
    style.position = Position::Relative;
    style.z_index = Some(5);
    let reason = get_stacking_context_reason(&style, None, false);
    assert_eq!(reason, Some(StackingContextReason::PositionedWithZIndex));
  }

  #[test]
  fn test_get_stacking_context_reason_none() {
    let style = ComputedStyle::default();
    let reason = get_stacking_context_reason(&style, None, false);
    assert_eq!(reason, None);
  }

  // Bounds computation tests

  #[test]
  fn test_compute_bounds() {
    let mut sc = StackingContext::new(0);
    sc.fragments
      .push(create_block_fragment(0.0, 0.0, 50.0, 50.0));
    sc.layer3_blocks
      .push(create_block_fragment(40.0, 40.0, 60.0, 60.0));

    sc.compute_bounds(None, None);

    // Should encompass both fragments: (0,0) to (100, 100)
    assert_eq!(sc.bounds.min_x(), 0.0);
    assert_eq!(sc.bounds.min_y(), 0.0);
    assert_eq!(sc.bounds.max_x(), 100.0);
    assert_eq!(sc.bounds.max_y(), 100.0);
  }

  #[test]
  fn test_compute_bounds_includes_descendant_paint_overflow() {
    // Regression coverage for descendant paint overflow (e.g. box-shadows) that is painted via
    // normal fragment recursion. These descendants are not stored as top-level entries in the
    // stacking context layer lists, so `compute_bounds` must recurse to find them.
    let mut sc = StackingContext::new(0);
    sc.offset_from_parent_context = Point::new(40.0, 40.0);

    sc.fragments.push(FragmentNode::new_block_styled(
      Rect::from_xywh(40.0, 40.0, 20.0, 20.0),
      vec![],
      Arc::new(ComputedStyle::default()),
    ));

    let mut shadow_style = ComputedStyle::default();
    shadow_style.box_shadow = vec![crate::css::types::BoxShadow {
      offset_x: crate::style::values::Length::px(0.0),
      offset_y: crate::style::values::Length::px(0.0),
      blur_radius: crate::style::values::Length::px(0.0),
      spread_radius: crate::style::values::Length::px(10.0),
      color: crate::style::color::Rgba::RED,
      inset: false,
    }];
    let shadow_style = Arc::new(shadow_style);

    // A non-stacking wrapper with a grandchild shadow.
    let inner = FragmentNode::new_block_styled(
      Rect::from_xywh(0.0, 0.0, 20.0, 20.0),
      vec![],
      shadow_style,
    );
    let wrapper = FragmentNode::new_block_styled(
      Rect::from_xywh(0.0, 0.0, 20.0, 20.0),
      vec![inner],
      Arc::new(ComputedStyle::default()),
    );
    sc.layer3_blocks.push(wrapper);

    sc.compute_bounds(None, None);

    // Inner box-shadow spreads 10px in all directions, so bounds should cover 30..70 in both axes
    // after applying the stacking context offset.
    assert_eq!(sc.bounds, Rect::from_xywh(30.0, 30.0, 40.0, 40.0));
  }

  #[test]
  fn test_compute_bounds_includes_positioned_descendant_paint_overflow() {
    // Positioned descendants that do *not* create their own stacking context are painted via
    // layer 6, so we must not rely on normal recursion from their non-positioned ancestors. This
    // regression ensures `compute_bounds` includes paint overflow from descendants under those
    // positioned fragments as well.
    let mut sc = StackingContext::new(0);
    sc.offset_from_parent_context = Point::new(40.0, 40.0);

    sc.fragments.push(FragmentNode::new_block_styled(
      Rect::from_xywh(40.0, 40.0, 20.0, 20.0),
      vec![],
      Arc::new(ComputedStyle::default()),
    ));

    let mut shadow_style = ComputedStyle::default();
    shadow_style.box_shadow = vec![crate::css::types::BoxShadow {
      offset_x: crate::style::values::Length::px(0.0),
      offset_y: crate::style::values::Length::px(0.0),
      blur_radius: crate::style::values::Length::px(0.0),
      spread_radius: crate::style::values::Length::px(10.0),
      color: crate::style::color::Rgba::RED,
      inset: false,
    }];
    let shadow_style = Arc::new(shadow_style);

    let mut positioned_style = ComputedStyle::default();
    positioned_style.position = Position::Relative;
    let positioned_style = Arc::new(positioned_style);

    let inner = FragmentNode::new_block_styled(
      Rect::from_xywh(0.0, 0.0, 20.0, 20.0),
      vec![],
      shadow_style,
    );
    let positioned = FragmentNode::new_block_styled(
      Rect::from_xywh(0.0, 0.0, 20.0, 20.0),
      vec![inner],
      positioned_style,
    );

    // Non-positioned wrapper at (5,5) with a positioned child. In the real stacking tree builder,
    // the positioned child would be translated and stored in `layer6_positioned`.
    let wrapper_origin = Point::new(5.0, 5.0);
    let wrapper = FragmentNode::new_block_styled(
      Rect::new(wrapper_origin, positioned.bounds.size),
      vec![positioned.clone()],
      Arc::new(ComputedStyle::default()),
    );
    sc.layer3_blocks.push(wrapper);

    let mut translated = positioned.clone();
    translated.translate_root_in_place(wrapper_origin);
    sc
      .layer6_positioned
      .push(OrderedFragment::new(translated, 0));

    sc.compute_bounds(None, None);

    assert_eq!(sc.bounds, Rect::from_xywh(35.0, 35.0, 40.0, 40.0));
  }

  #[test]
  fn test_compute_bounds_includes_children() {
    let mut parent = StackingContext::new(0);
    let mut child = StackingContext::new(0);
    child
      .fragments
      .push(create_block_fragment(0.0, 0.0, 5.0, 5.0));

    parent.children.push(child);
    parent.compute_bounds(None, None);

    assert_eq!(parent.bounds, Rect::from_xywh(0.0, 0.0, 5.0, 5.0));
  }

  #[test]
  fn test_compute_bounds_respects_child_offset() {
    let mut parent = StackingContext::new(0);
    let mut child = StackingContext::new(0);
    child.offset_from_parent_context = Point::new(5.0, 5.0);
    child
      .fragments
      .push(create_block_fragment(0.0, 0.0, 10.0, 10.0));

    parent.children.push(child);
    parent.compute_bounds(None, None);

    assert_eq!(parent.bounds, Rect::from_xywh(5.0, 5.0, 10.0, 10.0));
  }

  #[test]
  fn test_compute_bounds_translates_children_by_parent_offset() {
    let mut parent = StackingContext::new(0);
    parent.offset_from_parent_context = Point::new(50.0, 50.0);
    parent
      .fragments
      .push(create_block_fragment(0.0, 0.0, 20.0, 20.0));

    let mut child = StackingContext::new(0);
    child.offset_from_parent_context = Point::new(10.0, 10.0);
    child
      .fragments
      .push(create_block_fragment(0.0, 0.0, 100.0, 10.0));

    parent.children.push(child);
    parent.compute_bounds(None, None);

    assert_eq!(parent.bounds, Rect::from_xywh(50.0, 50.0, 110.0, 20.0));
  }

  #[test]
  fn test_compute_bounds_applies_parent_perspective_to_children() {
    let mut parent_style = ComputedStyle::default();
    parent_style.perspective = Some(Length::px(100.0));
    parent_style.perspective_origin = TransformOrigin {
      x: Length::px(0.0),
      y: Length::px(0.0),
      z: Length::px(0.0),
    };
    let parent_fragment = FragmentNode::new_block_styled(
      Rect::from_xywh(0.0, 0.0, 100.0, 20.0),
      vec![],
      Arc::new(parent_style),
    );

    let mut child_style = ComputedStyle::default();
    child_style.transform = vec![Transform::TranslateZ(Length::px(50.0))];
    let child_fragment = FragmentNode::new_block_styled(
      Rect::from_xywh(0.0, 0.0, 100.0, 20.0),
      vec![],
      Arc::new(child_style),
    );

    let mut parent = StackingContext::new(0);
    parent.fragments.push(parent_fragment);

    let mut child = StackingContext::new(0);
    child.fragments.push(child_fragment);

    parent.children.push(child);
    parent.compute_bounds(None, None);

    assert_eq!(parent.bounds, Rect::from_xywh(0.0, 0.0, 200.0, 40.0));
  }

  // Layer classification tests

  #[test]
  fn test_add_fragment_to_layer_block() {
    let mut sc = StackingContext::new(0);
    let fragment = create_block_fragment(0.0, 0.0, 100.0, 100.0);
    let mut style = ComputedStyle::default();
    style.display = Display::Block; // Default is Inline, need Block for block layer

    sc.add_fragment_to_layer(fragment, Some(&style), 0);

    assert_eq!(sc.layer3_blocks.len(), 1);
  }

  #[test]
  fn test_add_fragment_to_layer_inline() {
    let mut sc = StackingContext::new(0);
    let fragment = create_text_fragment(0.0, 0.0, 50.0, 20.0, "test");
    let mut style = ComputedStyle::default();
    style.display = Display::Inline;

    sc.add_fragment_to_layer(fragment, Some(&style), 0);

    assert_eq!(sc.layer5_inlines.len(), 1);
  }

  #[test]
  fn test_add_fragment_to_layer_positioned() {
    let mut sc = StackingContext::new(0);
    let fragment = create_block_fragment(0.0, 0.0, 100.0, 100.0);
    let mut style = ComputedStyle::default();
    style.position = Position::Relative;
    // z_index = 0 (default), so goes to layer 6

    sc.add_fragment_to_layer(fragment, Some(&style), 0);

    assert_eq!(sc.layer6_positioned.len(), 1);
  }
}
