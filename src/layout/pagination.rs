//! Pagination helpers that honor CSS @page rules and margin boxes.

use std::cmp::Ordering;
#[cfg(test)]
use std::collections::BTreeMap;
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;

use crate::css::types::{CollectedPageRule, PageMarginArea};
use crate::geometry::{Point, Rect, Size};
use crate::layout::axis::{FragmentAxes, PhysicalAxis};
use crate::layout::engine::{LayoutConfig, LayoutEngine};
use crate::layout::formatting_context::{
  layout_style_fingerprint, set_fragmentainer_block_size_hint, IntrinsicSizingMode, LayoutError,
};
use crate::layout::fragmentation::{
  apply_flex_parallel_flow_forced_break_shifts, apply_float_parallel_flow_forced_break_shifts,
  apply_grid_parallel_flow_forced_break_shifts, clip_node,
  collect_forced_boundaries_for_pagination_with_axes, normalize_fragment_margins,
  parallel_flow_content_extent, propagate_fragment_metadata, ForcedBoundary, FragmentAxis,
  FragmentationAnalyzer, FragmentationContext,
};
use crate::layout::running_elements::{running_elements_for_page, running_elements_for_page_fragment};
use crate::layout::running_strings::{StringSetEvent, StringSetEventCollector};
use crate::layout::utils::{border_size_from_box_sizing, resolve_length_with_percentage_metrics};
use crate::style::content::{
  ContentContext, ContentItem, ContentValue, CounterStyle, RunningElementValues,
  RunningStringValues,
};
use crate::style::display::{Display, FormattingContextType};
use crate::style::page::{resolve_page_style, PageSide, ResolvedPageStyle};
use crate::style::position::Position;
use crate::style::types::WritingMode;
use crate::style::values::Length;
use crate::style::{block_axis_is_horizontal, inline_axis_is_horizontal, ComputedStyle};
use crate::text::font_loader::FontContext;
use crate::tree::box_tree::{BoxNode, BoxTree, CrossOriginAttribute, ReplacedType};
use crate::tree::fragment_tree::{FragmentContent, FragmentNode, TextSourceRange};

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

fn html_and_body_box_ids(node: &FragmentNode) -> (Option<usize>, Option<usize>) {
  // Renderer-produced fragment trees use the root element (`<html>`) as the tree root, but
  // pagination can introduce synthetic wrappers (e.g. the per-page document wrapper). Those
  // wrappers may contain multiple children (page content + repeated `position: fixed` fragments),
  // so we can't rely on a single-child unwrap.
  //
  // Instead, find the first non-fixed DOM-backed fragment in tree order and treat it as the HTML
  // box.
  if let Some(html_id) = node.box_id() {
    return (Some(html_id), html_id.checked_add(1));
  }

  let mut stack: Vec<&FragmentNode> = Vec::new();
  stack.push(node);
  while let Some(current) = stack.pop() {
    if current
      .style
      .as_deref()
      .is_some_and(|style| matches!(style.position, Position::Fixed))
    {
      continue;
    }
    if let Some(html_id) = current.box_id() {
      return (Some(html_id), html_id.checked_add(1));
    }
    for child in current.children.iter().rev() {
      stack.push(child);
    }
  }

  (None, None)
}

fn subtree_has_in_flow_content(
  node: &FragmentNode,
  html_id: Option<usize>,
  body_id: Option<usize>,
) -> bool {
  // Running/footnote anchors capture paintable snapshots, but the anchors themselves don't paint
  // into the in-flow content stream.
  if matches!(
    node.content,
    FragmentContent::RunningAnchor { .. } | FragmentContent::FootnoteAnchor { .. }
  ) {
    return false;
  }
  if node
    .style
    .as_deref()
    .is_some_and(|style| matches!(style.position, Position::Fixed))
  {
    return false;
  }

  if matches!(node.content, FragmentContent::Text { .. } | FragmentContent::Replaced { .. }) {
    return true;
  }

  if let Some(box_id) = node.box_id() {
    if Some(box_id) != html_id && Some(box_id) != body_id {
      return true;
    }
  }

  node
    .children
    .iter()
    .any(|child| subtree_has_in_flow_content(child, html_id, body_id))
}

fn page_has_in_flow_content(page: &FragmentNode) -> bool {
  // Determine which box IDs correspond to the HTML/body wrappers so we don't treat trailing body
  // margins as "content" for pagination purposes.
  let mut html_id = None;
  let mut body_id = None;

  for child in page.children.iter() {
    if child
      .style
      .as_deref()
      .is_some_and(|style| matches!(style.position, Position::Fixed))
    {
      continue;
    }
    if html_id.is_none() && body_id.is_none() {
      (html_id, body_id) = html_and_body_box_ids(child);
    }
    if subtree_has_in_flow_content(child, html_id, body_id) {
      return true;
    }
  }

  false
}

/// Stable continuation token for paginated layout.
///
/// This intentionally encodes *content position* (DOM order) rather than any geometry derived from
/// a particular layout. It is used to continue pagination across varying @page sizes/margins
/// without skipping or duplicating in-flow content.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum BreakToken {
  Start,
  End,
  /// Block-level continuation within the box identified by `box_id`.
  ///
  /// `offset_bits` stores a canonicalized `f32` offset in fragmentation-axis coordinates relative
  /// to the block's own start edge.
  Block { box_id: usize, offset_bits: u32 },
  /// Byte offset into the source text node identified by `box_id`.
  Text { box_id: usize, offset: usize },
  /// Replaced-element continuation within the box identified by `box_id`.
  Replaced { box_id: usize, offset_bits: u32 },
}

fn inline_start_for_axis(axis: &FragmentAxis, rect: &Rect) -> f32 {
  if axis.block_is_horizontal {
    rect.y()
  } else {
    rect.x()
  }
}

fn translate_inline_for_axis(node: &mut FragmentNode, axis: &FragmentAxis, delta: f32) {
  if delta.abs() <= EPSILON {
    return;
  }
  if axis.block_is_horizontal {
    translate_fragment(node, 0.0, delta);
  } else {
    translate_fragment(node, delta, 0.0);
  }
}

fn token_from_text_fragment(box_id: usize, source_range: &TextSourceRange) -> BreakToken {
  BreakToken::Text {
    box_id,
    offset: source_range.start(),
  }
}

fn token_from_line_start(line: &FragmentNode) -> Option<BreakToken> {
  fn walk(node: &FragmentNode) -> Option<BreakToken> {
    match &node.content {
      FragmentContent::Text {
        box_id: Some(box_id),
        source_range: Some(range),
        is_marker: false,
        ..
      } => Some(token_from_text_fragment(*box_id, range)),
      FragmentContent::Replaced {
        box_id: Some(box_id),
        ..
      } => Some(BreakToken::Replaced {
        box_id: *box_id,
        offset_bits: f32_to_canonical_bits(0.0),
      }),
      _ => {
        for child in node.children.iter() {
          if let Some(found) = walk(child) {
            return Some(found);
          }
        }
        None
      }
    }
  }

  walk(line)
}

fn line_contains_text_offset(line: &FragmentNode, box_id: usize, offset: usize) -> bool {
  fn walk(node: &FragmentNode, box_id: usize, offset: usize) -> bool {
    match &node.content {
      FragmentContent::Text {
        box_id: Some(id),
        source_range: Some(range),
        ..
      } if *id == box_id => offset >= range.start() && offset < range.end(),
      _ => node.children.iter().any(|child| walk(child, box_id, offset)),
    }
  }

  walk(line, box_id, offset)
}

fn line_contains_replaced(line: &FragmentNode, box_id: usize) -> bool {
  fn walk(node: &FragmentNode, box_id: usize) -> bool {
    match &node.content {
      FragmentContent::Replaced { box_id: Some(id), .. } if *id == box_id => true,
      _ => node.children.iter().any(|child| walk(child, box_id)),
    }
  }

  walk(line, box_id)
}

