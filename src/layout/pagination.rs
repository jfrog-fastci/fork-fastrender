//! Pagination helpers that honor CSS @page rules and margin boxes.

use std::cmp::Ordering;
#[cfg(test)]
use std::collections::BTreeMap;
use std::collections::HashMap;
use std::sync::Arc;

use crate::css::types::{CollectedPageRule, PageMarginArea};
use crate::geometry::{Point, Rect, Size};
use crate::layout::axis::{FragmentAxes, PhysicalAxis};
use crate::layout::engine::{LayoutConfig, LayoutEngine};
use crate::layout::formatting_context::{
  layout_style_fingerprint, set_fragmentainer_block_size_hint, LayoutError,
};
use crate::layout::fragmentation::{
  apply_float_parallel_flow_forced_break_shifts, apply_grid_parallel_flow_forced_break_shifts,
  clip_node, collect_atomic_ranges_with_axes, collect_forced_boundaries_for_pagination_with_axes,
  collect_table_repetition_info_with_axes, normalize_atomic_ranges, normalize_fragment_margins,
  parallel_flow_content_extent, propagate_fragment_metadata, AtomicRange, ForcedBoundary,
  FragmentAxis, FragmentationContext, TableRepetitionInfo,
};
use crate::layout::running_elements::{running_elements_for_page, running_elements_for_page_fragment};
use crate::layout::running_strings::{collect_string_set_events, StringSetEvent};
use crate::style::content::{
  ContentContext, ContentItem, ContentValue, CounterStyle, RunningElementValues,
  RunningStringValues,
};
use crate::style::display::{Display, FormattingContextType};
use crate::style::page::{resolve_page_style, PageSide, ResolvedPageStyle};
use crate::style::position::Position;
use crate::style::types::WritingMode;
use crate::style::{block_axis_is_horizontal, ComputedStyle};
use crate::text::font_loader::FontContext;
use crate::tree::box_tree::{BoxNode, BoxTree, CrossOriginAttribute, ReplacedType};
use crate::tree::fragment_tree::{FragmentContent, FragmentNode};

/// Controls how paginated pages are positioned in the fragment tree.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum PageStacking {
  /// Translate each page along the block axis so they don't overlap.
  ///
  /// The provided gap is inserted between successive pages (clamped to >= 0).
  Stacked { gap: f32 },
  /// Leave all pages at the origin so they can be painted independently.
  Untranslated,
}

/// Options for pagination.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PaginateOptions {
  pub stacking: PageStacking,
}

impl Default for PaginateOptions {
  fn default() -> Self {
    Self {
      stacking: PageStacking::Stacked { gap: 0.0 },
    }
  }
}

const EPSILON: f32 = 0.01;

fn subtree_has_text(node: &FragmentNode) -> bool {
  if matches!(node.content, FragmentContent::Text { .. }) {
    return true;
  }
  node.children.iter().any(subtree_has_text)
}

fn pos_is_inside_atomic(pos: f32, atomic: &[AtomicRange]) -> bool {
  // Atomic ranges treat their endpoints as break-safe (see `atomic_containing` in
  // `layout::fragmentation`). Treat near-equal comparisons as outside the interval so pagination can
  // still place boundaries exactly at atomic endpoints.
  atomic.iter().any(|range| {
    pos > range.start + EPSILON && pos < range.end - EPSILON && range.end > range.start + EPSILON
  })
}

fn best_in_flow_block_boundary(
  node: &FragmentNode,
  abs_start: f32,
  axis: &FragmentAxis,
  parent_block_size: f32,
  start: f32,
  end: f32,
  atomic: &[AtomicRange],
  default_style: &ComputedStyle,
) -> Option<f32> {
  const MIN_FRACTION_OF_NEXT_BLOCK_ON_PAGE: f32 = 0.5;

  let node_block_size = axis.block_size(&node.bounds);
  if node_block_size <= EPSILON {
    return None;
  }
  let node_abs_end = abs_start + node_block_size;
  if node_abs_end <= start + EPSILON || abs_start >= end - EPSILON {
    return None;
  }

  let style = node
    .style
    .as_ref()
    .map(|s| s.as_ref())
    .unwrap_or(default_style);
  if style.float.is_floating() {
    return None;
  }

  let mut best: Option<f32> = None;
  for idx in 0..node.children.len().saturating_sub(1) {
    let child = &node.children[idx];
    let next = &node.children[idx + 1];
    if !child.content.is_block() || !next.content.is_block() {
      continue;
    }

    let (_child_abs_start, child_abs_end) =
      axis.flow_range(abs_start, parent_block_size, &child.bounds);
    let (next_abs_start, next_abs_end) = axis.flow_range(abs_start, parent_block_size, &next.bounds);

    let mut boundary = child_abs_end;
    if let Some(meta) = child.block_metadata.as_ref() {
      let mut candidate = child_abs_end + meta.margin_bottom;
      if candidate < child_abs_end {
        candidate = child_abs_end;
      }
      candidate = candidate.min(next_abs_start);
      boundary = candidate;
    }

    if boundary <= start + EPSILON || boundary > end + EPSILON {
      continue;
    }
    if pos_is_inside_atomic(boundary, atomic) {
      continue;
    }

    // Only apply this heuristic when the fragmentainer boundary would cut through the start of
    // `next` (i.e., `next` would be partially visible on this page), and that partial portion is a
    // small fraction of the block. This avoids forcing earlier breaks for "spacer" blocks where
    // clipping is acceptable (e.g. an empty div used for vertical space).
    if next_abs_start >= end - EPSILON || next_abs_end <= end + EPSILON {
      continue;
    }
    let next_block_size = next_abs_end - next_abs_start;
    if next_block_size <= EPSILON {
      continue;
    }
    if next_block_size > (end - start) + EPSILON {
      continue;
    }
    let fraction_on_page = ((end - next_abs_start).max(0.0) / next_block_size).min(1.0);
    if fraction_on_page >= MIN_FRACTION_OF_NEXT_BLOCK_ON_PAGE {
      continue;
    }
    if !subtree_has_text(next) {
      continue;
    }
    best = Some(best.map_or(boundary, |prev| prev.max(boundary)));
  }

  for child in node.children.iter() {
    let child_block_size = axis.block_size(&child.bounds);
    if child_block_size <= EPSILON {
      continue;
    }
    let child_abs_start = axis.flow_range(abs_start, node_block_size, &child.bounds).0;
    if let Some(candidate) = best_in_flow_block_boundary(
      child,
      child_abs_start,
      axis,
      child_block_size,
      start,
      end,
      atomic,
      default_style,
    ) {
      best = Some(best.map_or(candidate, |prev| prev.max(candidate)));
    }
  }

  best
}

fn opposite_page_side(side: PageSide) -> PageSide {
  match side {
    PageSide::Left => PageSide::Right,
    PageSide::Right => PageSide::Left,
  }
}

fn page_side_for_index(page_index: usize, first_page_side: PageSide) -> PageSide {
  if page_index % 2 == 0 {
    first_page_side
  } else {
    opposite_page_side(first_page_side)
  }
}

fn trim_ascii_whitespace(value: &str) -> &str {
  value.trim_matches(|c: char| matches!(c, '\u{0009}' | '\u{000A}' | '\u{000C}' | '\u{000D}' | ' '))
}

fn required_page_side(boundaries: &[ForcedBoundary], pos: f32) -> Option<PageSide> {
  boundaries
    .iter()
    .find(|b| (b.position - pos).abs() < EPSILON)
    .and_then(|b| b.page_side)
}

fn next_forced_boundary(boundaries: &[ForcedBoundary], start: f32, limit: f32) -> Option<f32> {
  boundaries
    .iter()
    .map(|b| b.position)
    .find(|p| *p > start + EPSILON && *p < limit - EPSILON)
}

fn dedup_forced_boundaries(mut boundaries: Vec<ForcedBoundary>) -> Vec<ForcedBoundary> {
  boundaries.sort_by(|a, b| {
    a.position
      .partial_cmp(&b.position)
      .unwrap_or(std::cmp::Ordering::Equal)
  });

  let mut deduped: Vec<ForcedBoundary> = Vec::new();
  for boundary in boundaries.drain(..) {
    if let Some(last) = deduped.last_mut() {
      if (last.position - boundary.position).abs() < EPSILON {
        match (last.page_side, boundary.page_side) {
          (None, side) => last.page_side = side,
          (side, None) => last.page_side = side,
          (Some(a), Some(b)) if a == b => last.page_side = Some(a),
          // Conflicting side constraints at the same boundary are unsatisfiable; drop the side
          // requirement and treat it as a generic forced break.
          (Some(_), Some(_)) => last.page_side = None,
        }
        continue;
      }
    }
    deduped.push(boundary);
  }
  deduped
}

fn split_atomic_ranges_at_forced_boundaries(
  atomic_ranges: &mut Vec<AtomicRange>,
  boundaries: &[ForcedBoundary],
) {
  if atomic_ranges.is_empty() || boundaries.is_empty() {
    return;
  }

  let mut points: Vec<f32> = boundaries.iter().map(|b| b.position).collect();
  points.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
  points.dedup_by(|a, b| (*a - *b).abs() < EPSILON);

  let mut split: Vec<AtomicRange> = Vec::with_capacity(atomic_ranges.len());
  for range in atomic_ranges.iter().copied() {
    let mut start = range.start;
    for pos in points.iter().copied() {
      if pos <= start + EPSILON || pos >= range.end - EPSILON {
        continue;
      }
      split.push(AtomicRange { start, end: pos });
      start = pos;
    }
    split.push(AtomicRange {
      start,
      end: range.end,
    });
  }

  atomic_ranges.clear();
  atomic_ranges.extend(split);
  normalize_atomic_ranges(atomic_ranges);
}

#[derive(Debug, Clone, Copy)]
struct BoxAxisRange {
  start: f32,
  end: f32,
}

fn fragment_box_id_for_axis_range(node: &FragmentNode) -> Option<usize> {
  match &node.content {
    FragmentContent::Block { box_id } => *box_id,
    FragmentContent::Inline { box_id, .. } => *box_id,
    FragmentContent::Text { box_id, .. } => *box_id,
    FragmentContent::Replaced { box_id, .. } => *box_id,
    _ => None,
  }
}

fn collect_box_axis_ranges(
  node: &FragmentNode,
  abs_start: f32,
  parent_block_size: f32,
  axes: FragmentAxes,
  out: &mut HashMap<usize, BoxAxisRange>,
) {
  let logical = node.logical_bounds();
  let start = axes.abs_block_start(&logical, abs_start, parent_block_size);
  let block_size = axes.block_size(&logical).max(0.0);
  let end = start + block_size;

  if let Some(box_id) = fragment_box_id_for_axis_range(node) {
    if box_id != 0 {
      out
        .entry(box_id)
        .and_modify(|range| {
          range.start = range.start.min(start);
          range.end = range.end.max(end);
        })
        .or_insert(BoxAxisRange { start, end });
    }
  }

  for child in node.children.iter() {
    collect_box_axis_ranges(child, start, block_size, axes, out);
  }
}

#[derive(Debug, Clone)]
struct CachedLayout {
  root: FragmentNode,
  total_height: f32,
  forced_boundaries: Vec<ForcedBoundary>,
  atomic_ranges: Vec<AtomicRange>,
  table_repetitions: Vec<TableRepetitionInfo>,
  page_name_transitions: Vec<PageNameTransition>,
  box_axis_ranges: HashMap<usize, BoxAxisRange>,
}

impl CachedLayout {
  fn from_root(
    mut root: FragmentNode,
    style: &ResolvedPageStyle,
    fallback_page_name: Option<&str>,
    axes: FragmentAxes,
  ) -> Self {
    let axis = FragmentAxis {
      block_is_horizontal: axes.block_axis() == PhysicalAxis::X,
      block_positive: axes.block_positive(),
    };
    let style_block_size = if axes.block_axis() == PhysicalAxis::X {
      style.content_size.width
    } else {
      style.content_size.height
    };

    apply_grid_parallel_flow_forced_break_shifts(&mut root, axes, style_block_size);
    apply_float_parallel_flow_forced_break_shifts(
      &mut root,
      axes,
      style_block_size,
      FragmentationContext::Page,
    );
    let page_name_transitions = collect_page_name_transitions(&root, &axis, fallback_page_name);

    let mut forced = collect_forced_boundaries_for_pagination_with_axes(&root, 0.0, axes);
    forced.extend(
      page_name_transitions
        .iter()
        .skip(1)
        .map(|transition| ForcedBoundary {
          position: transition.position,
          page_side: None,
        }),
    );
    let mut atomic_ranges = Vec::new();
    collect_atomic_ranges_with_axes(
      &root,
      0.0,
      axes,
      &mut atomic_ranges,
      FragmentationContext::Page,
      Some(style_block_size),
    );
    normalize_atomic_ranges(&mut atomic_ranges);

    let content_height =
      parallel_flow_content_extent(&root, axes, Some(style_block_size), FragmentationContext::Page);
    let total_height = if content_height > EPSILON {
      content_height
    } else {
      style_block_size
    };
    forced.push(ForcedBoundary {
      position: total_height,
      page_side: None,
    });
    forced = dedup_forced_boundaries(forced);
    // Forced breaks override `break-inside: avoid-*` semantics; ensure atomic ranges don't span
    // over forced boundaries so pagination doesn't incorrectly skip mandated breaks.
    split_atomic_ranges_at_forced_boundaries(&mut atomic_ranges, &forced);

    let table_repetitions =
      collect_table_repetition_info_with_axes(&root, axes, FragmentationContext::Page);

    let mut box_axis_ranges = HashMap::new();
    collect_box_axis_ranges(
      &root,
      0.0,
      axes.block_size(&root.logical_bounds()),
      axes,
      &mut box_axis_ranges,
    );

    Self {
      root,
      total_height,
      forced_boundaries: forced,
      atomic_ranges,
      table_repetitions,
      page_name_transitions,
      box_axis_ranges,
    }
  }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct PageLayoutKey {
  width_bits: u64,
  height_bits: u64,
  style_hash: u64,
  font_generation: u64,
}

#[inline]
fn f32_to_canonical_bits(value: f32) -> u32 {
  if value == 0.0 {
    0.0f32.to_bits()
  } else {
    value.to_bits()
  }
}

impl PageLayoutKey {
  fn new(style: &ResolvedPageStyle, style_hash: u64, font_generation: u64) -> Self {
    Self {
      width_bits: f32_to_canonical_bits(style.content_size.width) as u64,
      height_bits: f32_to_canonical_bits(style.content_size.height) as u64,
      style_hash,
      font_generation,
    }
  }
}

#[derive(Debug, Clone)]
struct BlockAxisMapping {
  base_total: f32,
  target_total: f32,
  /// Anchor points sorted by base position (monotonic in both base + target coordinates).
  base_to_target: Vec<(f32, f32)>,
  /// Anchor points sorted by target position (monotonic in both target + base coordinates).
  target_to_base: Vec<(f32, f32)>,
}

impl BlockAxisMapping {
  fn identity(total: f32) -> Self {
    Self {
      base_total: total,
      target_total: total,
      base_to_target: vec![(0.0, 0.0), (total, total)],
      target_to_base: vec![(0.0, 0.0), (total, total)],
    }
  }

  fn new(
    base_total: f32,
    target_total: f32,
    base_ranges: &HashMap<usize, BoxAxisRange>,
    target_ranges: &HashMap<usize, BoxAxisRange>,
  ) -> Self {
    let mut anchors: Vec<(f32, f32)> = Vec::new();
    anchors.push((0.0, 0.0));
    anchors.push((base_total, target_total));

    for (box_id, base) in base_ranges {
      if let Some(target) = target_ranges.get(box_id) {
        anchors.push((base.start, target.start));
        anchors.push((base.end, target.end));
      }
    }

    let base_to_target = build_monotonic_mapping(anchors, base_total, target_total);
    let target_to_base = build_monotonic_mapping(
      base_to_target.iter().map(|(b, t)| (*t, *b)).collect(),
      target_total,
      base_total,
    );

    Self {
      base_total,
      target_total,
      base_to_target,
      target_to_base,
    }
  }

  fn map_base_to_target(&self, pos: f32) -> f32 {
    map_piecewise(&self.base_to_target, pos, self.target_total)
  }