fn flow_start_for_token_in_layout(
  node: &FragmentNode,
  token: &BreakToken,
  abs_start: f32,
  parent_block_size: f32,
  axis: &FragmentAxis,
) -> Option<f32> {
  let node_block_size = axis.block_size(&node.bounds);
  let (node_abs_start, node_abs_end) = axis.flow_range(abs_start, parent_block_size, &node.bounds);

  match token {
    BreakToken::Start => return Some(0.0),
    BreakToken::End => return None,
    BreakToken::Block { box_id, offset_bits } => {
      if matches!(node.content, FragmentContent::Block { box_id: Some(id) } if id == *box_id) {
        let mut offset = f32::from_bits(*offset_bits);
        if !offset.is_finite() {
          offset = 0.0;
        }
        offset = offset.max(0.0);
        offset = offset.min((node_abs_end - node_abs_start).max(0.0));
        return Some(node_abs_start + offset);
      }
    }
    BreakToken::Text { box_id, offset } => {
      if matches!(node.content, FragmentContent::Line { .. })
        && line_contains_text_offset(node, *box_id, *offset)
      {
        return Some(node_abs_start);
      }
    }
    BreakToken::Replaced { box_id, offset_bits } => {
      let zero_bits = 0.0f32.to_bits();
      if *offset_bits == zero_bits
        && matches!(node.content, FragmentContent::Line { .. })
        && line_contains_replaced(node, *box_id)
      {
        return Some(node_abs_start);
      }
      if matches!(node.content, FragmentContent::Replaced { box_id: Some(id), .. } if id == *box_id)
      {
        let mut offset = f32::from_bits(*offset_bits);
        if !offset.is_finite() {
          offset = 0.0;
        }
        offset = offset.max(0.0);
        offset = offset.min((node_abs_end - node_abs_start).max(0.0));
        return Some(node_abs_start + offset);
      }
    }
  }

  for child in node.children.iter() {
    if let Some(found) = flow_start_for_token_in_layout(
      child,
      token,
      node_abs_start,
      node_block_size,
      axis,
    ) {
      return Some(found);
    }
  }
  None
}

fn next_break_token_after_pos(
  node: &FragmentNode,
  pos: f32,
  abs_start: f32,
  parent_block_size: f32,
  axis: &FragmentAxis,
  best: &mut Option<(f32, BreakToken)>,
) {
  let node_block_size = axis.block_size(&node.bounds);
  let (node_abs_start, _node_abs_end) = axis.flow_range(abs_start, parent_block_size, &node.bounds);

  let consider = |best: &mut Option<(f32, BreakToken)>, start: f32, token: BreakToken| {
    if start + EPSILON < pos {
      return;
    }
    match best {
      Some((best_start, _)) if *best_start <= start + EPSILON => {}
      _ => *best = Some((start, token)),
    }
  };

  match &node.content {
    FragmentContent::Line { .. } => {
      if let Some(token) = token_from_line_start(node) {
        consider(best, node_abs_start, token);
      }
    }
    FragmentContent::Block { box_id: Some(id) } => {
      consider(
        best,
        node_abs_start,
        BreakToken::Block {
          box_id: *id,
          offset_bits: f32_to_canonical_bits(0.0),
        },
      );
    }
    FragmentContent::Replaced { box_id: Some(id), .. } => {
      consider(
        best,
        node_abs_start,
        BreakToken::Replaced {
          box_id: *id,
          offset_bits: f32_to_canonical_bits(0.0),
        },
      );
    }
    _ => {}
  }

  for child in node.children.iter() {
    next_break_token_after_pos(child, pos, node_abs_start, node_block_size, axis, best);
  }
}

fn line_start_containing_pos(
  node: &FragmentNode,
  pos: f32,
  abs_start: f32,
  parent_block_size: f32,
  axis: &FragmentAxis,
  best: &mut Option<f32>,
) {
  let node_block_size = axis.block_size(&node.bounds);
  let (node_abs_start, node_abs_end) = axis.flow_range(abs_start, parent_block_size, &node.bounds);

  if matches!(node.content, FragmentContent::Line { .. })
    && pos > node_abs_start + EPSILON
    && pos < node_abs_end - EPSILON
    && node_abs_end > node_abs_start + EPSILON
  {
    *best = Some(best.map_or(node_abs_start, |prev| prev.max(node_abs_start)));
  }

  for child in node.children.iter() {
    line_start_containing_pos(child, pos, node_abs_start, node_block_size, axis, best);
  }
}

fn continuation_token_for_pos(
  node: &FragmentNode,
  pos: f32,
  abs_start: f32,
  parent_block_size: f32,
  axis: &FragmentAxis,
) -> Option<BreakToken> {
  // Prefer the deepest fragment (max depth) that spans the page boundary. If multiple candidates are
  // at the same depth, prefer the one with the smallest block-size range, and then prefer replaced
  // elements over generic blocks.
  fn walk(
    node: &FragmentNode,
    pos: f32,
    abs_start: f32,
    parent_block_size: f32,
    axis: &FragmentAxis,
    depth: usize,
    best: &mut Option<(usize, f32, bool, BreakToken)>,
  ) {
    let node_block_size = axis.block_size(&node.bounds);
    let (node_abs_start, node_abs_end) = axis.flow_range(abs_start, parent_block_size, &node.bounds);

    if pos > node_abs_start + EPSILON
      && pos < node_abs_end - EPSILON
      && node_abs_end > node_abs_start + EPSILON
    {
      let span = node_abs_end - node_abs_start;
      let mut consider = |is_replaced: bool, token: BreakToken| {
        match best {
          Some((best_depth, best_span, best_is_replaced, _))
            if *best_depth > depth
              || (*best_depth == depth
                && (*best_span < span - EPSILON
                  || ((*best_span - span).abs() <= EPSILON && *best_is_replaced >= is_replaced))) => {}
          _ => *best = Some((depth, span, is_replaced, token)),
        }
      };

      match &node.content {
        FragmentContent::Block { box_id: Some(id) } => {
          let offset = (pos - node_abs_start).max(0.0);
          consider(
            false,
            BreakToken::Block {
              box_id: *id,
              offset_bits: f32_to_canonical_bits(offset),
            },
          );
        }
        FragmentContent::Replaced { box_id: Some(id), .. } => {
          let offset = (pos - node_abs_start).max(0.0);
          consider(
            true,
            BreakToken::Replaced {
              box_id: *id,
              offset_bits: f32_to_canonical_bits(offset),
            },
          );
        }
        _ => {}
      }
    }

    for child in node.children.iter() {
      walk(child, pos, node_abs_start, node_block_size, axis, depth + 1, best);
    }
  }

  let mut best: Option<(usize, f32, bool, BreakToken)> = None;
  walk(node, pos, abs_start, parent_block_size, axis, 0, &mut best);
  best.map(|(_, _, _, token)| token)
}

fn trim_line_children_to_text_offset(node: &mut FragmentNode, box_id: usize, offset: usize) -> bool {
  if let FragmentContent::Text {
    box_id: Some(id),
    source_range,
    text,
    shaped,
    ..
  } = &mut node.content
  {
    if *id == box_id {
      if let Some(range) = source_range.as_mut() {
        let start = range.start();
        let end = range.end();
        if offset >= start && offset < end {
          let rel = offset.saturating_sub(start);
          let full = text.as_ref();
          if rel >= full.len() {
            text.clone_from(&Arc::from(""));
            *shaped = None;
            *source_range = TextSourceRange::new(offset.min(end)..end);
            return true;
          }
          let slice_start = if full.is_char_boundary(rel) {
            rel
          } else {
            full
              .char_indices()
              .map(|(idx, _)| idx)
              .find(|idx| *idx >= rel)
              .unwrap_or(full.len())
          };
          let new_start = start.saturating_add(slice_start).min(end);
          let sliced = &full[slice_start..];
          text.clone_from(&Arc::from(sliced));
          *shaped = None;
          *source_range = TextSourceRange::new(new_start..end);
          return true;
        }
      }
    }
  }

  let children = node.children_mut();
  for idx in 0..children.len() {
    if trim_line_children_to_text_offset(&mut children[idx], box_id, offset) {
      if idx > 0 {
        children.drain(0..idx);
      }
      return true;
    }
  }
  false
}

fn trim_line_children_to_replaced(node: &mut FragmentNode, box_id: usize) -> bool {
  match &node.content {
    FragmentContent::Replaced { box_id: Some(id), .. } if *id == box_id => return true,
    _ => {}
  }

  let children = node.children_mut();
  for idx in 0..children.len() {
    if trim_line_children_to_replaced(&mut children[idx], box_id) {
      if idx > 0 {
        children.drain(0..idx);
      }
      return true;
    }
  }
  false
}