  fn map_target_to_base(&self, pos: f32) -> f32 {
    map_piecewise(&self.target_to_base, pos, self.base_total)
  }
}

fn build_monotonic_mapping(
  mut points: Vec<(f32, f32)>,
  max_x: f32,
  max_y: f32,
) -> Vec<(f32, f32)> {
  points.retain(|(x, y)| x.is_finite() && y.is_finite());
  for (x, y) in points.iter_mut() {
    *x = x.clamp(0.0, max_x);
    *y = y.clamp(0.0, max_y);
  }

  points.sort_by(|a, b| {
    a.0
      .partial_cmp(&b.0)
      .unwrap_or(std::cmp::Ordering::Equal)
  });

  // Deduplicate by x, keeping the maximum y for each x to preserve monotonicity.
  let mut deduped: Vec<(f32, f32)> = Vec::new();
  for (x, y) in points {
    if let Some(last) = deduped.last_mut() {
      if (x - last.0).abs() < EPSILON {
        last.1 = last.1.max(y);
        continue;
      }
    }
    deduped.push((x, y));
  }

  if deduped.is_empty() {
    return vec![(0.0, 0.0), (max_x, max_y)];
  }

  // Ensure there is a 0 -> 0 anchor.
  if deduped[0].0 > EPSILON {
    deduped.insert(0, (0.0, 0.0));
  } else {
    deduped[0] = (0.0, deduped[0].1);
  }

  // Ensure there is an end anchor.
  if (deduped.last().unwrap().0 - max_x).abs() > EPSILON {
    deduped.push((max_x, max_y));
  } else if let Some(last) = deduped.last_mut() {
    last.0 = max_x;
    last.1 = max_y;
  }

  // Enforce monotonicity in y.
  let mut out: Vec<(f32, f32)> = Vec::with_capacity(deduped.len());
  let mut last_y = 0.0f32;
  for (x, y) in deduped {
    last_y = last_y.max(y);
    out.push((x, last_y));
  }

  // Ensure final anchor reaches max_y.
  if let Some(last) = out.last_mut() {
    last.0 = max_x;
    last.1 = max_y;
  }
  out
}

fn map_piecewise(points: &[(f32, f32)], x: f32, max_y: f32) -> f32 {
  if !x.is_finite() {
    return 0.0;
  }
  if points.is_empty() {
    return x.clamp(0.0, max_y);
  }
  if points.len() == 1 {
    return points[0].1.clamp(0.0, max_y);
  }

  if x <= points[0].0 + EPSILON {
    return points[0].1.clamp(0.0, max_y);
  }
  let last = points.len() - 1;
  if x >= points[last].0 - EPSILON {
    return points[last].1.clamp(0.0, max_y);
  }

  let idx = points
    .partition_point(|(px, _)| *px < x)
    .saturating_sub(1)
    .min(points.len().saturating_sub(2));
  let (x0, y0) = points[idx];
  let (x1, y1) = points[idx + 1];
  let dx = (x1 - x0).max(EPSILON);
  let t = ((x - x0) / dx).clamp(0.0, 1.0);
  (y0 + t * (y1 - y0)).clamp(0.0, max_y)
}

/// Split a laid out fragment tree into pages using the provided @page rules.
///
/// When @page rules change the content size between pages (e.g., :left/:right or named pages),
/// each page is re-laid out against its resolved page style so line wrapping matches the used
/// page box. Layouts are cached per page style to avoid redundant work when the same style is
/// reused (e.g., multiple :right pages).
pub fn paginate_fragment_tree(
  box_tree: &BoxTree,
  initial_layout: Option<(&ResolvedPageStyle, &FragmentNode)>,
  rules: &[CollectedPageRule<'_>],
  fallback_page_size: Size,
  font_ctx: &FontContext,
  root_style: &Arc<ComputedStyle>,
  root_font_size: f32,
  initial_page_name: Option<String>,
  enable_layout_cache: bool,
) -> Result<Vec<FragmentNode>, LayoutError> {
  // Page progression is defined in terms of the document's principal writing mode and direction.
  // The box tree root is normalized to carry the root element's writing mode + direction, even
  // when the root box is a synthetic wrapper.
  let root_axes = FragmentAxes::from_writing_mode_and_direction(
    box_tree.root.style.writing_mode,
    box_tree.root.style.direction,
  );
  let root_axis = FragmentAxis {
    block_is_horizontal: root_axes.block_axis() == PhysicalAxis::X,
    block_positive: root_axes.block_positive(),
  };
  let log_running_elements =
    crate::debug::runtime::runtime_toggles().truthy("FASTR_LOG_RUNNING_ELEMENTS");
  if rules.is_empty() {
    if let Some((_, root)) = initial_layout {
      return Ok(vec![root.clone()]);
    }

    let mut config = LayoutConfig::for_viewport(fallback_page_size);
    config.enable_cache = enable_layout_cache;
    let engine = LayoutEngine::with_font_context(config, font_ctx.clone());
    let tree = engine.layout_tree(box_tree)?;
    return Ok(vec![tree.root]);
  }

  let style_hash = layout_style_fingerprint(root_style);
  let font_generation = font_ctx.font_generation();
  let mut layouts: HashMap<PageLayoutKey, CachedLayout> = HashMap::new();
  let base_style_for_margins = Some(root_style.as_ref());
  let fallback_page_name = initial_page_name.as_deref();

  if let Some((style, root)) = initial_layout {
    let key = PageLayoutKey::new(style, style_hash, font_generation);
    layouts
      .entry(key)
      .or_insert_with(|| CachedLayout::from_root(root.clone(), style, fallback_page_name, root_axes));
  }

  let mut first_page_side = if root_axes.page_progression_is_ltr() {
    PageSide::Right
  } else {
    PageSide::Left
  };

  let (base_key, base_total_height, base_page_names, base_forced, base_root, base_box_axis_ranges) = loop {
    let base_style = resolve_page_style(
      rules,
      0,
      initial_page_name.as_deref(),
      first_page_side,
      false,
      fallback_page_size,
      root_font_size,
      base_style_for_margins,
    );
    let base_key = PageLayoutKey::new(&base_style, style_hash, font_generation);
    let base_layout = layout_for_style(
      &base_style,
      base_key,
      &mut layouts,
      box_tree,
      font_ctx,
      fallback_page_name,
      root_axes,
      enable_layout_cache,
    )?;

    // CSS Page 3 requires UAs to suppress leading blank pages. If the document starts with a forced
    // side constraint (e.g. `break-before: left` on the first element), treat that requirement as
    // the initial page side rather than emitting empty pages.
    if let Some(required) = required_page_side(&base_layout.forced_boundaries, 0.0) {
      if required != first_page_side {
        first_page_side = required;
        continue;
      }
    }

    break (
      base_key,
      base_layout.total_height.max(EPSILON),
      base_layout.page_name_transitions.clone(),
      base_layout.forced_boundaries.clone(),
      base_layout.root.clone(),
      base_layout.box_axis_ranges.clone(),
    );
  };

  let mut string_set_events = collect_string_set_events(&base_root, box_tree, root_axes);
  string_set_events.sort_by(|a, b| {
    a.abs_block
      .partial_cmp(&b.abs_block)
      .unwrap_or(Ordering::Equal)
  });
  let mut string_event_idx = 0usize;
  let mut string_set_carry: HashMap<String, String> = HashMap::new();
  let mut running_element_state = crate::layout::running_elements::RunningElementState::default();

  let mut block_axis_mappings: HashMap<PageLayoutKey, BlockAxisMapping> = HashMap::new();
  block_axis_mappings.insert(base_key, BlockAxisMapping::identity(base_total_height));

  let mut pages: Vec<(
    FragmentNode,
    ResolvedPageStyle,
    HashMap<String, RunningStringValues>,
    HashMap<String, RunningElementValues>,
  )> = Vec::new();
  let mut consumed_base = 0.0f32;
  let mut page_index = 0usize;

  loop {
    let start_in_base = consumed_base;
    let mut page_name =
      page_name_for_position(&base_page_names, start_in_base, fallback_page_name);
    let side = page_side_for_index(page_index, first_page_side);
    let required_side = required_page_side(&base_forced, start_in_base);
    let is_blank_page = required_side.map_or(false, |required| required != side);

    let mut page_style = resolve_page_style(
      rules,
      page_index,
      page_name.as_deref(),
      side,
      is_blank_page,
      fallback_page_size,
      root_font_size,
      base_style_for_margins,
    );
    let mut key = PageLayoutKey::new(&page_style, style_hash, font_generation);
    let mut layout = layout_for_style(
      &page_style,
      key,
      &mut layouts,
      box_tree,
      font_ctx,
      fallback_page_name,
      root_axes,
      enable_layout_cache,
    )?;
    let axis = root_axis;

    let mut total_height = layout.total_height;
    if total_height <= EPSILON {
      break;
    }
    let root_block_size = axis.block_size(&layout.root.bounds);

    let mut fixed_fragments = Vec::new();
    collect_fixed_fragments(&layout.root, Point::ZERO, &mut fixed_fragments);
    let mut page_root = FragmentNode::new_block_styled(
      Rect::from_xywh(
        0.0,
        0.0,
        page_style.total_size.width,
        page_style.total_size.height,
      ),
      Vec::new(),
      Arc::new(page_style.page_style.clone()),
    );
    let mut page_running_elements: HashMap<String, RunningElementValues> = HashMap::new();

    let mut end_in_base = start_in_base;

    if !is_blank_page {
      let mut start = {
        let mapping = block_axis_mappings.entry(key).or_insert_with(|| {
          BlockAxisMapping::new(
            base_total_height,
            total_height,
            &base_box_axis_ranges,
            &layout.box_axis_ranges,
          )
        });
        mapping.map_base_to_target(consumed_base).min(total_height)
      };
      let actual_page_name =
        page_name_for_position(&layout.page_name_transitions, start, fallback_page_name);
      if actual_page_name != page_name {
        page_name = actual_page_name;
        page_style = resolve_page_style(
          rules,
          page_index,
          page_name.as_deref(),
          side,
          is_blank_page,
          fallback_page_size,
          root_font_size,
          base_style_for_margins,
        );
        key = PageLayoutKey::new(&page_style, style_hash, font_generation);
        layout = layout_for_style(
          &page_style,
          key,
          &mut layouts,
          box_tree,
          font_ctx,
          fallback_page_name,
          root_axes,
          enable_layout_cache,
        )?;
        total_height = layout.total_height;
        start = {
          let mapping = block_axis_mappings.entry(key).or_insert_with(|| {
            BlockAxisMapping::new(
              base_total_height,
              total_height,
              &base_box_axis_ranges,
              &layout.box_axis_ranges,
            )
          });
          mapping.map_base_to_target(consumed_base).min(total_height)
        };
      }

      if start >= total_height - EPSILON {
        break;
      }

      let page_block = if axis.block_is_horizontal {
        page_style.content_size.width
      } else {
        page_style.content_size.height
      }
      .max(1.0);
      let natural_end = (start + page_block).min(total_height);
      let mut end_candidate = natural_end;
      let mut forced_break = false;
      if let Some(boundary) = next_forced_boundary(&layout.forced_boundaries, start, end_candidate)
      {
        end_candidate = boundary;
        forced_break = true;
      }

      end_candidate =
        adjust_for_atomic_ranges(start, end_candidate, &layout.atomic_ranges).min(total_height);

      // When the fragmentainer limit would clip into the start of an in-flow block (e.g. showing
      // the first line of the next heading at the bottom of the page), prefer breaking at the
      // prior in-flow boundary instead. This approximates the standard fragmentation algorithm of
      // filling the fragmentainer until the next sibling would overflow, without forcing earlier
      // breaks for spacer-only blocks.
      //
      // Keep forced breaks and atomic-range adjustments authoritative: those are already producing
      // a deliberate boundary choice.
      if !forced_break
        && (end_candidate - natural_end).abs() < EPSILON
        && natural_end + EPSILON < total_height
      {
        let default_style = ComputedStyle::default();
        if let Some(boundary) = best_in_flow_block_boundary(
          &layout.root,
          0.0,
          &axis,
          root_block_size,
          start,
          end_candidate,
          &layout.atomic_ranges,
          &default_style,
        ) {
          end_candidate = boundary;
        }
      }

      if !forced_break {
        let adjusted = adjust_end_for_table_repetition(
          start,
          end_candidate,
          page_block,
          &layout.table_repetitions,
        )
        .min(total_height);
        if adjusted < end_candidate - EPSILON {
          end_candidate = adjust_for_atomic_ranges(start, adjusted, &layout.atomic_ranges).min(total_height);
        }
      }

      if end_candidate <= start + EPSILON {
        end_candidate = adjust_for_atomic_ranges(
          start,
          (start + page_block).min(total_height),
          &layout.atomic_ranges,
        )
        .min(total_height);
        if end_candidate <= start + EPSILON {
          break;
        }
      }

      let mut end = end_candidate;
      let mut clipped = clip_node(
        &layout.root,
        &axis,
        start,
        end_candidate,
        0.0,
        start,
        end_candidate,
        root_block_size,
        page_index,
        0,
        FragmentationContext::Page,
        page_block,
        root_axes,
      )?;
      let mut page_footnotes: Vec<FootnoteOccurrence> = Vec::new();

      // If the page contains `float: footnote` calls, the footnote area at the bottom of the page
      // reduces the block-size available for main flow content. Use a provisional clip to
      // determine which footnotes are eligible for this page and adjust the end accordingly.
      if let Some(mut provisional) = clipped.take() {
        strip_fixed_fragments(&mut provisional);
        normalize_fragment_margins(
          &mut provisional,
          page_index == 0,
          end_candidate >= total_height - 0.01,
          &axis,
        );
        let provisional_footnotes = collect_footnotes_for_page(&provisional, &axis);
        let adjusted_end =
          adjust_end_for_footnotes(start, end_candidate, page_block, &provisional_footnotes, &axis);
        if adjusted_end > start + EPSILON {
          end = adjusted_end;
        }

        // If the footnote adjustment did not change the break position, we can reuse the clipped
        // subtree and avoid re-clipping.
        if (end - end_candidate).abs() < EPSILON {
          page_footnotes = provisional_footnotes;
          clipped = Some(provisional);
        }
      }

      if clipped.is_none() {
        clipped = clip_node(
          &layout.root,
          &axis,
          start,
          end,
          0.0,
          start,
          end,
          root_block_size,
          page_index,
          0,
          FragmentationContext::Page,
          page_block,
          root_axes,
        )?;
      }

      if let Some(mut content) = clipped {
        strip_fixed_fragments(&mut content);
        normalize_fragment_margins(
          &mut content,
          page_index == 0,
          end >= total_height - 0.01,
          &axis,
        );
        if page_footnotes.is_empty() {
          page_footnotes = collect_footnotes_for_page(&content, &axis);
        }
        let footnote_area = build_footnote_area_fragment(&page_style, &axis, &page_footnotes);

        let clipped_block_size = axis.block_size(&content.bounds);
        let page_block_size = if axis.block_is_horizontal {
          page_style.content_size.width
        } else {
          page_style.content_size.height
        };
        content.bounds = if axis.block_is_horizontal {
          Rect::from_xywh(
            content.bounds.x(),
            content.bounds.y(),
            page_style.content_size.width,
            content.bounds.height(),
          )
        } else {
          Rect::from_xywh(
            content.bounds.x(),
            content.bounds.y(),
            content.bounds.width(),
            page_style.content_size.height,
          )
        };
        if !axis.block_positive {
          // `clip_node` rebases fragments to the minimum physical coordinate of the clipped slice.
          // When the block axis runs in the negative direction (e.g. `writing-mode: vertical-rl`),
          // paginated slices should instead align their block-start edge to the page's block-start
          // edge (right/bottom) after the page content box is expanded to its full size.
          let delta = (page_block_size - clipped_block_size).max(0.0);
          if delta > EPSILON {
            for child in content.children_mut().iter_mut() {
              if axis.block_is_horizontal {
                translate_fragment(child, delta, 0.0);
              } else {
                translate_fragment(child, 0.0, delta);
              }
            }
          }
        }
        translate_fragment(
          &mut content,
          page_style.content_origin.x,
          page_style.content_origin.y,
        );
        page_running_elements = running_elements_for_page_fragment(&content, root_axes, &mut running_element_state);
        if log_running_elements {
          let mut counts: HashMap<String, usize> = HashMap::new();
          fn collect(node: &FragmentNode, out: &mut HashMap<String, usize>) {
            if let FragmentContent::RunningAnchor { name, .. } = &node.content {
              *out.entry(name.to_string()).or_insert(0) += 1;
            }
            for child in node.children.iter() {
              collect(child, out);
            }
          }
          fn first_text(node: &FragmentNode) -> Option<String> {
            match &node.content {
              FragmentContent::Text { text, .. } => Some(text.to_string()),
              _ => {
                for child in node.children.iter() {
                  if let Some(found) = first_text(child) {
                    return Some(found);
                  }
                }
                None
              }
            }
          }
          collect(&content, &mut counts);
          let mut previews: HashMap<String, Vec<String>> = HashMap::new();
          for (name, values) in &page_running_elements {
            let mut texts = Vec::new();
            for snap in values.first.iter().chain(values.last.iter()) {
              if let Some(text) = first_text(snap) {
                let preview: String = text.chars().take(80).collect();
                texts.push(preview);
              }
            }
            previews.insert(name.clone(), texts);
          }
          eprintln!(
            "[paginate-running] page={} anchors={:?} selected={:?}",
            page_index, counts, previews
          );
        }
        page_root.children_mut().push(content);
        if let Some(footnote_area) = footnote_area {
          page_root.children_mut().push(footnote_area);
        }
      }

      let mut mapped_end_in_base = {
        let mapping = block_axis_mappings.entry(key).or_insert_with(|| {
          BlockAxisMapping::new(
            base_total_height,
            total_height,
            &base_box_axis_ranges,
            &layout.box_axis_ranges,
          )
        });
        mapping.map_target_to_base(end).min(base_total_height)
      };

      // Fall back to proportional mapping if the anchor map cannot make forward progress.
      if mapped_end_in_base <= consumed_base + EPSILON {
        let base_advance = ((end - start).max(0.0) / total_height) * base_total_height;
        mapped_end_in_base = (consumed_base + base_advance).min(base_total_height);
      }

      end_in_base = mapped_end_in_base;
    }

    for mut fixed in fixed_fragments {
      translate_fragment(
        &mut fixed,
        page_style.content_origin.x,
        page_style.content_origin.y,
      );
      page_root.children_mut().push(fixed);
    }

    let page_strings = running_strings_for_page(
      &string_set_events,
      &mut string_event_idx,
      &mut string_set_carry,
      start_in_base,
      end_in_base,
    );

    if is_blank_page {
      // Blank pages still participate in margin box running element resolution by carrying the last
      // running element seen so far.
      let mut idx = 0usize;
      page_running_elements = running_elements_for_page(
        &[],
        &mut idx,
        &mut running_element_state,
        0.0,
        0.0,
      );
    }

    pages.push((page_root, page_style, page_strings, page_running_elements));
    if !is_blank_page {
      consumed_base = end_in_base;
    }
    page_index += 1;

    if consumed_base >= base_total_height - EPSILON {
      break;
    }
  }

  if pages.is_empty() {
    return Ok(vec![base_root]);
  }

  let count = pages.len();
  let mut page_roots = Vec::with_capacity(count);
  for (idx, (mut page, style, running_strings, running_elements)) in pages.into_iter().enumerate() {
    page.children_mut().extend(build_margin_box_fragments(
      &style,
      font_ctx,
      idx,
      count,
      &running_strings,
      &running_elements,
    ));
    propagate_fragment_metadata(&mut page, idx, count);
    page_roots.push(page);
  }

  Ok(page_roots)
}

/// Split a laid out fragment tree into pages using the provided @page rules with options.
pub fn paginate_fragment_tree_with_options(
  box_tree: &BoxTree,
  initial_layout: Option<(&ResolvedPageStyle, &FragmentNode)>,
  rules: &[CollectedPageRule<'_>],
  fallback_page_size: Size,
  font_ctx: &FontContext,
  root_style: &Arc<ComputedStyle>,
  root_font_size: f32,
  initial_page_name: Option<String>,
  enable_layout_cache: bool,
  options: PaginateOptions,
) -> Result<Vec<FragmentNode>, LayoutError> {
  let mut pages = paginate_fragment_tree(
    box_tree,
    initial_layout,
    rules,
    fallback_page_size,
    font_ctx,
    root_style,
    root_font_size,
    initial_page_name,
    enable_layout_cache,
  )?;

  apply_page_stacking(&mut pages, box_tree.root.style.writing_mode, options.stacking);

  Ok(pages)
}

fn adjust_for_atomic_ranges(start: f32, mut end: f32, ranges: &[AtomicRange]) -> f32 {
  const EPSILON: f32 = 0.01;

  // If the fragment starts inside an atomic range, extend the end so we don't split it.
  //
  // Atomic range endpoints are break-safe (see `atomic_containing` in `fragmentation.rs`), so treat
  // `start == range.start` as being "inside" for this extension logic.
  if let Some(containing) = ranges.iter().copied().find(|range| {
    start >= range.start - EPSILON && start < range.end - EPSILON && range.end > range.start
  }) {
    if end < containing.end - EPSILON {
      return containing.end;
    }
  }

  // Only adjust when the chosen fragmentainer boundary would *split* an atomic range. Atomic
  // ranges that are fully contained within `[start, end]` are already safe to paginate over, and
  // shrinking `end` to their start would create empty pages when the first atomic content begins
  // after `start` (e.g. a table preceded by default body margins).
  if let Some(containing_end) = ranges
    .iter()
    .copied()
    .filter(|range| {
      end > range.start + EPSILON && end < range.end - EPSILON && range.end > range.start
    })
    .min_by(|a, b| {
      a.start
        .partial_cmp(&b.start)
        .unwrap_or(std::cmp::Ordering::Equal)
    })
  {
    if containing_end.start <= start + EPSILON {
      end = end.max(containing_end.end);
    } else {
      end = end.min(containing_end.start);
    }
  }

  end
}

fn table_header_overhead_at(tables: &[TableRepetitionInfo], pos: f32) -> f32 {
  tables
    .iter()
    .filter(|info| {
      info.header_block_size > EPSILON
        && pos > info.start + EPSILON
        && pos < info.end - EPSILON
    })
    .map(|info| info.header_block_size)
    .sum()
}

fn innermost_footer_table_at<'a>(
  tables: &'a [TableRepetitionInfo],
  pos: f32,
) -> Option<&'a TableRepetitionInfo> {
  tables
    .iter()
    .filter(|info| {
      info.footer_block_size > EPSILON
        && pos > info.start + EPSILON
        && pos < info.end - EPSILON
    })
    .max_by(|a, b| a.start.partial_cmp(&b.start).unwrap_or(Ordering::Equal))
}

fn adjust_end_for_table_repetition(
  start: f32,
  end_candidate: f32,
  fragmentainer_size: f32,
  tables: &[TableRepetitionInfo],
) -> f32 {
  if !(fragmentainer_size.is_finite() && fragmentainer_size > 0.0) {
    return end_candidate;
  }

  let header_overhead = table_header_overhead_at(tables, start).min((fragmentainer_size - EPSILON).max(0.0));
  let max_without_footer = (fragmentainer_size - header_overhead).max(EPSILON);
  let mut max_len = max_without_footer;

  if let Some(table) = innermost_footer_table_at(tables, end_candidate) {
    let footer_overhead = table
      .footer_block_size
      .min((fragmentainer_size - header_overhead - EPSILON).max(0.0));
    if footer_overhead > EPSILON {
      let max_with_footer = (fragmentainer_size - header_overhead - footer_overhead).max(EPSILON);
      if start + max_with_footer <= table.start + EPSILON {
        // Not enough space to include any of the table while reserving the repeated footer. Break
        // before the table instead so preceding content can still fill the page.
        return table.start.min(start + max_without_footer).min(end_candidate);
      }
      max_len = max_with_footer;
    }
  }

  end_candidate.min(start + max_len)
}

#[derive(Debug, Clone)]
struct PageNameTransition {
  /// Flow position (in fragmentation-axis coordinates) where the page name becomes active.
  position: f32,
  /// The page name used from `position` onwards. An empty string represents the unnamed page type.
  name: String,
}

#[derive(Debug, Clone)]
struct PropagatedPageValues {
  start: String,
  end: String,
}

fn page_property_applies(node: &FragmentNode) -> bool {
  if !matches!(node.content, FragmentContent::Block { .. }) {
    return false;
  }

  let Some(style) = node.style.as_deref() else {
    // Anonymous block boxes participate in class-A break points even though they don't carry an
    // authored `page` value.
    return true;
  };

  style.position.is_in_flow() && style.display.is_block_level()
}

fn page_name_at_position<'a>(transitions: &'a [PageNameTransition], pos: f32) -> &'a str {
  if transitions.is_empty() {
    return "";
  }

  let idx = transitions.partition_point(|t| t.position <= pos + EPSILON);
  transitions
    .get(idx.saturating_sub(1))
    .map(|t| t.name.as_str())
    .unwrap_or("")
}

fn page_name_for_position(
  transitions: &[PageNameTransition],
  pos: f32,
  fallback: Option<&str>,
) -> Option<String> {
  let name = page_name_at_position(transitions, pos);
  if name.is_empty() {
    fallback.map(|s| s.to_string())
  } else {
    Some(name.to_string())
  }
}

fn collect_page_name_transitions(
  root: &FragmentNode,
  axis: &FragmentAxis,
  fallback: Option<&str>,
) -> Vec<PageNameTransition> {
  fn propagate(
    node: &FragmentNode,
    abs_start: f32,
    inherited_used: &str,
    transitions: &mut Vec<PageNameTransition>,
    axis: &FragmentAxis,
    parent_block_size: f32,
    force_apply: bool,
  ) -> Option<PropagatedPageValues> {
    let applies = force_apply || page_property_applies(node);
    let used = if applies {
      node
        .style
        .as_deref()
        .and_then(|style| style.page.clone())
        .unwrap_or_else(|| inherited_used.to_string())
    } else {
      inherited_used.to_string()
    };
    let inherited_for_children = if applies { used.as_str() } else { inherited_used };

    let mut child_starts: Vec<f32> = Vec::with_capacity(node.children.len());
    let mut child_ends: Vec<f32> = Vec::with_capacity(node.children.len());
    let mut child_values: Vec<Option<PropagatedPageValues>> = Vec::with_capacity(node.children.len());

    for child in node.children.iter() {
      let child_block_size = axis.block_size(&child.bounds);
      let (child_abs_start, child_abs_end) = axis.flow_range(abs_start, parent_block_size, &child.bounds);
      let values = propagate(
        child,
        child_abs_start,
        inherited_for_children,
        transitions,
        axis,
        child_block_size,
        false,
      );
      child_starts.push(child_abs_start);
      child_ends.push(child_abs_end);
      child_values.push(values);
    }

    for idx in 0..node.children.len().saturating_sub(1) {
      let Some(prev) = child_values[idx].as_ref() else {
        continue;
      };
      let Some(next) = child_values[idx + 1].as_ref() else {
        continue;
      };
      if prev.end == next.start {
        continue;
      }

      let mut boundary = child_ends[idx];
      if let Some(meta) = node
        .children
        .get(idx)
        .and_then(|child| child.block_metadata.as_ref())
      {
        let mut candidate = child_ends[idx] + meta.margin_bottom;
        if candidate < child_ends[idx] {
          candidate = child_ends[idx];
        }
        candidate = candidate.min(child_starts[idx + 1]);
        boundary = candidate;
      }

      transitions.push(PageNameTransition {
        position: boundary,
        name: next.start.clone(),
      });
    }

    if !applies {
      return None;
    }

    let start = match child_values.first().and_then(|val| val.as_ref()) {
      Some(values) => values.start.clone(),
      None => used.clone(),
    };
    let end = match child_values.last().and_then(|val| val.as_ref()) {
      Some(values) => values.end.clone(),
      None => used.clone(),
    };

    Some(PropagatedPageValues { start, end })
  }

  let inherited = fallback.unwrap_or("");
  let parent_block_size = axis.block_size(&root.bounds);
  let mut transitions = Vec::new();
  let root_values = propagate(
    root,
    0.0,
    inherited,
    &mut transitions,
    axis,
    parent_block_size,
    true,
  )
  .unwrap_or_else(|| PropagatedPageValues {
    start: inherited.to_string(),
    end: inherited.to_string(),
  });

  transitions.push(PageNameTransition {
    position: 0.0,
    name: root_values.start,
  });

  transitions.sort_by(|a, b| a.position.partial_cmp(&b.position).unwrap_or(Ordering::Equal));

  let mut deduped: Vec<PageNameTransition> = Vec::new();
  for transition in transitions {
    if let Some(last) = deduped.last_mut() {
      if (last.position - transition.position).abs() < EPSILON {
        last.name = transition.name;
        continue;
      }
      if last.name == transition.name {
        continue;
      }
    }
    deduped.push(transition);
  }

  if deduped.is_empty() {
    deduped.push(PageNameTransition {
      position: 0.0,
      name: inherited.to_string(),
    });
  }

  // Guarantee a `0.0` transition for callers that binary-search positions.
  if (deduped[0].position - 0.0).abs() > EPSILON {
    deduped.insert(
      0,
      PageNameTransition {
        position: 0.0,
        name: inherited.to_string(),
      },
    );
  } else {
    deduped[0].position = 0.0;
  }

  deduped
}