fn trim_clipped_content_start(content: &mut FragmentNode, axis: &FragmentAxis, token: &BreakToken) {
  let zero_bits = 0.0f32.to_bits();
  let mut target: Option<BreakToken> = match token {
    BreakToken::Text { .. } => Some(token.clone()),
    BreakToken::Replaced { offset_bits, .. } if *offset_bits == zero_bits => Some(token.clone()),
    _ => None,
  };
  if target.is_none() {
    return;
  }

  fn walk(node: &mut FragmentNode, axis: &FragmentAxis, token: &BreakToken, done: &mut bool) {
    if *done {
      return;
    }
    if matches!(node.content, FragmentContent::Line { .. }) {
      let matched = match token {
        BreakToken::Text { box_id, offset } => line_contains_text_offset(node, *box_id, *offset),
        BreakToken::Replaced { box_id, .. } => line_contains_replaced(node, *box_id),
        _ => false,
      };
      if matched {
        match token {
          BreakToken::Text { box_id, offset } => {
            let _ = trim_line_children_to_text_offset(node, *box_id, *offset);
          }
          BreakToken::Replaced { box_id, .. } => {
            let _ = trim_line_children_to_replaced(node, *box_id);
          }
          _ => {}
        }

        let children = node.children_mut();
        let min_inline = children
          .iter()
          .map(|c| inline_start_for_axis(axis, &c.bounds))
          .fold(f32::INFINITY, f32::min);
        if min_inline.is_finite() && min_inline.abs() > EPSILON {
          for child in children.iter_mut() {
            translate_inline_for_axis(child, axis, -min_inline);
          }
        }

        *done = true;
        return;
      }
    }
    for child in node.children_mut().iter_mut() {
      walk(child, axis, token, done);
      if *done {
        return;
      }
    }
  }

  let token = target.take().unwrap();
  let mut done = false;
  walk(content, axis, &token, &mut done);
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

#[derive(Debug, Clone)]
struct CachedLayout {
  root: FragmentNode,
  total_height: f32,
  forced_boundaries: Vec<ForcedBoundary>,
  page_name_transitions: Vec<PageNameTransition>,
  string_set_events: Vec<StringSetEvent>,
}

impl CachedLayout {
  fn from_root(
    mut root: FragmentNode,
    style: &ResolvedPageStyle,
    fallback_page_name: Option<&str>,
    axes: FragmentAxes,
    string_set_collector: &StringSetEventCollector,
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
    apply_flex_parallel_flow_forced_break_shifts(
      &mut root,
      axes,
      style_block_size,
      FragmentationContext::Page,
    );
    let page_name_transitions = collect_page_name_transitions(&root, &axis, fallback_page_name);

    let forced = collect_forced_boundaries_for_pagination_with_axes(&root, 0.0, axes);

    let content_height = parallel_flow_content_extent(
      &root,
      axes,
      Some(style_block_size),
      FragmentationContext::Page,
    );
    let total_height = if content_height > EPSILON {
      content_height
    } else {
      style_block_size
    };
    let forced = dedup_forced_boundaries(forced);

    let mut string_set_events = string_set_collector.collect(&root, axes);
    string_set_events.sort_by(|a, b| {
      a.abs_block
        .partial_cmp(&b.abs_block)
        .unwrap_or(Ordering::Equal)
    });

    Self {
      root,
      total_height,
      forced_boundaries: forced,
      page_name_transitions,
      string_set_events,
    }
  }
}

#[derive(Debug)]
struct PageBreakPlanner {
  analyzer: FragmentationAnalyzer,
}

impl PageBreakPlanner {
  fn new(layout: &CachedLayout, axes: FragmentAxes, fragmentainer_size_hint: f32) -> Self {
    let mut analyzer = FragmentationAnalyzer::new(
      &layout.root,
      FragmentationContext::Page,
      axes,
      true,
      Some(fragmentainer_size_hint),
    );
    analyzer.add_forced_break_positions(
      layout
        .page_name_transitions
        .iter()
        .skip(1)
        .map(|transition| transition.position),
    );
    // The generic fragmentation path (`fragment_tree`) assumes fixed-size fragmentainers and uses a
    // heuristic to avoid selecting between-sibling boundaries that land far before the limit (which
    // would shift subsequent fragments). For the @page-aware paginator we can safely enable early
    // sibling breaks when paginating along a non-default axis (e.g. vertical writing modes). For
    // the common horizontal flow we keep the heuristic enabled to avoid creating short pages when
    // only large empty gaps are available.
    let allow_early = axes.block_axis() != PhysicalAxis::Y || !axes.block_positive();
    analyzer.set_allow_early_sibling_breaks(allow_early);
    Self { analyzer }
  }

  fn next_boundary(
    &mut self,
    start: f32,
    fragmentainer_size: f32,
    total_extent: f32,
  ) -> Result<f32, LayoutError> {
    self
      .analyzer
      .next_boundary(start, fragmentainer_size, total_extent)
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
  let mut break_planners: HashMap<PageLayoutKey, PageBreakPlanner> = HashMap::new();
  let base_style_for_margins = Some(root_style.as_ref());
  let fallback_page_name = initial_page_name.as_deref();
  let string_set_collector = StringSetEventCollector::new(box_tree);

  if let Some((style, root)) = initial_layout {
    let key = PageLayoutKey::new(style, style_hash, font_generation);
    layouts.entry(key).or_insert_with(|| {
      CachedLayout::from_root(
        root.clone(),
        style,
        fallback_page_name,
        root_axes,
        &string_set_collector,
      )
    });
  }

  let mut first_page_side = if root_axes.page_progression_is_ltr() {
    PageSide::Right
  } else {
    PageSide::Left
  };

  let (base_total_height, base_page_names, base_forced, base_root) = loop {
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
      &string_set_collector,
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
      base_layout.total_height.max(EPSILON),
      base_layout.page_name_transitions.clone(),
      base_layout.forced_boundaries.clone(),
      base_layout.root.clone(),
    );
  };

  let mut string_set_carry: HashMap<String, String> = HashMap::new();
  let mut running_element_state = crate::layout::running_elements::RunningElementState::default();
  let mut string_set_seen_boxes: HashSet<usize> = HashSet::new();
  let mut string_event_indices: HashMap<PageLayoutKey, usize> = HashMap::new();

  let mut pages: Vec<(
    FragmentNode,
    ResolvedPageStyle,
    HashMap<String, RunningStringValues>,
    HashMap<String, RunningElementValues>,
    bool,
  )> = Vec::new();
  let base_root_block_size = root_axis.block_size(&base_root.bounds);
  let mut token = BreakToken::Start;
  let mut page_index = 0usize;
  let mut pending_footnotes: VecDeque<PendingFootnote> = VecDeque::new();

  loop {
    let start_in_base = match &token {
      BreakToken::Start => 0.0,
      BreakToken::End => base_total_height,
      _ => flow_start_for_token_in_layout(&base_root, &token, 0.0, base_root_block_size, &root_axis)
        .ok_or_else(|| {
          LayoutError::MissingContext(
            "pagination break token could not be resolved in base layout".into(),
          )
        })?,
    };
    let mut page_name = page_name_for_position(&base_page_names, start_in_base, fallback_page_name);
    let side = page_side_for_index(page_index, first_page_side);
    let required_side = required_page_side(&base_forced, start_in_base);
    let is_blank_page = required_side.map_or(false, |required| required != side);

    if matches!(token, BreakToken::End) && pending_footnotes.is_empty() && !is_blank_page {
      break;
    }

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
      &string_set_collector,
      enable_layout_cache,
    )?;
    let axis = root_axis;

    let mut total_height = layout.total_height;
    if total_height <= EPSILON {
      break;
    }
    let mut root_block_size = axis.block_size(&layout.root.bounds);

    // Determine where the current continuation token maps into this page's layout. This must be
    // done *before* building the page root, because resolving the effective page name may require
    // re-laying out with a different @page style.
    let mut start = 0.0f32;
    if !is_blank_page && pending_footnotes.is_empty() {
      start = match &token {
        BreakToken::Start => 0.0,
        BreakToken::End => total_height,
        _ => flow_start_for_token_in_layout(&layout.root, &token, 0.0, root_block_size, &axis)
          .ok_or_else(|| {
            LayoutError::MissingContext("pagination break token could not be resolved".into())
          })?,
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
          &string_set_collector,
          enable_layout_cache,
        )?;
        total_height = layout.total_height;
        if total_height <= EPSILON {
          break;
        }
        root_block_size = axis.block_size(&layout.root.bounds);
        start = match &token {
          BreakToken::Start => 0.0,
          BreakToken::End => total_height,
          _ => flow_start_for_token_in_layout(&layout.root, &token, 0.0, root_block_size, &axis)
            .ok_or_else(|| {
              LayoutError::MissingContext("pagination break token could not be resolved".into())
            })?,
        };
      }
      if start >= total_height - EPSILON {
        break;
      }
    }

    let mut fixed_fragments = Vec::new();
    collect_fixed_fragments(&layout.root, Point::ZERO, &mut fixed_fragments);

    // CSS Page 3 requires:
    // - page background is always the bottom-most layer.
    // - the page border/box-shadow + document contents behave as a single stacking context at z=0
    //   relative to page-margin boxes (which can paint in front or behind via z-index).
    //
    // We implement this by splitting the @page style into:
    // - `page_background_style`: background only (painted by the page root fragment).
    // - `document_wrapper_style`: border/box-shadow only (painted by a synthetic wrapper fragment
    //   that also contains all document content and establishes a stacking context at z=0).
    let mut page_background_style = page_style.page_style.clone();
    page_background_style.box_shadow.clear();
    page_background_style.border_top_width = Length::px(0.0);
    page_background_style.border_right_width = Length::px(0.0);
    page_background_style.border_bottom_width = Length::px(0.0);
    page_background_style.border_left_width = Length::px(0.0);

    let mut document_wrapper_style = page_style.page_style.clone();
    document_wrapper_style.reset_background_to_initial();

    let page_bounds = Rect::from_xywh(
      0.0,
      0.0,
      page_style.total_size.width,
      page_style.total_size.height,
    );
    let mut page_root = FragmentNode::new_block_styled(
      page_bounds,
      Vec::new(),
      Arc::new(page_background_style),
    );
    let mut document_wrapper = FragmentNode::new_block_styled(
      page_bounds,
      Vec::new(),
      Arc::new(document_wrapper_style),
    );
    document_wrapper.force_stacking_context_with_z_index(0);
    let mut page_running_elements: HashMap<String, RunningElementValues> = HashMap::new();

    let mut string_slice_start = 0.0f32;
    let mut string_slice_end = 0.0f32;

    let mut next_token = token.clone();

    if !is_blank_page {
      if !pending_footnotes.is_empty() {
        // When a footnote body overflows the page, render continuation pages that contain only the
        // remaining footnote content. This ensures pagination makes forward progress instead of
        // endlessly deferring the overflowing footnote call.
        let content_bounds = Rect::from_xywh(
          page_style.content_origin.x,
          page_style.content_origin.y,
          page_style.content_size.width,
          page_style.content_size.height,
        );
        document_wrapper
          .children_mut()
          .push(FragmentNode::new_block(content_bounds, Vec::new()));

        let page_block = if axis.block_is_horizontal {
          page_style.content_size.width
        } else {
          page_style.content_size.height
        }
        .max(1.0);

        // Simple, fixed separator rule: 1px solid currentColor.
        let separator_block = 1.0;
        let mut remaining = (page_block - separator_block).max(0.0);
        let mut slices: Vec<FragmentNode> = Vec::new();

        while remaining > EPSILON {
          let Some(pending) = pending_footnotes.front_mut() else {
            break;
          };
          if pending.offset >= pending.total_extent - EPSILON {
            pending_footnotes.pop_front();
            continue;
          }

          let next = pending.analyzer.next_boundary_with_cursor(
            pending.offset,
            remaining,
            pending.total_extent,
            &mut pending.opportunity_cursor,
          )?;
          if next <= pending.offset + EPSILON {
            break;
          }

          let root_block_size = axis.block_size(&pending.root.bounds);
          if let Some(mut slice) = clip_node(
            &pending.root,
            &axis,
            pending.offset,
            next,
            0.0,
            pending.offset,
            next,
            root_block_size,
            page_index,
            0,
            FragmentationContext::Page,
            remaining,
            root_axes,
          )? {
            normalize_fragment_margins(
              &mut slice,
              pending.offset <= EPSILON,
              next >= pending.total_extent - EPSILON,
              &axis,
            );
            let slice_block = axis.block_size(&slice.bounds).max(0.0);
            if slice_block > EPSILON {
              remaining -= slice_block;
              slices.push(slice);
            }
          }

          pending.offset = next;
          if pending.offset >= pending.total_extent - EPSILON {
            pending_footnotes.pop_front();
          }
        }

        if let Some(footnote_area) = build_footnote_area_fragment(&page_style, &axis, &slices) {
          document_wrapper.children_mut().push(footnote_area);
        }
      } else {
      let page_block = if axis.block_is_horizontal {
        page_style.content_size.width
      } else {
        page_style.content_size.height
      }
      .max(1.0);
      let planner = break_planners
        .entry(key)
        .or_insert_with(|| PageBreakPlanner::new(layout, root_axes, page_block));
      let mut end_candidate = planner
        .next_boundary(start, page_block, total_height)?
        .min(total_height);
      if end_candidate <= start + EPSILON {
        // Guard against degenerate boundary selection. Pagination must always make progress; fall
        // back to the fragmentainer limit if the analyzer returns a non-advancing boundary.
        end_candidate = (start + page_block).min(total_height);
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
        // Re-run boundary selection against the reduced main-flow block-size so widows/orphans and
        // avoid/forced break hints are evaluated against the *effective* fragmentainer size (page
        // content box minus reserved footnote area).
        if adjusted_end > start + EPSILON && adjusted_end + EPSILON < end_candidate {
          let effective_block = (adjusted_end - start).max(0.0);
          if effective_block > EPSILON {
            let selected = planner
              .next_boundary(start, effective_block, total_height)?
              .min(adjusted_end);
            if selected > start + EPSILON {
              end = selected;
            } else {
              end = adjusted_end;
            }
          } else {
            end = adjusted_end;
          }
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
        trim_clipped_content_start(&mut content, &axis, &token);
        if page_footnotes.is_empty() {
          page_footnotes = collect_footnotes_for_page(&content, &axis);
        }
        // Generate footnote body slices. Oversized footnotes are fragmented and continued on
        // subsequent pages.
        let separator_block = 1.0;
        let main_block = (end - start).max(0.0);
        let mut available_for_oversize = (page_block - main_block - separator_block).max(0.0);
        let mut footnote_slices: Vec<FragmentNode> = Vec::new();
        for occ in page_footnotes.iter() {
          let mut snapshot = occ.snapshot.clone();
          let offset = Point::new(-snapshot.bounds.x(), -snapshot.bounds.y());
          snapshot.translate_root_in_place(offset);

          let body_block = axis.block_size(&snapshot.bounds).max(0.0);
          let is_oversize = body_block + separator_block > page_block + EPSILON;
          if !is_oversize {
            footnote_slices.push(snapshot);
            continue;
          }

          if available_for_oversize <= EPSILON {
            available_for_oversize = (page_block - separator_block).max(0.0);
          }

          let analyzer = FragmentationAnalyzer::new(
            &snapshot,
            FragmentationContext::Page,
            root_axes,
            true,
            Some(page_block),
          );
          let total_extent = analyzer.content_extent().max(EPSILON);
          let mut pending = PendingFootnote {
            root: snapshot,
            analyzer,
            opportunity_cursor: 0,
            offset: 0.0,
            total_extent,
          };

          let next = pending.analyzer.next_boundary_with_cursor(
            pending.offset,
            available_for_oversize,
            pending.total_extent,
            &mut pending.opportunity_cursor,
          )?;

          let root_block_size = axis.block_size(&pending.root.bounds);
          if let Some(mut slice) = clip_node(
            &pending.root,
            &axis,
            pending.offset,
            next,
            0.0,
            pending.offset,
            next,
            root_block_size,
            page_index,
            0,
            FragmentationContext::Page,
            available_for_oversize,
            root_axes,
          )? {
            normalize_fragment_margins(
              &mut slice,
              pending.offset <= EPSILON,
              next >= pending.total_extent - EPSILON,
              &axis,
            );
            footnote_slices.push(slice);
          }

          pending.offset = next;
          if pending.offset < pending.total_extent - EPSILON {
            pending_footnotes.push_back(pending);
          }
        }

        let footnote_area = build_footnote_area_fragment(&page_style, &axis, &footnote_slices);

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
        document_wrapper.children_mut().push(content);
        if let Some(footnote_area) = footnote_area {
          document_wrapper.children_mut().push(footnote_area);
        }
      }

      string_slice_start = start;
      string_slice_end = end;

      let mut token_pos = end;
      let mut containing_line = None;
      line_start_containing_pos(
        &layout.root,
        end,
        0.0,
        root_block_size,
        &axis,
        &mut containing_line,
      );
      if let Some(line_start) = containing_line {
        token_pos = line_start;
      }

      let mut best_next: Option<(f32, BreakToken)> = None;
      next_break_token_after_pos(
        &layout.root,
        token_pos,
        0.0,
        root_block_size,
        &axis,
        &mut best_next,
      );
      let continuation = if token_pos >= total_height - EPSILON {
        None
      } else {
        continuation_token_for_pos(&layout.root, token_pos, 0.0, root_block_size, &axis)
      };
      next_token = match best_next {
        // If the next break token begins at the current boundary, prefer it over continuation
        // offsets. (This is especially important for line/text tokens, which enable stable trimming
        // when rewrapping causes the token offset to land mid-line.)
        Some((next_start, tok)) if (next_start - token_pos).abs() < EPSILON => tok,
        Some((_next_start, tok)) => continuation.unwrap_or(tok),
        None => continuation.unwrap_or(BreakToken::End),
      };
      }
    }

    for mut fixed in fixed_fragments {
      translate_fragment(
        &mut fixed,
        page_style.content_origin.x,
        page_style.content_origin.y,
      );
      document_wrapper.children_mut().push(fixed);
    }

    page_root.children_mut().push(document_wrapper);

    let page_strings = if is_blank_page {
      snapshot_running_strings(&string_set_carry)
    } else {
      let idx = string_event_indices.entry(key).or_insert(0);
      running_strings_for_page(
        &layout.string_set_events,
        idx,
        &mut string_set_carry,
        &mut string_set_seen_boxes,
        string_slice_start,
        string_slice_end,
      )
    };

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

    pages.push((
      page_root,
      page_style,
      page_strings,
      page_running_elements,
      is_blank_page,
    ));
    if !is_blank_page {
      token = next_token;
    }
    page_index += 1;
  }

  // Suppress trailing pages that contain no in-flow content beyond the root HTML/body wrappers (and
  // any repeated `position: fixed` fragments). This most commonly happens when the only remaining
  // "content" is trailing margins (e.g. `body { margin-bottom }`), which should not force an extra
  // empty page at the end of pagination.
  while pages.len() > 1 {
    let Some((page, _style, _strings, _running, is_blank_page)) = pages.last() else {
      break;
    };
    if *is_blank_page {
      break;
    }
    if page_has_in_flow_content(page) {
      break;
    }
    pages.pop();
  }

  if pages.is_empty() {
    return Ok(vec![base_root]);
  }

  let count = pages.len();
  let mut page_roots = Vec::with_capacity(count);
  for (idx, (mut page, style, running_strings, running_elements, _is_blank_page)) in
    pages.into_iter().enumerate()
  {
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

  apply_page_stacking(
    &mut pages,
    box_tree.root.style.writing_mode,
    options.stacking,
  );

  Ok(pages)
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
    let inherited_for_children = if applies {
      used.as_str()
    } else {
      inherited_used
    };

    let mut child_starts: Vec<f32> = Vec::with_capacity(node.children.len());
    let mut child_ends: Vec<f32> = Vec::with_capacity(node.children.len());
    let mut child_values: Vec<Option<PropagatedPageValues>> =
      Vec::with_capacity(node.children.len());

    for child in node.children.iter() {
      let child_block_size = axis.block_size(&child.bounds);
      let (child_abs_start, child_abs_end) =
        axis.flow_range(abs_start, parent_block_size, &child.bounds);
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

  transitions.sort_by(|a, b| {
    a.position
      .partial_cmp(&b.position)
      .unwrap_or(Ordering::Equal)
  });

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
  seen_boxes: &mut HashSet<usize>,
  start: f32,
  end: f32,
) -> HashMap<String, RunningStringValues> {
  let start_boundary = start - EPSILON;
  let mut emitted_boxes: HashSet<usize> = HashSet::new();
  while *idx < events.len() && events[*idx].abs_block < start_boundary {
    let event = &events[*idx];
    if should_apply_string_set_event(event, seen_boxes, &mut emitted_boxes) {
      carry.insert(event.name.clone(), event.value.clone());
    }
    *idx += 1;
  }

  let mut snapshot = snapshot_running_strings(carry);

  while *idx < events.len() && events[*idx].abs_block < end {
    let event = &events[*idx];
    if !should_apply_string_set_event(event, seen_boxes, &mut emitted_boxes) {
      *idx += 1;
      continue;
    }
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

fn snapshot_running_strings(
  carry: &HashMap<String, String>,
) -> HashMap<String, RunningStringValues> {
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
  snapshot
}

fn should_apply_string_set_event(
  event: &StringSetEvent,
  seen_boxes: &mut HashSet<usize>,
  emitted_boxes: &mut HashSet<usize>,
) -> bool {
  let Some(box_id) = event.box_id else {
    return true;
  };
  if emitted_boxes.contains(&box_id) {
    return true;
  }
  if seen_boxes.contains(&box_id) {
    return false;
  }
  seen_boxes.insert(box_id);
  emitted_boxes.insert(box_id);
  true
}

#[derive(Debug, Clone)]
struct FootnoteOccurrence {
  pos: f32,
  snapshot: FragmentNode,
}

#[derive(Debug)]
struct PendingFootnote {
  root: FragmentNode,
  analyzer: FragmentationAnalyzer,
  opportunity_cursor: usize,
  offset: f32,
  total_extent: f32,
}

fn collect_footnotes_for_page(
  root: &FragmentNode,
  axis: &crate::layout::fragmentation::FragmentAxis,
) -> Vec<FootnoteOccurrence> {
  let mut occurrences: Vec<FootnoteOccurrence> = Vec::new();
  let root_block_size = axis.block_size(&root.bounds);
  collect_footnote_occurrences(root, 0.0, root_block_size, axis, None, &mut occurrences);
  occurrences.sort_by(|a, b| a.pos.partial_cmp(&b.pos).unwrap_or(Ordering::Equal));
  occurrences
}

fn collect_footnote_occurrences(
  node: &FragmentNode,
  abs_flow_start: f32,
  parent_block_size: f32,
  axis: &crate::layout::fragmentation::FragmentAxis,
  current_line_start: Option<f32>,
  out: &mut Vec<FootnoteOccurrence>,
) {
  let node_block_size = axis.block_size(&node.bounds);
  let abs_block = axis.flow_range(abs_flow_start, parent_block_size, &node.bounds).0;
  let current_line_start = if matches!(node.content, FragmentContent::Line { .. }) {
    Some(abs_block)
  } else {
    current_line_start
  };

  if let FragmentContent::FootnoteAnchor { snapshot } = &node.content {
    out.push(FootnoteOccurrence {
      // Footnote calls are typically inside line boxes (which are indivisible during pagination).
      // Use the line's flow-start as the call position so deferring a footnote moves the whole
      // line, not just part of it.
      pos: current_line_start.unwrap_or(abs_block),
      snapshot: (**snapshot).clone(),
    });
  }

  for child in node.children.iter() {
    collect_footnote_occurrences(child, abs_block, node_block_size, axis, current_line_start, out);
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

  // Simple, fixed separator rule: 1px solid currentColor.
  let separator_block = 1.0;

  let mut included = 0usize;
  let mut total_footnote_block = 0.0f32;
  for occ in footnotes {
    let body_block = axis.block_size(&occ.snapshot.bounds).max(0.0);
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
    let body_block = axis.block_size(&footnotes[0].snapshot.bounds).max(0.0);
    let footnote_overflows_page = body_block + separator_block > page_block + EPSILON;
    let footnote_consumes_page = body_block + separator_block >= page_block - EPSILON;
    if (footnote_overflows_page || footnote_consumes_page) && footnotes[0].pos <= EPSILON {
      // If the call is already at the start of the page and the footnote would otherwise consume
      // the entire page, reserve some space so the footnote body can start and continue on
      // subsequent pages.
      let min_footnote_content = 1.0;
      let max_main_block = (page_block - separator_block - min_footnote_content).max(0.0);
      let desired_main_block = (page_block * 0.5).min(max_main_block);
      let mut end = start + desired_main_block;
      // If there are additional footnote calls in this clipped slice, stop before the next call so
      // later footnotes are deferred until this overflowing footnote finishes. (When calls share
      // the same line box, we can't split them here.)
      if footnotes.len() > 1 && footnotes[1].pos > EPSILON {
        end = end.min(start + footnotes[1].pos);
      }
      end
    } else {
      // No footnote calls fit alongside their bodies; defer the first call to the next page.
      start + footnotes[0].pos
    }
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
  slices: &[FragmentNode],
) -> Option<FragmentNode> {
  if slices.is_empty() {
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

  // Simple, fixed separator rule: 1px solid currentColor.
  let separator_block = 1.0;
  let flow_box_start_to_physical = |flow_offset: f32, block_size: f32, parent_block_size: f32| {
    if axis.block_positive {
      flow_offset
    } else {
      parent_block_size - flow_offset - block_size
    }
  };

  let mut snapshots: Vec<FragmentNode> = Vec::with_capacity(slices.len());
  let mut total_footnote_block = 0.0f32;
  for occ in slices {
    let mut snapshot = occ.clone();
    let offset = Point::new(-snapshot.bounds.x(), -snapshot.bounds.y());
    snapshot.translate_root_in_place(offset);
    total_footnote_block += axis.block_size(&snapshot.bounds).max(0.0);
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
    let body_block = axis.block_size(&snapshot.bounds).max(0.0);
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

#[derive(Debug, Clone)]
struct MarginBoxPlan {
  style: Arc<ComputedStyle>,
  content: MarginBoxPlanContent,
}

#[derive(Debug, Clone)]
enum MarginBoxPlanContent {
  /// `content: element(...)` and it resolved to a single snapshot, so we can reuse it directly
  /// without running layout.
  SnapshotOnly { snapshot: FragmentNode },
  /// A regular margin box laid out via a dedicated box tree, plus any running-element snapshots to
  /// append after layout.
  BoxTree {
    tree: BoxTree,
    element_snapshots: Vec<FragmentNode>,
  },
}

fn build_margin_box_fragments(
  style: &ResolvedPageStyle,
  font_ctx: &FontContext,
  page_index: usize,
  page_count: usize,
  running_strings: &HashMap<String, RunningStringValues>,
  running_elements: &HashMap<String, RunningElementValues>,
) -> Vec<FragmentNode> {
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

  let mut plans: HashMap<PageMarginArea, MarginBoxPlan> = HashMap::new();
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
    // CSS Page 3: `display` does not apply to page-margin boxes, so treat them as block
    // containers for layout purposes (even if the computed style says otherwise).
    let mut owned_style = box_style.clone();
    owned_style.display = Display::Block;
    let style_arc = Arc::new(owned_style);
    let box_style = style_arc.as_ref();
    let plan = if let ContentValue::Items(items) = &box_style.content_value {
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
            MarginBoxPlan {
              style: style_arc,
              content: MarginBoxPlanContent::SnapshotOnly { snapshot },
            }
          } else {
            // The only content item was `element()` but it resolved to nothing, so the margin box is
            // effectively empty.
            let root = BoxNode::new_block(style_arc.clone(), FormattingContextType::Block, vec![]);
            MarginBoxPlan {
              style: style_arc,
              content: MarginBoxPlanContent::BoxTree {
                tree: BoxTree::new(root),
                element_snapshots,
              },
            }
          }
        } else {
          let children = build_margin_box_children(
            box_style,
            page_index,
            page_count,
            running_strings,
            &style_arc,
          );
          let root = BoxNode::new_block(style_arc.clone(), FormattingContextType::Block, children);
          MarginBoxPlan {
            style: style_arc,
            content: MarginBoxPlanContent::BoxTree {
              tree: BoxTree::new(root),
              element_snapshots,
            },
          }
        }
      } else {
        let children = build_margin_box_children(
          box_style,
          page_index,
          page_count,
          running_strings,
          &style_arc,
        );
        let root = BoxNode::new_block(style_arc.clone(), FormattingContextType::Block, children);
        MarginBoxPlan {
          style: style_arc,
          content: MarginBoxPlanContent::BoxTree {
            tree: BoxTree::new(root),
            element_snapshots,
          },
        }
      }
    } else {
      let children = build_margin_box_children(
        box_style,
        page_index,
        page_count,
        running_strings,
        &style_arc,
      );
      let root = BoxNode::new_block(style_arc.clone(), FormattingContextType::Block, children);
      MarginBoxPlan {
        style: style_arc,
        content: MarginBoxPlanContent::BoxTree {
          tree: BoxTree::new(root),
          element_snapshots: Vec::new(),
        },
      }
    };

    plans.insert(area, plan);
  }

  if plans.is_empty() {
    return Vec::new();
  }

  // Intrinsic sizing probes for page-margin boxes can require layout to measure text. Use a shared
  // engine so shaping caches (fonts) are reused, but compute the box sizes per CSS Page 3.
  let intrinsic_engine =
    LayoutEngine::with_font_context(LayoutConfig::new(style.total_size), font_ctx.clone());
  let bounds_map = compute_margin_box_bounds(style, &plans, &intrinsic_engine);

  let mut fragments = Vec::new();
  for area in CANONICAL_MARGIN_AREA_ORDER {
    let Some(plan) = plans.get(&area) else {
      continue;
    };
    let Some(bounds) = bounds_map.get(&area).copied() else {
      continue;
    };
    if bounds.width() <= 0.0 || bounds.height() <= 0.0 {
      continue;
    }

    match &plan.content {
      MarginBoxPlanContent::SnapshotOnly { snapshot } => {
        let mut fragment = FragmentNode::new_block_styled(
          bounds,
          vec![snapshot.clone()],
          plan.style.clone(),
        );
        fragment.force_stacking_context_with_z_index(plan.style.z_index.unwrap_or(0));
        fragments.push(fragment);
      }
      MarginBoxPlanContent::BoxTree {
        tree: box_tree,
        element_snapshots,
      } => {
        let config = LayoutConfig::new(Size::new(bounds.width(), bounds.height()));
        let engine = LayoutEngine::with_font_context(config, font_ctx.clone());
        if let Ok(mut tree) = engine.layout_tree(box_tree) {
          tree.root.bounds = Rect::from_xywh(0.0, 0.0, bounds.width(), bounds.height());
          tree.root.scroll_overflow = Rect::from_xywh(
            0.0,
            0.0,
            tree.root.scroll_overflow.width().max(bounds.width()),
            tree.root.scroll_overflow.height().max(bounds.height()),
          );
          let mut next_y = tree
            .root
            .children
            .iter()
            .map(|child| child.bounds.y() + child.bounds.height())
            .fold(0.0, f32::max);
          for mut snapshot in element_snapshots.iter().cloned() {
            translate_fragment(&mut snapshot, 0.0, next_y);
            next_y += snapshot.bounds.height();
            tree.root.children_mut().push(snapshot);
          }
          translate_fragment(&mut tree.root, bounds.x(), bounds.y());
          tree
            .root
            .force_stacking_context_with_z_index(plan.style.z_index.unwrap_or(0));
          fragments.push(tree.root);
        }
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
  string_set_collector: &StringSetEventCollector,
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
    let layout = CachedLayout::from_root(
      layout_tree.root,
      style,
      fallback_page_name,
      root_axes,
      string_set_collector,
    );
    cache.insert(key, layout);
  }

  Ok(cache.get(&key).expect("layout cache just populated"))
}

#[derive(Debug, Clone, Copy)]
struct VariableMarginBox {
  generated: bool,
  /// Used outer size when the variable dimension is not `auto`.
  outer: Option<f32>,
  /// Outer min-content size (used when width/height is auto).
  outer_min: f32,
  /// Outer max-content size (used when width/height is auto).
  outer_max: f32,
  /// Outer min constraint from `min-width`/`min-height`.
  min_constraint: f32,
  /// Outer max constraint from `max-width`/`max-height`.
  max_constraint: f32,
  /// Margin on the start edge of the variable dimension (auto resolves to 0).
  margin_start: f32,
  /// Margin on the end edge of the variable dimension (auto resolves to 0).
  margin_end: f32,
}

impl VariableMarginBox {
  fn not_generated() -> Self {
    Self {
      generated: false,
      outer: Some(0.0),
      outer_min: 0.0,
      outer_max: 0.0,
      min_constraint: 0.0,
      max_constraint: 0.0,
      margin_start: 0.0,
      margin_end: 0.0,
    }
  }

  fn margin_sum(self) -> f32 {
    self.margin_start + self.margin_end
  }

  fn fixed_outer(mut self, outer: f32) -> Self {
    let outer = if outer.is_finite() {
      outer.max(0.0)
    } else {
      0.0
    };
    self.outer = Some(outer);
    self.outer_min = outer;
    self.outer_max = outer;
    self
  }

  fn border_box_size(self, used_outer: f32) -> f32 {
    let size = used_outer - self.margin_sum();
    if size.is_finite() {
      size.max(0.0)
    } else {
      0.0
    }
  }
}

fn physical_axis_is_inline(writing_mode: WritingMode, axis: PhysicalAxis) -> bool {
  match (inline_axis_is_horizontal(writing_mode), axis) {
    (true, PhysicalAxis::X) => true,
    (true, PhysicalAxis::Y) => false,
    (false, PhysicalAxis::X) => false,
    (false, PhysicalAxis::Y) => true,
  }
}

fn resolve_len(
  style: &ComputedStyle,
  len: Length,
  percent_base: Option<f32>,
  viewport: Size,
) -> f32 {
  resolve_length_with_percentage_metrics(
    len,
    percent_base,
    viewport,
    style.font_size,
    style.root_font_size,
    Some(style),
    None,
  )
  .unwrap_or(0.0)
}

fn flex_fit_two_auto_boxes(
  a: &VariableMarginBox,
  c: &VariableMarginBox,
  available: f32,
) -> (f32, f32) {
  let available = if available.is_finite() {
    available.max(0.0)
  } else {
    0.0
  };
  let min_a = a.outer_min.max(0.0);
  let max_a = a.outer_max.max(min_a);
  let min_c = c.outer_min.max(0.0);
  let max_c = c.outer_max.max(min_c);

  let sum_max = max_a + max_c;
  if sum_max < available {
    let flex_space = available - sum_max;
    let mut factor_a = max_a;
    let mut factor_c = max_c;
    let mut sum_factors = factor_a + factor_c;
    if sum_factors == 0.0 {
      factor_a = 1.0;
      factor_c = 1.0;
      sum_factors = 2.0;
    }
    let used_a = max_a + flex_space * factor_a / sum_factors;
    let used_c = max_c + flex_space * factor_c / sum_factors;
    return (used_a.max(0.0), used_c.max(0.0));
  }

  let sum_min = min_a + min_c;
  let flex_space = available - sum_min;
  let (mut factor_a, mut factor_c) = if sum_min < available {
    // Case 2: distribute between min-content and max-content.
    ((max_a - min_a).max(0.0), (max_c - min_c).max(0.0))
  } else {
    // Case 3: shrink below min-content proportionally.
    (min_a, min_c)
  };
  let mut sum_factors = factor_a + factor_c;
  if sum_factors == 0.0 {
    factor_a = 1.0;
    factor_c = 1.0;
    sum_factors = 2.0;
  }
  let used_a = min_a + flex_space * factor_a / sum_factors;
  let used_c = min_c + flex_space * factor_c / sum_factors;
  (used_a.max(0.0), used_c.max(0.0))
}

fn distribute_two_boxes(
  a: &VariableMarginBox,
  c: &VariableMarginBox,
  available: f32,
) -> (f32, f32) {
  match (a.outer, c.outer) {
    (Some(a_fixed), Some(c_fixed)) => (a_fixed.max(0.0), c_fixed.max(0.0)),
    (None, Some(c_fixed)) => ((available - c_fixed).max(0.0), c_fixed.max(0.0)),
    (Some(a_fixed), None) => (a_fixed.max(0.0), (available - a_fixed).max(0.0)),
    (None, None) => flex_fit_two_auto_boxes(a, c, available),
  }
}

fn compute_used_outer_sizes(
  a: &VariableMarginBox,
  b: &VariableMarginBox,
  c: &VariableMarginBox,
  available: f32,
) -> (f32, f32, f32) {
  if !b.generated {
    let (used_a, used_c) = distribute_two_boxes(a, c, available);
    return (used_a, 0.0, used_c);
  }

  let used_b = if let Some(fixed) = b.outer {
    fixed.max(0.0)
  } else {
    // Resolve B's auto size using the imaginary AC box.
    let ac = if a.outer.is_some() && c.outer.is_some() {
      // When both side boxes have definite sizes, AC is also definite.
      let fixed_a = a.outer.unwrap_or(0.0);
      let fixed_c = c.outer.unwrap_or(0.0);
      VariableMarginBox::not_generated().fixed_outer(2.0 * fixed_a.max(fixed_c))
    } else {
      // Otherwise, AC is treated like an auto-sized box whose intrinsic sizes are derived from the
      // larger of A/C (including fixed sizes). This matches CSS Page 3's imaginary AC box used to
      // resolve B while preserving centering.
      let outer_min = 2.0 * a.outer_min.max(c.outer_min);
      let outer_max = 2.0 * a.outer_max.max(c.outer_max);
      VariableMarginBox {
        generated: true,
        outer: None,
        outer_min,
        outer_max,
        min_constraint: 0.0,
        max_constraint: f32::INFINITY,
        margin_start: 0.0,
        margin_end: 0.0,
      }
    };
    let (used_b, _used_ac) = distribute_two_boxes(b, &ac, available);
    used_b
  };

  let remaining = (available - used_b).max(0.0);
  let used_a = a.outer.unwrap_or(remaining / 2.0).max(0.0);
  let used_c = c.outer.unwrap_or(remaining / 2.0).max(0.0);
  (used_a, used_b, used_c)
}

fn compute_used_outer_sizes_with_minmax(
  a: VariableMarginBox,
  b: VariableMarginBox,
  c: VariableMarginBox,
  available: f32,
) -> (f32, f32, f32) {
  let (tentative_a, tentative_b, tentative_c) = compute_used_outer_sizes(&a, &b, &c, available);

  let mut a2 = a;
  let mut b2 = b;
  let mut c2 = c;
  let mut max_violation = false;
  for (used, box_) in [
    (tentative_a, &mut a2),
    (tentative_b, &mut b2),
    (tentative_c, &mut c2),
  ] {
    if !box_.generated {
      continue;
    }
    let max = box_.max_constraint;
    if max.is_finite() && used > max + EPSILON {
      *box_ = box_.fixed_outer(max);
      max_violation = true;
    }
  }
  let (after_max_a, after_max_b, after_max_c) = if max_violation {
    compute_used_outer_sizes(&a2, &b2, &c2, available)
  } else {
    (tentative_a, tentative_b, tentative_c)
  };

  let mut a3 = a2;
  let mut b3 = b2;
  let mut c3 = c2;
  let mut min_violation = false;
  for (used, box_) in [
    (after_max_a, &mut a3),
    (after_max_b, &mut b3),
    (after_max_c, &mut c3),
  ] {
    if !box_.generated {
      continue;
    }
    if used + EPSILON < box_.min_constraint {
      *box_ = box_.fixed_outer(box_.min_constraint);
      min_violation = true;
    }
  }
  if min_violation {
    compute_used_outer_sizes(&a3, &b3, &c3, available)
  } else {
    (after_max_a, after_max_b, after_max_c)
  }
}

fn variable_box_metrics(
  area: PageMarginArea,
  plans: &HashMap<PageMarginArea, MarginBoxPlan>,
  intrinsic_engine: &LayoutEngine,
  variable_axis: PhysicalAxis,
  variable_base: f32,
  percentage_base: f32,
  viewport: Size,
) -> VariableMarginBox {
  let Some(plan) = plans.get(&area) else {
    return VariableMarginBox::not_generated();
  };
  let style = plan.style.as_ref();

  let (margin_start, margin_end) = match variable_axis {
    PhysicalAxis::X => (
      style
        .margin_left
        .map(|len| resolve_len(style, len, Some(percentage_base), viewport))
        .unwrap_or(0.0),
      style
        .margin_right
        .map(|len| resolve_len(style, len, Some(percentage_base), viewport))
        .unwrap_or(0.0),
    ),
    PhysicalAxis::Y => (
      style
        .margin_top
        .map(|len| resolve_len(style, len, Some(percentage_base), viewport))
        .unwrap_or(0.0),
      style
        .margin_bottom
        .map(|len| resolve_len(style, len, Some(percentage_base), viewport))
        .unwrap_or(0.0),
    ),
  };

  let (padding_start, padding_end, border_start, border_end) = match variable_axis {
    PhysicalAxis::X => (
      resolve_len(style, style.padding_left, Some(percentage_base), viewport).max(0.0),
      resolve_len(style, style.padding_right, Some(percentage_base), viewport).max(0.0),
      resolve_len(
        style,
        style.used_border_left_width(),
        Some(percentage_base),
        viewport,
      )
      .max(0.0),
      resolve_len(
        style,
        style.used_border_right_width(),
        Some(percentage_base),
        viewport,
      )
      .max(0.0),
    ),
    PhysicalAxis::Y => (
      resolve_len(style, style.padding_top, Some(percentage_base), viewport).max(0.0),
      resolve_len(style, style.padding_bottom, Some(percentage_base), viewport).max(0.0),
      resolve_len(
        style,
        style.used_border_top_width(),
        Some(percentage_base),
        viewport,
      )
      .max(0.0),
      resolve_len(
        style,
        style.used_border_bottom_width(),
        Some(percentage_base),
        viewport,
      )
      .max(0.0),
    ),
  };
  let edges = padding_start + padding_end + border_start + border_end;
  let margin_sum = margin_start + margin_end;

  let mut min_constraint = 0.0;
  let mut max_constraint = f32::INFINITY;
  match variable_axis {
    PhysicalAxis::X => {
      if let Some(min_len) = style.min_width {
        min_constraint = resolve_len(style, min_len, Some(variable_base), viewport);
      }
      if let Some(max_len) = style.max_width {
        max_constraint = resolve_len(style, max_len, Some(variable_base), viewport);
      }
    }
    PhysicalAxis::Y => {
      if let Some(min_len) = style.min_height {
        min_constraint = resolve_len(style, min_len, Some(variable_base), viewport);
      }
      if let Some(max_len) = style.max_height {
        max_constraint = resolve_len(style, max_len, Some(variable_base), viewport);
      }
    }
  }
  let min_border = border_size_from_box_sizing(min_constraint.max(0.0), edges, style.box_sizing);
  let max_border = if max_constraint.is_finite() {
    border_size_from_box_sizing(max_constraint.max(0.0), edges, style.box_sizing)
  } else {
    f32::INFINITY
  };
  let min_outer = (min_border + margin_sum).max(0.0);
  let max_outer = if max_border.is_finite() {
    (max_border + margin_sum).max(min_outer)
  } else {
    f32::INFINITY
  };

  let mut result = VariableMarginBox {
    generated: true,
    outer: None,
    outer_min: 0.0,
    outer_max: 0.0,
    min_constraint: min_outer,
    max_constraint: max_outer,
    margin_start,
    margin_end,
  };

  let computed_size = match variable_axis {
    PhysicalAxis::X => style.width,
    PhysicalAxis::Y => style.height,
  };
  if let Some(len) = computed_size {
    let resolved = resolve_len(style, len, Some(variable_base), viewport).max(0.0);
    let border = border_size_from_box_sizing(resolved, edges, style.box_sizing);
    let outer = border + margin_sum;
    return result.fixed_outer(outer);
  }

  // Auto size: use intrinsic sizing to compute min/max content contributions.
  let (intrinsic_min, intrinsic_max) = match &plan.content {
    MarginBoxPlanContent::SnapshotOnly { snapshot } => {
      let value = match variable_axis {
        PhysicalAxis::X => snapshot.bounds.width(),
        PhysicalAxis::Y => snapshot.bounds.height(),
      };
      (value.max(0.0), value.max(0.0))
    }
    MarginBoxPlanContent::BoxTree { tree, .. } => {
      let axis_is_inline = physical_axis_is_inline(style.writing_mode, variable_axis);
      let intrinsic = |mode| {
        if axis_is_inline {
          intrinsic_engine.compute_intrinsic_size(&tree.root, mode)
        } else {
          intrinsic_engine.compute_intrinsic_block_size(&tree.root, mode)
        }
      };
      let min = intrinsic(IntrinsicSizingMode::MinContent).unwrap_or(0.0);
      let max = match intrinsic(IntrinsicSizingMode::MaxContent) {
        Ok(v) => v,
        Err(LayoutError::Timeout { .. }) => {
          return VariableMarginBox {
            generated: true,
            outer: None,
            outer_min: (min.max(0.0) + margin_sum).max(0.0),
            outer_max: (min.max(0.0) + margin_sum).max(0.0),
            min_constraint: min_outer,
            max_constraint: max_outer,
            margin_start,
            margin_end,
          }
          .fixed_outer((min.max(0.0) + margin_sum).max(0.0))
        }
        Err(_) => min,
      };
      (min.max(0.0), max.max(0.0))
    }
  };

  result.outer_min = (intrinsic_min + margin_sum).max(0.0);
  result.outer_max = (intrinsic_max + margin_sum).max(result.outer_min);
  result
}

fn compute_margin_box_bounds(
  style: &ResolvedPageStyle,
  plans: &HashMap<PageMarginArea, MarginBoxPlan>,
  intrinsic_engine: &LayoutEngine,
) -> HashMap<PageMarginArea, Rect> {
  let trimmed_width = style.page_size.width - 2.0 * style.trim;
  let trimmed_height = style.page_size.height - 2.0 * style.trim;
  let origin_x = style.bleed + style.trim;
  let origin_y = style.bleed + style.trim;
  let ml = style.margin_left;
  let mr = style.margin_right;
  let mt = style.margin_top;
  let mb = style.margin_bottom;

  let available_width = (trimmed_width - ml - mr).max(0.0);
  let available_height = (trimmed_height - mt - mb).max(0.0);
  let viewport = style.total_size;

  let mut out: HashMap<PageMarginArea, Rect> = HashMap::new();
  let rect = |x: f32, y: f32, w: f32, h: f32| -> Option<Rect> {
    if w <= 0.0 || h <= 0.0 {
      None
    } else {
      Some(Rect::from_xywh(x, y, w, h))
    }
  };

  // Corner boxes are fixed-size intersections of the adjacent page margins.
  let corner_bounds: &[(PageMarginArea, f32, f32, f32, f32)] = &[
    (PageMarginArea::TopLeftCorner, origin_x, origin_y, ml, mt),
    (
      PageMarginArea::TopRightCorner,
      origin_x + trimmed_width - mr,
      origin_y,
      mr,
      mt,
    ),
    (
      PageMarginArea::BottomRightCorner,
      origin_x + trimmed_width - mr,
      origin_y + trimmed_height - mb,
      mr,
      mb,
    ),
    (
      PageMarginArea::BottomLeftCorner,
      origin_x,
      origin_y + trimmed_height - mb,
      ml,
      mb,
    ),
  ];
  for (area, x, y, w, h) in corner_bounds {
    if plans.contains_key(area) {
      if let Some(r) = rect(*x, *y, *w, *h) {
        out.insert(*area, r);
      }
    }
  }

  // Helper to compute the three boxes on a side (A/B/C) per CSS Page 3.
  let compute_horizontal = |y: f32,
                            height: f32,
                            a_area: PageMarginArea,
                            b_area: PageMarginArea,
                            c_area: PageMarginArea,
                            out: &mut HashMap<PageMarginArea, Rect>| {
    if height <= 0.0 || available_width <= 0.0 {
      return;
    }
    let cb_x = origin_x + ml;
    let cb_w = available_width;
    let a = variable_box_metrics(
      a_area,
      plans,
      intrinsic_engine,
      PhysicalAxis::X,
      cb_w,
      cb_w,
      viewport,
    );
    let b = variable_box_metrics(
      b_area,
      plans,
      intrinsic_engine,
      PhysicalAxis::X,
      cb_w,
      cb_w,
      viewport,
    );
    let c = variable_box_metrics(
      c_area,
      plans,
      intrinsic_engine,
      PhysicalAxis::X,
      cb_w,
      cb_w,
      viewport,
    );
    let (used_a, used_b, used_c) = compute_used_outer_sizes_with_minmax(a, b, c, cb_w);

    let a_outer_x = cb_x;
    let b_outer_x = cb_x + (cb_w - used_b) / 2.0;
    let c_outer_x = cb_x + cb_w - used_c;

    let a_border_w = a.border_box_size(used_a);
    let b_border_w = b.border_box_size(used_b);
    let c_border_w = c.border_box_size(used_c);

    if plans.contains_key(&a_area) {
      if let Some(r) = rect(a_outer_x + a.margin_start, y, a_border_w, height) {
        out.insert(a_area, r);
      }
    }
    if plans.contains_key(&b_area) {
      if let Some(r) = rect(b_outer_x + b.margin_start, y, b_border_w, height) {
        out.insert(b_area, r);
      }
    }
    if plans.contains_key(&c_area) {
      if let Some(r) = rect(c_outer_x + c.margin_start, y, c_border_w, height) {
        out.insert(c_area, r);
      }
    }
  };

  let compute_vertical = |x: f32,
                          width: f32,
                          a_area: PageMarginArea,
                          b_area: PageMarginArea,
                          c_area: PageMarginArea,
                          out: &mut HashMap<PageMarginArea, Rect>| {
    if width <= 0.0 || available_height <= 0.0 {
      return;
    }
    let cb_y = origin_y + mt;
    let cb_h = available_height;
    let a = variable_box_metrics(
      a_area,
      plans,
      intrinsic_engine,
      PhysicalAxis::Y,
      cb_h,
      width,
      viewport,
    );
    let b = variable_box_metrics(
      b_area,
      plans,
      intrinsic_engine,
      PhysicalAxis::Y,
      cb_h,
      width,
      viewport,
    );
    let c = variable_box_metrics(
      c_area,
      plans,
      intrinsic_engine,
      PhysicalAxis::Y,
      cb_h,
      width,
      viewport,
    );
    let (used_a, used_b, used_c) = compute_used_outer_sizes_with_minmax(a, b, c, cb_h);

    let a_outer_y = cb_y;
    let b_outer_y = cb_y + (cb_h - used_b) / 2.0;
    let c_outer_y = cb_y + cb_h - used_c;

    let a_border_h = a.border_box_size(used_a);
    let b_border_h = b.border_box_size(used_b);
    let c_border_h = c.border_box_size(used_c);

    if plans.contains_key(&a_area) {
      if let Some(r) = rect(x, a_outer_y + a.margin_start, width, a_border_h) {
        out.insert(a_area, r);
      }
    }
    if plans.contains_key(&b_area) {
      if let Some(r) = rect(x, b_outer_y + b.margin_start, width, b_border_h) {
        out.insert(b_area, r);
      }
    }
    if plans.contains_key(&c_area) {
      if let Some(r) = rect(x, c_outer_y + c.margin_start, width, c_border_h) {
        out.insert(c_area, r);
      }
    }
  };

  // Top and bottom: variable width.
  compute_horizontal(
    origin_y,
    mt,
    PageMarginArea::TopLeft,
    PageMarginArea::TopCenter,
    PageMarginArea::TopRight,
    &mut out,
  );
  compute_horizontal(
    origin_y + trimmed_height - mb,
    mb,
    PageMarginArea::BottomLeft,
    PageMarginArea::BottomCenter,
    PageMarginArea::BottomRight,
    &mut out,
  );

  // Left and right: variable height.
  compute_vertical(
    origin_x,
    ml,
    PageMarginArea::LeftTop,
    PageMarginArea::LeftMiddle,
    PageMarginArea::LeftBottom,
    &mut out,
  );
  compute_vertical(
    origin_x + trimmed_width - mr,
    mr,
    PageMarginArea::RightTop,
    PageMarginArea::RightMiddle,
    PageMarginArea::RightBottom,
    &mut out,
  );

  out
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