fn apply_page_stacking(
  pages: &mut [FragmentNode],
  writing_mode: WritingMode,
  stacking: PageStacking,
) {
  let PageStacking::Stacked { gap } = stacking else {
    return;
  };

  let gap = gap.max(0.0);
  let horizontal = block_axis_is_horizontal(writing_mode);
  let mut offset = 0.0;
  let mut previous_extent: Option<f32> = None;

  for page in pages.iter_mut() {
    if let Some(extent) = previous_extent {
      offset += extent + gap;
    }

    translate_fragment(
      page,
      if horizontal { offset } else { 0.0 },
      if horizontal { 0.0 } else { offset },
    );

    previous_extent = Some(if horizontal {
      page.bounds.width()
    } else {
      page.bounds.height()
    });
  }
}

fn running_strings_for_page(
  events: &[StringSetEvent],
  idx: &mut usize,
  carry: &mut HashMap<String, String>,
  start: f32,
  end: f32,
) -> HashMap<String, RunningStringValues> {
  let start_boundary = start - EPSILON;
  while *idx < events.len() && events[*idx].abs_block < start_boundary {
    let event = &events[*idx];
    carry.insert(event.name.clone(), event.value.clone());
    *idx += 1;
  }

  let mut snapshot = HashMap::new();
  for (name, value) in carry.iter() {
    snapshot.insert(
      name.clone(),
      RunningStringValues {
        start: Some(value.clone()),
        first: None,
        last: None,
      },
    );
  }

  while *idx < events.len() && events[*idx].abs_block < end {
    let event = &events[*idx];
    let entry = snapshot
      .entry(event.name.clone())
      .or_insert_with(|| RunningStringValues {
        start: carry.get(&event.name).cloned(),
        first: None,
        last: None,
      });
    if entry.first.is_none() {
      if (event.abs_block - start).abs() < EPSILON {
        entry.start = Some(event.value.clone());
      }
      entry.first = Some(event.value.clone());
    }
    entry.last = Some(event.value.clone());
    carry.insert(event.name.clone(), event.value.clone());
    *idx += 1;
  }

  snapshot
}

#[derive(Debug, Clone)]
struct FootnoteOccurrence {
  pos: f32,
  snapshot: FragmentNode,
}

fn collect_footnotes_for_page(
  root: &FragmentNode,
  axis: &crate::layout::fragmentation::FragmentAxis,
) -> Vec<FootnoteOccurrence> {
  let mut occurrences: Vec<FootnoteOccurrence> = Vec::new();
  collect_footnote_occurrences(root, Point::ZERO, axis, &mut occurrences);
  occurrences.sort_by(|a, b| a.pos.partial_cmp(&b.pos).unwrap_or(Ordering::Equal));
  occurrences
}

fn collect_footnote_occurrences(
  node: &FragmentNode,
  origin: Point,
  axis: &crate::layout::fragmentation::FragmentAxis,
  out: &mut Vec<FootnoteOccurrence>,
) {
  let abs_origin = Point::new(origin.x + node.bounds.x(), origin.y + node.bounds.y());
  let abs_block = if axis.block_is_horizontal {
    abs_origin.x
  } else {
    abs_origin.y
  };

  if let FragmentContent::FootnoteAnchor { snapshot } = &node.content {
    out.push(FootnoteOccurrence {
      pos: abs_block,
      snapshot: (**snapshot).clone(),
    });
  }

  for child in node.children.iter() {
    collect_footnote_occurrences(child, abs_origin, axis, out);
  }
}

fn adjust_end_for_footnotes(
  start: f32,
  end_candidate: f32,
  page_block: f32,
  footnotes: &[FootnoteOccurrence],
  axis: &crate::layout::fragmentation::FragmentAxis,
) -> f32 {
  if footnotes.is_empty() {
    return end_candidate;
  }

  let block_size = |rect: &Rect| if axis.block_is_horizontal { rect.width() } else { rect.height() };
  // Simple, fixed separator rule: 1px solid currentColor.
  let separator_block = 1.0;

  let mut included = 0usize;
  let mut total_footnote_block = 0.0f32;
  for occ in footnotes {
    let body_block = block_size(&occ.snapshot.bounds).max(0.0);
    let next_total = total_footnote_block + body_block;
    let next_with_separator = next_total + separator_block;
    let main_block = page_block - next_with_separator;
    if next_with_separator <= page_block && occ.pos < main_block {
      included += 1;
      total_footnote_block = next_total;
      continue;
    }
    break;
  }

  let end = if included == 0 {
    // No footnote calls fit alongside their bodies; defer the first call to the next page.
    start + footnotes[0].pos
  } else {
    let footnote_block = separator_block + total_footnote_block;
    let main_block = (page_block - footnote_block).max(0.0);
    let mut end = start + main_block;
    if included < footnotes.len() {
      end = end.min(start + footnotes[included].pos);
    }
    end
  }
  .min(end_candidate);

  end
}

fn build_footnote_area_fragment(
  page_style: &ResolvedPageStyle,
  axis: &crate::layout::fragmentation::FragmentAxis,
  footnotes: &[FootnoteOccurrence],
) -> Option<FragmentNode> {
  if footnotes.is_empty() {
    return None;
  }

  let page_block = if axis.block_is_horizontal {
    page_style.content_size.width
  } else {
    page_style.content_size.height
  }
  .max(1.0);
  let page_inline = if axis.block_is_horizontal {
    page_style.content_size.height
  } else {
    page_style.content_size.width
  }
  .max(0.0);

  let block_size = |rect: &Rect| {
    if axis.block_is_horizontal {
      rect.width()
    } else {
      rect.height()
    }
  };

  // Simple, fixed separator rule: 1px solid currentColor.
  let separator_block = 1.0;
  let flow_box_start_to_physical = |flow_offset: f32, block_size: f32, parent_block_size: f32| {
    if axis.block_positive {
      flow_offset
    } else {
      parent_block_size - flow_offset - block_size
    }
  };

  let mut snapshots: Vec<FragmentNode> = Vec::with_capacity(footnotes.len());
  let mut total_footnote_block = 0.0f32;
  for occ in footnotes {
    let mut snapshot = occ.snapshot.clone();
    let offset = Point::new(-snapshot.bounds.x(), -snapshot.bounds.y());
    snapshot.translate_root_in_place(offset);
    total_footnote_block += block_size(&snapshot.bounds).max(0.0);
    snapshots.push(snapshot);
  }

  let footnote_block = separator_block + total_footnote_block;
  if footnote_block <= EPSILON {
    return None;
  }

  // Position the entire footnote area at the block-end of the page content box.
  let desired_flow_start = page_block - footnote_block;
  let mut physical_block_start =
    flow_box_start_to_physical(desired_flow_start, footnote_block, page_block);
  if physical_block_start < 0.0 {
    physical_block_start = 0.0;
  }

  let bounds = if axis.block_is_horizontal {
    Rect::from_xywh(
      page_style.content_origin.x + physical_block_start,
      page_style.content_origin.y,
      footnote_block,
      page_inline,
    )
  } else {
    Rect::from_xywh(
      page_style.content_origin.x,
      page_style.content_origin.y + physical_block_start,
      page_inline,
      footnote_block,
    )
  };

  let mut children: Vec<FragmentNode> = Vec::with_capacity(1 + snapshots.len());

  // Separator fragment.
  let mut separator_style = ComputedStyle::default();
  separator_style.display = Display::Block;
  separator_style.writing_mode = page_style.page_style.writing_mode;
  separator_style.direction = page_style.page_style.direction;
  separator_style.color = page_style.page_style.color;
  separator_style.background_color = page_style.page_style.color;
  let separator_style = Arc::new(separator_style);

  let separator_flow_offset = 0.0;
  let separator_block_start =
    flow_box_start_to_physical(separator_flow_offset, separator_block, footnote_block);
  let separator_bounds = if axis.block_is_horizontal {
    Rect::from_xywh(separator_block_start, 0.0, separator_block, page_inline)
  } else {
    Rect::from_xywh(0.0, separator_block_start, page_inline, separator_block)
  };
  children.push(FragmentNode::new_block_styled(
    separator_bounds,
    Vec::new(),
    separator_style,
  ));

  // Stack footnote body snapshots along the block axis in insertion order.
  let mut flow_offset = separator_block;
  for mut snapshot in snapshots {
    let body_block = block_size(&snapshot.bounds).max(0.0);
    let body_block_start = flow_box_start_to_physical(flow_offset, body_block, footnote_block);
    let translate = if axis.block_is_horizontal {
      Point::new(body_block_start, 0.0)
    } else {
      Point::new(0.0, body_block_start)
    };
    snapshot.translate_root_in_place(translate);
    children.push(snapshot);
    flow_offset += body_block;
  }

  Some(FragmentNode::new_block(bounds, children))
}

fn translate_fragment(node: &mut FragmentNode, dx: f32, dy: f32) {
  node.bounds = Rect::from_xywh(
    node.bounds.x() + dx,
    node.bounds.y() + dy,
    node.bounds.width(),
    node.bounds.height(),
  );
  if let Some(logical) = node.logical_override {
    node.logical_override = Some(Rect::from_xywh(
      logical.x() + dx,
      logical.y() + dy,
      logical.width(),
      logical.height(),
    ));
  }
}

fn is_fixed_fragment(fragment: &FragmentNode) -> bool {
  fragment
    .style
    .as_deref()
    .is_some_and(|style| style.position == Position::Fixed)
}

fn strip_fixed_fragments(node: &mut FragmentNode) {
  let mut kept = Vec::with_capacity(node.children.len());
  for mut child in node.children_mut().drain(..) {
    if is_fixed_fragment(&child) {
      continue;
    }
    strip_fixed_fragments(&mut child);
    kept.push(child);
  }
  node.set_children(kept);
}

fn collect_fixed_fragments(node: &FragmentNode, origin: Point, out: &mut Vec<FragmentNode>) {
  if is_fixed_fragment(node) {
    let mut cloned = node.clone();
    translate_fragment(&mut cloned, origin.x, origin.y);
    out.push(cloned);
    return;
  }

  let next_origin = Point::new(origin.x + node.bounds.x(), origin.y + node.bounds.y());
  for child in node.children.iter() {
    collect_fixed_fragments(child, next_origin, out);
  }
}

fn build_margin_box_fragments(
  style: &ResolvedPageStyle,
  font_ctx: &FontContext,
  page_index: usize,
  page_count: usize,
  running_strings: &HashMap<String, RunningStringValues>,
  running_elements: &HashMap<String, RunningElementValues>,
) -> Vec<FragmentNode> {
  let mut fragments = Vec::new();

  const CANONICAL_MARGIN_AREA_ORDER: [PageMarginArea; 16] = [
    PageMarginArea::TopLeftCorner,
    PageMarginArea::TopLeft,
    PageMarginArea::TopCenter,
    PageMarginArea::TopRight,
    PageMarginArea::TopRightCorner,
    PageMarginArea::RightTop,
    PageMarginArea::RightMiddle,
    PageMarginArea::RightBottom,
    PageMarginArea::BottomRightCorner,
    PageMarginArea::BottomRight,
    PageMarginArea::BottomCenter,
    PageMarginArea::BottomLeft,
    PageMarginArea::BottomLeftCorner,
    PageMarginArea::LeftBottom,
    PageMarginArea::LeftMiddle,
    PageMarginArea::LeftTop,
  ];

  for area in CANONICAL_MARGIN_AREA_ORDER {
    let Some(box_style) = style.margin_boxes.get(&area) else {
      continue;
    };
    if matches!(
      box_style.content_value,
      ContentValue::None | ContentValue::Normal
    ) {
      continue;
    }
    if matches!(box_style.display, Display::None) {
      continue;
    }

    if let Some(bounds) = margin_box_bounds(area, style) {
      if bounds.width() <= 0.0 || bounds.height() <= 0.0 {
        continue;
      }

      let style_arc = Arc::new(box_style.clone());
      if let ContentValue::Items(items) = &box_style.content_value {
        let mut element_snapshots = Vec::new();
        for item in items {
          if let ContentItem::Element { ident, select } = item {
            if let Some(snapshot) = crate::layout::running_elements::select_running_element(
              ident,
              *select,
              running_elements,
            ) {
              element_snapshots.push(snapshot);
            }
          }
        }
        if items.len() == 1 {
          if let ContentItem::Element { .. } = &items[0] {
            if let Some(snapshot) = element_snapshots.pop() {
              fragments.push(FragmentNode::new_block_styled(
                bounds,
                vec![snapshot],
                style_arc,
              ));
              continue;
            }
          }
        }
        let children = build_margin_box_children(
          box_style,
          page_index,
          page_count,
          running_strings,
          &style_arc,
        );
        let root = BoxNode::new_block(style_arc.clone(), FormattingContextType::Block, children);
        let box_tree = BoxTree::new(root);

        let config = LayoutConfig::new(Size::new(bounds.width(), bounds.height()));
        let engine = LayoutEngine::with_font_context(config, font_ctx.clone());
        if let Ok(mut tree) = engine.layout_tree(&box_tree) {
          tree.root.bounds = Rect::from_xywh(
            tree.root.bounds.x(),
            tree.root.bounds.y(),
            bounds.width(),
            bounds.height(),
          );
          tree.root.scroll_overflow = Rect::from_xywh(
            tree.root.scroll_overflow.x(),
            tree.root.scroll_overflow.y(),
            tree.root.scroll_overflow.width().max(bounds.width()),
            tree.root.scroll_overflow.height().max(bounds.height()),
          );
          let mut next_y = tree
            .root
            .children
            .iter()
            .map(|child| child.bounds.y() + child.bounds.height())
            .fold(0.0, f32::max);
          for mut snapshot in element_snapshots {
            translate_fragment(&mut snapshot, 0.0, next_y);
            next_y += snapshot.bounds.height();
            tree.root.children_mut().push(snapshot);
          }
          translate_fragment(&mut tree.root, bounds.x(), bounds.y());
          fragments.push(tree.root);
        }
        continue;
      }
      let children = build_margin_box_children(
        box_style,
        page_index,
        page_count,
        running_strings,
        &style_arc,
      );
      let root = BoxNode::new_block(style_arc.clone(), FormattingContextType::Block, children);
      let box_tree = BoxTree::new(root);

      let config = LayoutConfig::new(Size::new(bounds.width(), bounds.height()));
      let engine = LayoutEngine::with_font_context(config, font_ctx.clone());
      if let Ok(mut tree) = engine.layout_tree(&box_tree) {
        tree.root.bounds = Rect::from_xywh(
          tree.root.bounds.x(),
          tree.root.bounds.y(),
          bounds.width(),
          bounds.height(),
        );
        tree.root.scroll_overflow = Rect::from_xywh(
          tree.root.scroll_overflow.x(),
          tree.root.scroll_overflow.y(),
          tree.root.scroll_overflow.width().max(bounds.width()),
          tree.root.scroll_overflow.height().max(bounds.height()),
        );
        translate_fragment(&mut tree.root, bounds.x(), bounds.y());
        fragments.push(tree.root);
      }
    }
  }

  fragments
}

fn build_margin_box_children(
  box_style: &ComputedStyle,
  page_index: usize,
  page_count: usize,
  running_strings: &HashMap<String, RunningStringValues>,
  style: &Arc<ComputedStyle>,
) -> Vec<BoxNode> {
  let mut children: Vec<BoxNode> = Vec::new();
  let mut context = ContentContext::new();
  context.set_quotes(box_style.quotes.clone());
  context.set_running_strings(running_strings.clone());
  context.set_counter(
    "page",
    page_index.saturating_add(1).min(i32::MAX as usize) as i32,
  );
  context.set_counter("pages", page_count.min(i32::MAX as usize) as i32);

  let mut text_buf = String::new();
  let flush_text = |buf: &mut String, out: &mut Vec<BoxNode>, style: &Arc<ComputedStyle>| {
    if !buf.is_empty() {
      out.push(BoxNode::new_text(style.clone(), buf.clone()));
      buf.clear();
    }
  };

  match &box_style.content_value {
    ContentValue::Items(items) => {
      for item in items {
        match item {
          ContentItem::String(s) => text_buf.push_str(s),
          ContentItem::Attr { name, fallback, .. } => {
            if let Some(val) = context.get_attribute(name) {
              text_buf.push_str(val);
            } else if let Some(fb) = fallback {
              text_buf.push_str(fb);
            }
          }
          ContentItem::Counter { name, style } => {
            let value = context.get_counter(name);
            let formatted = box_style
              .counter_styles
              .format_value(value, style.clone().unwrap_or(CounterStyle::Decimal.into()));
            text_buf.push_str(&formatted);
          }
          ContentItem::Counters {
            name,
            separator,
            style,
          } => {
            let values = context.get_counters(name);
            let style_name = style.clone().unwrap_or(CounterStyle::Decimal.into());
            if values.is_empty() {
              text_buf.push_str(&box_style.counter_styles.format_value(0, style_name));
            } else {
              let formatted: Vec<String> = values
                .iter()
                .map(|v| {
                  box_style
                    .counter_styles
                    .format_value(*v, style_name.clone())
                })
                .collect();
              text_buf.push_str(&formatted.join(separator));
            }
          }
          ContentItem::StringReference { name, kind } => {
            text_buf.push_str(context.get_running_string(name, *kind).unwrap_or(""));
          }
          ContentItem::OpenQuote => {
            text_buf.push_str(context.open_quote());
            context.push_quote();
          }
          ContentItem::CloseQuote => {
            text_buf.push_str(context.close_quote());
            context.pop_quote();
          }
          ContentItem::NoOpenQuote => context.push_quote(),
          ContentItem::NoCloseQuote => context.pop_quote(),
          ContentItem::Url(url) => {
            if trim_ascii_whitespace(url).is_empty() {
              continue;
            }
            flush_text(&mut text_buf, &mut children, style);
            children.push(BoxNode::new_replaced(
              style.clone(),
              ReplacedType::Image {
                src: url.clone(),
                alt: None,
                crossorigin: CrossOriginAttribute::None,
                referrer_policy: None,
                srcset: Vec::new(),
                sizes: None,
                picture_sources: Vec::new(),
              },
              None,
              None,
            ));
          }
          ContentItem::Element { .. } => {
            flush_text(&mut text_buf, &mut children, style);
          }
        }
      }
    }
    ContentValue::None | ContentValue::Normal => {}
  }

  flush_text(&mut text_buf, &mut children, style);
  children
}

fn layout_for_style<'a>(
  style: &ResolvedPageStyle,
  key: PageLayoutKey,
  cache: &'a mut HashMap<PageLayoutKey, CachedLayout>,
  box_tree: &BoxTree,
  font_ctx: &FontContext,
  fallback_page_name: Option<&str>,
  root_axes: FragmentAxes,
  enable_layout_cache: bool,
) -> Result<&'a CachedLayout, LayoutError> {
  if !cache.contains_key(&key) {
    let mut config = LayoutConfig::for_viewport(style.content_size);
    config.enable_cache = enable_layout_cache;
    let engine = LayoutEngine::with_font_context(config, font_ctx.clone());
    let block_size_hint = if root_axes.block_axis() == PhysicalAxis::X {
      style.content_size.width
    } else {
      style.content_size.height
    };
    let _hint = set_fragmentainer_block_size_hint(Some(block_size_hint));
    let layout_tree = engine.layout_tree(box_tree)?;
    let layout = CachedLayout::from_root(layout_tree.root, style, fallback_page_name, root_axes);
    cache.insert(key, layout);
  }

  Ok(cache.get(&key).expect("layout cache just populated"))
}

fn margin_box_bounds(area: PageMarginArea, style: &ResolvedPageStyle) -> Option<Rect> {
  let trimmed_width = style.page_size.width - 2.0 * style.trim;
  let trimmed_height = style.page_size.height - 2.0 * style.trim;
  let origin_x = style.bleed + style.trim;
  let origin_y = style.bleed + style.trim;
  let ml = style.margin_left;
  let mr = style.margin_right;
  let mt = style.margin_top;
  let mb = style.margin_bottom;

  let top_width = trimmed_width - ml - mr;
  let side_height = trimmed_height - mt - mb;

  let rect = |x: f32, y: f32, w: f32, h: f32| -> Option<Rect> {
    if w <= 0.0 || h <= 0.0 {
      None
    } else {
      Some(Rect::from_xywh(x, y, w, h))
    }
  };

  match area {
    PageMarginArea::TopLeftCorner => rect(origin_x, origin_y, ml, mt),
    PageMarginArea::TopLeft => rect(origin_x + ml, origin_y, top_width / 3.0, mt),
    PageMarginArea::TopCenter => rect(
      origin_x + ml + top_width / 3.0,
      origin_y,
      top_width / 3.0,
      mt,
    ),
    PageMarginArea::TopRight => rect(
      origin_x + ml + 2.0 * top_width / 3.0,
      origin_y,
      top_width / 3.0,
      mt,
    ),
    PageMarginArea::TopRightCorner => rect(origin_x + trimmed_width - mr, origin_y, mr, mt),
    PageMarginArea::RightTop => rect(
      origin_x + trimmed_width - mr,
      origin_y + mt,
      mr,
      side_height / 3.0,
    ),
    PageMarginArea::RightMiddle => rect(
      origin_x + trimmed_width - mr,
      origin_y + mt + side_height / 3.0,
      mr,
      side_height / 3.0,
    ),
    PageMarginArea::RightBottom => rect(
      origin_x + trimmed_width - mr,
      origin_y + mt + 2.0 * side_height / 3.0,
      mr,
      side_height / 3.0,
    ),
    PageMarginArea::BottomRightCorner => rect(
      origin_x + trimmed_width - mr,
      origin_y + trimmed_height - mb,
      mr,
      mb,
    ),
    PageMarginArea::BottomRight => rect(
      origin_x + ml + 2.0 * top_width / 3.0,
      origin_y + trimmed_height - mb,
      top_width / 3.0,
      mb,
    ),
    PageMarginArea::BottomCenter => rect(
      origin_x + ml + top_width / 3.0,
      origin_y + trimmed_height - mb,
      top_width / 3.0,
      mb,
    ),
    PageMarginArea::BottomLeft => rect(
      origin_x + ml,
      origin_y + trimmed_height - mb,
      top_width / 3.0,
      mb,
    ),
    PageMarginArea::BottomLeftCorner => rect(origin_x, origin_y + trimmed_height - mb, ml, mb),
    PageMarginArea::LeftBottom => rect(
      origin_x,
      origin_y + mt + 2.0 * side_height / 3.0,
      ml,
      side_height / 3.0,
    ),
    PageMarginArea::LeftMiddle => rect(
      origin_x,
      origin_y + mt + side_height / 3.0,
      ml,
      side_height / 3.0,
    ),
    PageMarginArea::LeftTop => rect(origin_x, origin_y + mt, ml, side_height / 3.0),
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::style::content::RunningElementSelect;
  use crate::style::display::Display;
  use crate::style::ComputedStyle;
  use crate::text::font_db::FontDatabase;
  use crate::tree::fragment_tree::{FragmentContent, FragmentNode};
  use std::sync::Arc;

  fn contains_running_anchor(node: &FragmentNode) -> bool {
    matches!(node.content, FragmentContent::RunningAnchor { .. })
      || node.children.iter().any(contains_running_anchor)
  }

  #[test]
  fn page_layout_key_canonicalizes_negative_zero() {
    let style = ResolvedPageStyle {
      page_size: Size::new(100.0, 100.0),
      total_size: Size::new(100.0, 100.0),
      content_size: Size::new(0.0, 80.0),
      content_origin: Point::new(0.0, 0.0),
      margin_top: 0.0,
      margin_right: 0.0,
      margin_bottom: 0.0,
      margin_left: 0.0,
      bleed: 0.0,
      trim: 0.0,
      margin_boxes: BTreeMap::new(),
      page_style: ComputedStyle::default(),
    };
    let mut style_neg = style.clone();
    style_neg.content_size = Size::new(-0.0, 80.0);

    let key = PageLayoutKey::new(&style, 1, 2);
    let key_neg = PageLayoutKey::new(&style_neg, 1, 2);
    assert_eq!(key, key_neg);
  }

  #[test]
  fn running_element_snapshots_are_recentred_without_moving_children() {
    let mut running_style = ComputedStyle::default();
    running_style.display = Display::Block;
    running_style.running_position = Some("header".to_string());

    let text_child = FragmentNode::new_text(Rect::from_xywh(5.0, 6.0, 20.0, 4.0), "Header", 3.0);
    let anchor_snapshot = FragmentNode::new_block(
      Rect::from_xywh(2.0, 2.0, 5.0, 2.0),
      vec![FragmentNode::new_text(
        Rect::from_xywh(1.0, 1.0, 3.0, 1.0),
        "Anchor",
        0.0,
      )],
    );
    let anchor_child = FragmentNode::new_running_anchor(
      Rect::from_xywh(7.0, 8.0, 3.0, 3.0),
      "marker".into(),
      anchor_snapshot,
    );

    let header_bounds = Rect::from_xywh(30.0, 40.0, 50.0, 10.0);
    let logical_bounds = Rect::from_xywh(32.0, 42.0, 50.0, 10.0);
    let mut running_fragment = FragmentNode::new_block_styled(
      header_bounds,
      vec![text_child, anchor_child],
      Arc::new(running_style),
    );
    running_fragment.logical_override = Some(logical_bounds);

    let root = FragmentNode::new_block(
      Rect::from_xywh(0.0, 0.0, 120.0, 200.0),
      vec![running_fragment],
    );
    assert!(
      contains_running_anchor(&root),
      "fixture should include a running anchor fragment"
    );

    let events = crate::layout::running_elements::collect_running_element_events(
      &root,
      FragmentAxes::default(),
    );
    let snapshot = events
      .iter()
      .find(|event| event.name == "header")
      .map(|event| &event.snapshot)
      .expect("running element snapshot collected");

    assert_eq!(snapshot.bounds.x(), 0.0);
    assert_eq!(snapshot.bounds.y(), 0.0);
    assert_eq!(snapshot.bounds.width(), header_bounds.width());
    assert_eq!(snapshot.bounds.height(), header_bounds.height());

    let logical = snapshot
      .logical_override
      .expect("logical override should be preserved");
    assert_eq!(logical.x(), 0.0);
    assert_eq!(logical.y(), 0.0);
    assert_eq!(logical.width(), logical_bounds.width());
    assert_eq!(logical.height(), logical_bounds.height());

    assert_eq!(snapshot.children.len(), 1);
    let child = &snapshot.children[0];
    assert!(matches!(child.content, FragmentContent::Text { .. }));
    assert_eq!(child.bounds.x(), 5.0);
    assert_eq!(child.bounds.y(), 6.0);

    assert!(
      !contains_running_anchor(snapshot),
      "running anchors should be stripped from snapshots"
    );
  }

  #[test]
  fn margin_box_fragments_follow_canonical_area_order() {
    let expected_order = [
      PageMarginArea::TopLeftCorner,
      PageMarginArea::TopLeft,
      PageMarginArea::TopCenter,
      PageMarginArea::TopRight,
      PageMarginArea::TopRightCorner,
      PageMarginArea::RightTop,
      PageMarginArea::RightMiddle,
      PageMarginArea::RightBottom,
      PageMarginArea::BottomRightCorner,
      PageMarginArea::BottomRight,
      PageMarginArea::BottomCenter,
      PageMarginArea::BottomLeft,
      PageMarginArea::BottomLeftCorner,
      PageMarginArea::LeftBottom,
      PageMarginArea::LeftMiddle,
      PageMarginArea::LeftTop,
    ];
    let expected_text: Vec<String> = expected_order
      .iter()
      .map(|area| format!("{area:?}"))
      .collect();

    let font_ctx = FontContext::with_database(Arc::new(FontDatabase::empty()));
    let running_strings: HashMap<String, RunningStringValues> = HashMap::new();

    for _ in 0..8 {
      let mut margin_boxes: BTreeMap<PageMarginArea, ComputedStyle> = BTreeMap::new();
      let mut running_elements: HashMap<String, RunningElementValues> = HashMap::new();

      for area in expected_order {
        let ident = format!("{area:?}");
        let mut box_style = ComputedStyle::default();
        box_style.display = Display::Block;
        box_style.content_value = ContentValue::Items(vec![ContentItem::Element {
          ident: ident.clone(),
          select: RunningElementSelect::Start,
        }]);
        margin_boxes.insert(area, box_style);
        running_elements.insert(
          ident.clone(),
          RunningElementValues {
            start: Some(FragmentNode::new_text(
              Rect::from_xywh(0.0, 0.0, 0.0, 0.0),
              ident,
              0.0,
            )),
            first: None,
            last: None,
          },
        );
      }

      let page_style = ResolvedPageStyle {
        page_size: Size::new(100.0, 100.0),
        total_size: Size::new(100.0, 100.0),
        content_size: Size::new(80.0, 80.0),
        content_origin: Point::new(10.0, 10.0),
        margin_top: 10.0,
        margin_right: 10.0,
        margin_bottom: 10.0,
        margin_left: 10.0,
        bleed: 0.0,
        trim: 0.0,
        margin_boxes,
        page_style: ComputedStyle::default(),
      };

      let fragments = build_margin_box_fragments(
        &page_style,
        &font_ctx,
        0,
        1,
        &running_strings,
        &running_elements,
      );

      assert_eq!(fragments.len(), expected_text.len());
      let actual_text: Vec<String> = fragments
        .iter()
        .map(|fragment| {
          fragment
            .children
            .first()
            .and_then(|child| child.content.text())
            .unwrap_or("")
            .to_string()
        })
        .collect();

      assert_eq!(actual_text, expected_text);
    }
  }

  #[test]
  fn margin_box_content_url_does_not_treat_nbsp_as_empty() {
    let mut box_style = ComputedStyle::default();
    box_style.display = Display::Block;
    box_style.content_value = ContentValue::Items(vec![ContentItem::Url("\u{00A0}".to_string())]);
    let style = Arc::new(box_style.clone());
    let running_strings: HashMap<String, RunningStringValues> = HashMap::new();

    let children = build_margin_box_children(&box_style, 0, 1, &running_strings, &style);
    assert_eq!(children.len(), 1);
    let crate::tree::box_tree::BoxType::Replaced(replaced) = &children[0].box_type else {
      panic!("expected replaced child");
    };
    match &replaced.replaced_type {
      ReplacedType::Image { src, .. } => assert_eq!(src, "\u{00A0}"),
      other => panic!("expected image replaced content, got {other:?}"),
    }
  }

  fn footnote_occurrence(pos: f32, block_size: f32) -> FootnoteOccurrence {
    FootnoteOccurrence {
      pos,
      snapshot: FragmentNode::new_block(Rect::from_xywh(0.0, 0.0, 10.0, block_size), vec![]),
    }
  }

  #[test]
  fn adjust_end_for_footnotes_reserves_space_for_included_footnotes() {
    let axis = FragmentAxis {
      block_is_horizontal: false,
      block_positive: true,
    };
    let footnotes = vec![footnote_occurrence(50.0, 10.0)];
    let end = adjust_end_for_footnotes(0.0, 100.0, 100.0, &footnotes, &axis);
    assert!(
      (end - 89.0).abs() < 0.01,
      "expected end=89 after reserving separator+body, got {end}"
    );
  }

  #[test]
  fn adjust_end_for_footnotes_defers_first_call_when_it_does_not_fit() {
    let axis = FragmentAxis {
      block_is_horizontal: false,
      block_positive: true,
    };
    let footnotes = vec![footnote_occurrence(95.0, 10.0)];
    let end = adjust_end_for_footnotes(0.0, 100.0, 100.0, &footnotes, &axis);
    assert!(
      (end - 95.0).abs() < 0.01,
      "expected end=95 so the call moves to the next page, got {end}"
    );
  }
}
