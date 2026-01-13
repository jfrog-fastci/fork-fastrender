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
  layout_style_fingerprint, set_fragmentainer_axes_hint, set_fragmentainer_block_offset_hint,
  set_fragmentainer_block_size_hint, set_footnote_area_inline_size_hint, IntrinsicSizingMode,
  LayoutError,
};
use crate::layout::fragmentation::{
  apply_abspos_parallel_flow_forced_break_shifts, apply_flex_parallel_flow_forced_break_shifts,
  apply_float_parallel_flow_forced_break_shifts, apply_grid_parallel_flow_forced_break_shifts,
  apply_table_cell_parallel_flow_forced_break_shifts,
  clip_node,
  collect_forced_boundaries_for_pagination_with_axes, normalize_fragment_margins,
  parallel_flow_content_extent, propagate_fragment_metadata, ForcedBoundary, FragmentAxis,
  FragmentationAnalyzer, FragmentationContext,
};
use crate::layout::running_elements::{
  running_elements_for_page, running_elements_for_page_fragment,
};
use crate::layout::running_strings::{StringSetEvent, StringSetEventCollector};
use crate::layout::utils::{border_size_from_box_sizing, resolve_length_with_percentage_metrics};
use crate::style::content::{
  ContentContext, ContentItem, ContentValue, CounterStyle, RunningElementValues,
  RunningStringValues,
};
use crate::style::display::{Display, FormattingContextType};
use crate::style::page::{resolve_page_style, PageSide, ResolvedPageStyle};
use crate::style::position::Position;
use crate::style::types::{FootnoteDisplay, FootnotePolicy, WritingMode};
use crate::style::values::Length;
use crate::style::{
  block_axis_is_horizontal, inline_axis_is_horizontal, inline_axis_positive, ComputedStyle,
};
use crate::text::font_loader::FontContext;
use crate::tree::box_tree::{
  BoxNode, BoxTree, CrossOriginAttribute, ImageDecodingAttribute, ImageLoadingAttribute,
  ReplacedType, SrcsetCandidate, SrcsetDescriptor,
};
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

fn snapshot_running_elements_for_non_content_page(
  state: &mut crate::layout::running_elements::RunningElementState,
) -> HashMap<String, RunningElementValues> {
  // Blank pages and footnote-only continuation pages still resolve `element()` in @page margin
  // boxes by carrying the last running element snapshot from the previous in-flow content page.
  // No running element events should be consumed for these pages.
  let mut idx = 0usize;
  running_elements_for_page(&[], &mut idx, state, 0.0, 0.0)
}

fn srcset_candidates_for_url_image(url: &crate::style::types::UrlImage) -> Vec<SrcsetCandidate> {
  match url
    .override_resolution
    .filter(|d| d.is_finite() && *d > 0.0)
  {
    Some(density) => vec![SrcsetCandidate {
      url: url.url.clone(),
      descriptor: SrcsetDescriptor::Density(density),
    }],
    None => Vec::new(),
  }
}

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

  if matches!(
    node.content,
    FragmentContent::Text { .. } | FragmentContent::Replaced { .. }
  ) {
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
  Block {
    box_id: usize,
    offset_bits: u32,
  },
  /// Byte offset into the source text node identified by `box_id`.
  Text {
    box_id: usize,
    offset: usize,
  },
  /// Replaced-element continuation within the box identified by `box_id`.
  Replaced {
    box_id: usize,
    offset_bits: u32,
  },
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
      _ => node
        .children
        .iter()
        .any(|child| walk(child, box_id, offset)),
    }
  }

  walk(line, box_id, offset)
}

fn line_contains_replaced(line: &FragmentNode, box_id: usize) -> bool {
  fn walk(node: &FragmentNode, box_id: usize) -> bool {
    match &node.content {
      FragmentContent::Replaced {
        box_id: Some(id), ..
      } if *id == box_id => true,
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
  // `BreakToken::Block` and `BreakToken::Replaced` need special handling: a single box can produce
  // multiple fragments with the same `box_id` (e.g. multi-column layout clones fragments into
  // columns). Tokens store an offset into the *original* box, so resolving them needs to pick the
  // fragment slice whose `slice_info` range contains that offset.
  //
  // Scan the fragment tree to find the matching fragment with the greatest `slice_offset` that
  // still covers the requested offset. This avoids accidentally resolving to an earlier fragment
  // when multiple fragments overlap due to rounding or when the offset lands exactly on a slice
  // boundary.
  match token {
    BreakToken::Block {
      box_id,
      offset_bits,
    } => {
      let mut offset = f32::from_bits(*offset_bits);
      if !offset.is_finite() || offset < 0.0 {
        offset = 0.0;
      }
      let mut best: Option<(f32, f32)> = None; // (slice_offset, abs_pos)

      fn walk(
        node: &FragmentNode,
        box_id: usize,
        offset: f32,
        abs_start: f32,
        parent_block_size: f32,
        axis: &FragmentAxis,
        best: &mut Option<(f32, f32)>,
      ) {
        let node_block_size = axis.block_size(&node.bounds);
        let (node_abs_start, node_abs_end) =
          axis.flow_range(abs_start, parent_block_size, &node.bounds);
        if matches!(node.content, FragmentContent::Block { box_id: Some(id) } if id == box_id) {
          let mut slice_offset = node.slice_info.slice_offset;
          if !slice_offset.is_finite() || slice_offset < 0.0 {
            slice_offset = 0.0;
          }
          let span = (node_abs_end - node_abs_start).max(0.0);
          let rel = offset - slice_offset;
          if rel + EPSILON >= 0.0 && rel <= span + EPSILON {
            let rel = rel.clamp(0.0, span);
            let abs_pos = node_abs_start + rel;
            match best {
              Some((best_slice, best_pos))
                if *best_slice > slice_offset + EPSILON
                  || ((*best_slice - slice_offset).abs() <= EPSILON
                    && *best_pos >= abs_pos - EPSILON) => {}
              _ => *best = Some((slice_offset, abs_pos)),
            }
          }
        }

        for child in node.children.iter() {
          walk(
            child,
            box_id,
            offset,
            node_abs_start,
            node_block_size,
            axis,
            best,
          );
        }
      }

      walk(
        node,
        *box_id,
        offset,
        abs_start,
        parent_block_size,
        axis,
        &mut best,
      );
      return best.map(|(_, pos)| pos);
    }
    BreakToken::Replaced {
      box_id,
      offset_bits,
    } => {
      let mut offset = f32::from_bits(*offset_bits);
      if !offset.is_finite() || offset < 0.0 {
        offset = 0.0;
      }
      let zero_bits = 0.0f32.to_bits();
      if *offset_bits == zero_bits {
        // Special case: line-start tokens for inline replaced elements use `offset_bits = 0` and
        // should resolve to the containing line box start.
        fn walk_line(
          node: &FragmentNode,
          box_id: usize,
          abs_start: f32,
          parent_block_size: f32,
          axis: &FragmentAxis,
        ) -> Option<f32> {
          let node_block_size = axis.block_size(&node.bounds);
          let (node_abs_start, _node_abs_end) =
            axis.flow_range(abs_start, parent_block_size, &node.bounds);
          if matches!(node.content, FragmentContent::Line { .. })
            && line_contains_replaced(node, box_id)
          {
            return Some(node_abs_start);
          }
          for child in node.children.iter() {
            if let Some(found) = walk_line(child, box_id, node_abs_start, node_block_size, axis) {
              return Some(found);
            }
          }
          None
        }

        if let Some(found) = walk_line(node, *box_id, abs_start, parent_block_size, axis) {
          return Some(found);
        }
      }

      let mut best: Option<(f32, f32)> = None; // (slice_offset, abs_pos)

      fn walk(
        node: &FragmentNode,
        box_id: usize,
        offset: f32,
        abs_start: f32,
        parent_block_size: f32,
        axis: &FragmentAxis,
        best: &mut Option<(f32, f32)>,
      ) {
        let node_block_size = axis.block_size(&node.bounds);
        let (node_abs_start, node_abs_end) =
          axis.flow_range(abs_start, parent_block_size, &node.bounds);
        if matches!(node.content, FragmentContent::Replaced { box_id: Some(id), .. } if id == box_id)
        {
          let mut slice_offset = node.slice_info.slice_offset;
          if !slice_offset.is_finite() || slice_offset < 0.0 {
            slice_offset = 0.0;
          }
          let span = (node_abs_end - node_abs_start).max(0.0);
          let rel = offset - slice_offset;
          if rel + EPSILON >= 0.0 && rel <= span + EPSILON {
            let rel = rel.clamp(0.0, span);
            let abs_pos = node_abs_start + rel;
            match best {
              Some((best_slice, best_pos))
                if *best_slice > slice_offset + EPSILON
                  || ((*best_slice - slice_offset).abs() <= EPSILON
                    && *best_pos >= abs_pos - EPSILON) => {}
              _ => *best = Some((slice_offset, abs_pos)),
            }
          }
        }

        for child in node.children.iter() {
          walk(
            child,
            box_id,
            offset,
            node_abs_start,
            node_block_size,
            axis,
            best,
          );
        }
      }

      walk(
        node,
        *box_id,
        offset,
        abs_start,
        parent_block_size,
        axis,
        &mut best,
      );
      return best.map(|(_, pos)| pos);
    }
    _ => {}
  }

  let node_block_size = axis.block_size(&node.bounds);
  let (node_abs_start, node_abs_end) = axis.flow_range(abs_start, parent_block_size, &node.bounds);

  match token {
    BreakToken::Start => return Some(0.0),
    BreakToken::End => return None,
    BreakToken::Block {
      box_id,
      offset_bits,
    } => {
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
    BreakToken::Replaced {
      box_id,
      offset_bits,
    } => {
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
    if let Some(found) =
      flow_start_for_token_in_layout(child, token, node_abs_start, node_block_size, axis)
    {
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
      // `BreakToken::Block` offsets are expressed in the coordinate space of the original
      // unfragmented box. Use this fragment slice's offset so the token is unambiguous when a box
      // has already been fragmented (e.g. multi-column layout produces multiple fragments with the
      // same `box_id`).
      let mut slice_offset = node.slice_info.slice_offset;
      if !slice_offset.is_finite() || slice_offset < 0.0 {
        slice_offset = 0.0;
      }
      consider(
        best,
        node_abs_start,
        BreakToken::Block {
          box_id: *id,
          offset_bits: f32_to_canonical_bits(slice_offset),
        },
      );
    }
    FragmentContent::Replaced {
      box_id: Some(id), ..
    } => {
      let mut slice_offset = node.slice_info.slice_offset;
      if !slice_offset.is_finite() || slice_offset < 0.0 {
        slice_offset = 0.0;
      }
      consider(
        best,
        node_abs_start,
        BreakToken::Replaced {
          box_id: *id,
          offset_bits: f32_to_canonical_bits(slice_offset),
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
  under_column: bool,
) {
  let node_block_size = axis.block_size(&node.bounds);
  let (node_abs_start, node_abs_end) = axis.flow_range(abs_start, parent_block_size, &node.bounds);

  let under_column = under_column
    || node.fragmentainer.column_index.is_some()
    || node.fragmentainer.column_set_index.is_some();
  // Avoid snapping page boundaries to line-box starts inside nested fragmentation contexts such as
  // multi-column layout. Column content is already laid out in its own fragmentation coordinate
  // system, and treating those line fragments as part of the main paginated flow can cause the
  // continuation token to jump backwards (duplicating content across pages).
  let under_column = under_column || node.fragmentation.is_some();

  let contains_pos = matches!(node.content, FragmentContent::Line { .. })
    && pos > node_abs_start + EPSILON
    && pos < node_abs_end - EPSILON
    && node_abs_end > node_abs_start + EPSILON;
  if contains_pos && !under_column {
    *best = Some(best.map_or(node_abs_start, |prev| prev.max(node_abs_start)));
  }

  for child in node.children.iter() {
    line_start_containing_pos(
      child,
      pos,
      node_abs_start,
      node_block_size,
      axis,
      best,
      under_column,
    );
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
    let (node_abs_start, node_abs_end) =
      axis.flow_range(abs_start, parent_block_size, &node.bounds);

    if pos > node_abs_start + EPSILON
      && pos < node_abs_end - EPSILON
      && node_abs_end > node_abs_start + EPSILON
    {
      let span = node_abs_end - node_abs_start;
      let mut consider = |is_replaced: bool, token: BreakToken| match best {
        Some((best_depth, best_span, best_is_replaced, _))
          if *best_depth > depth
            || (*best_depth == depth
              && (*best_span < span - EPSILON
                || ((*best_span - span).abs() <= EPSILON && *best_is_replaced >= is_replaced))) => {
        }
        _ => *best = Some((depth, span, is_replaced, token)),
      };

      match &node.content {
        FragmentContent::Block { box_id: Some(id) } => {
          let mut offset = (pos - node_abs_start).max(0.0);
          // Encode the offset into the original unfragmented box by adding the slice offset for
          // this fragment.
          let slice_offset = node.slice_info.slice_offset;
          if slice_offset.is_finite() && slice_offset > 0.0 {
            offset += slice_offset;
          }
          consider(
            false,
            BreakToken::Block {
              box_id: *id,
              offset_bits: f32_to_canonical_bits(offset),
            },
          );
        }
        FragmentContent::Replaced {
          box_id: Some(id), ..
        } => {
          let mut offset = (pos - node_abs_start).max(0.0);
          let slice_offset = node.slice_info.slice_offset;
          if slice_offset.is_finite() && slice_offset > 0.0 {
            offset += slice_offset;
          }
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
      walk(
        child,
        pos,
        node_abs_start,
        node_block_size,
        axis,
        depth + 1,
        best,
      );
    }
  }

  let mut best: Option<(usize, f32, bool, BreakToken)> = None;
  walk(node, pos, abs_start, parent_block_size, axis, 0, &mut best);
  best.map(|(_, _, _, token)| token)
}

fn trim_line_children_to_text_offset(
  node: &mut FragmentNode,
  box_id: usize,
  offset: usize,
) -> bool {
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
    FragmentContent::Replaced {
      box_id: Some(id), ..
    } if *id == box_id => return true,
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

  let Some(token) = target.take() else {
    return;
  };
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

fn page_boundary_start_for_next_page(
  previous_boundary_start: f32,
  token_start: f32,
  forced: &[ForcedBoundary],
) -> f32 {
  if !token_start.is_finite() {
    return previous_boundary_start;
  }
  // The pagination continuation token encodes the start of the *next in-flow content*, but
  // forced page-side constraints apply to the page boundary itself. In paged multi-column layout,
  // the next content can start after the page boundary (e.g. when the first column is empty). When
  // advancing to a new token, treat the latest forced boundary between the previous page start and
  // the token's resolved position as the current page boundary.
  if token_start <= previous_boundary_start + EPSILON {
    return token_start;
  }

  let mut boundary = token_start;
  for forced_boundary in forced.iter() {
    if forced_boundary.position > previous_boundary_start + EPSILON
      && forced_boundary.position <= token_start + EPSILON
    {
      boundary = forced_boundary.position;
    }
  }

  boundary
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

fn sort_string_set_events(events: &mut [StringSetEvent]) {
  events.sort_by(|a, b| {
    let a_pos = if a.abs_block.is_finite() { a.abs_block } else { 0.0 };
    let b_pos = if b.abs_block.is_finite() { b.abs_block } else { 0.0 };
    // Canonicalize `-0.0` to `0.0` so positions that compare equal also tie-break by `sequence`.
    let a_pos = if a_pos == 0.0 { 0.0 } else { a_pos };
    let b_pos = if b_pos == 0.0 { 0.0 } else { b_pos };
    match a_pos.total_cmp(&b_pos) {
      Ordering::Equal => a.sequence.cmp(&b.sequence),
      other => other,
    }
  });
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

    apply_grid_parallel_flow_forced_break_shifts(
      &mut root,
      axes,
      style_block_size,
      FragmentationContext::Page,
    );
    apply_table_cell_parallel_flow_forced_break_shifts(
      &mut root,
      axes,
      style_block_size,
      FragmentationContext::Page,
    );
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
    apply_abspos_parallel_flow_forced_break_shifts(
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
    sort_string_set_events(&mut string_set_events);

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
  footnote_inline_bits: u64,
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

pub(crate) fn footnote_area_content_inline_size(style: &ResolvedPageStyle) -> Option<f32> {
  let inline_is_horizontal = inline_axis_is_horizontal(style.page_style.writing_mode);
  let page_inline = if inline_is_horizontal {
    style.content_size.width
  } else {
    style.content_size.height
  };
  let footnote_style = &style.footnote_style;
  let (padding_start, padding_end, border_start, border_end) = if inline_is_horizontal {
    (
      resolve_len(
        footnote_style,
        footnote_style.padding_left,
        Some(page_inline),
        style.total_size,
      )
      .max(0.0),
      resolve_len(
        footnote_style,
        footnote_style.padding_right,
        Some(page_inline),
        style.total_size,
      )
      .max(0.0),
      resolve_len(
        footnote_style,
        footnote_style.used_border_left_width(),
        Some(page_inline),
        style.total_size,
      )
      .max(0.0),
      resolve_len(
        footnote_style,
        footnote_style.used_border_right_width(),
        Some(page_inline),
        style.total_size,
      )
      .max(0.0),
    )
  } else {
    (
      resolve_len(
        footnote_style,
        footnote_style.padding_top,
        Some(page_inline),
        style.total_size,
      )
      .max(0.0),
      resolve_len(
        footnote_style,
        footnote_style.padding_bottom,
        Some(page_inline),
        style.total_size,
      )
      .max(0.0),
      resolve_len(
        footnote_style,
        footnote_style.used_border_top_width(),
        Some(page_inline),
        style.total_size,
      )
      .max(0.0),
      resolve_len(
        footnote_style,
        footnote_style.used_border_bottom_width(),
        Some(page_inline),
        style.total_size,
      )
      .max(0.0),
    )
  };
  let edges = padding_start + padding_end + border_start + border_end;
  let footnote_content_inline = (page_inline - edges).max(0.0);
  footnote_content_inline
    .is_finite()
    .then_some(footnote_content_inline.max(0.0))
}

impl PageLayoutKey {
  fn new(style: &ResolvedPageStyle, style_hash: u64, font_generation: u64) -> Self {
    let footnote_inline = footnote_area_content_inline_size(style).unwrap_or(0.0);
    Self {
      width_bits: f32_to_canonical_bits(style.content_size.width) as u64,
      height_bits: f32_to_canonical_bits(style.content_size.height) as u64,
      footnote_inline_bits: f32_to_canonical_bits(footnote_inline) as u64,
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
  let mut base_page_boundary_start = 0.0f32;
  let mut base_page_boundary_token = token.clone();
  let mut page_index = 0usize;
  let mut pending_footnotes: VecDeque<PendingFootnote> = VecDeque::new();
  let mut string_start_keyword_token = BreakToken::Start;

  loop {
    let start_in_base = match &token {
      BreakToken::Start => 0.0,
      BreakToken::End => base_total_height,
      _ => {
        flow_start_for_token_in_layout(&base_root, &token, 0.0, base_root_block_size, &root_axis)
          .ok_or_else(|| {
            LayoutError::MissingContext(
              "pagination break token could not be resolved in base layout".into(),
            )
          })?
      }
    };
    if token != base_page_boundary_token {
      base_page_boundary_start =
        page_boundary_start_for_next_page(base_page_boundary_start, start_in_base, &base_forced);
      base_page_boundary_token = token.clone();
    }
    let mut page_name = page_name_for_position(&base_page_names, start_in_base, fallback_page_name);
    let side = page_side_for_index(page_index, first_page_side);
    let required_side = required_page_side(&base_forced, base_page_boundary_start);
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
    if !is_blank_page && !matches!(token, BreakToken::End) {
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
    let page_box_origin = Point::new(
      page_style.bleed + page_style.trim + page_style.margin_left,
      page_style.bleed + page_style.trim + page_style.margin_top,
    );
    let page_box_size = Size::new(
      (page_style.page_size.width
        - 2.0 * page_style.trim
        - page_style.margin_left
        - page_style.margin_right)
        .max(0.0),
      (page_style.page_size.height
        - 2.0 * page_style.trim
        - page_style.margin_top
        - page_style.margin_bottom)
        .max(0.0),
    );
    let page_box_bounds = Rect::new(page_box_origin, page_box_size);
    let content_offset = Point::new(
      page_style.content_origin.x - page_box_origin.x,
      page_style.content_origin.y - page_box_origin.y,
    );
    let mut page_root =
      FragmentNode::new_block_styled(page_bounds, Vec::new(), Arc::new(page_background_style));
    let mut document_wrapper = FragmentNode::new_block_styled(
      page_box_bounds,
      Vec::new(),
      Arc::new(document_wrapper_style),
    );
    document_wrapper.force_stacking_context_with_z_index(0);
    let mut page_running_elements: HashMap<String, RunningElementValues> = HashMap::new();
    let page_string_start_keyword_pos = match &string_start_keyword_token {
      BreakToken::Start => 0.0,
      BreakToken::End => total_height,
      _ => flow_start_for_token_in_layout(
        &layout.root,
        &string_start_keyword_token,
        0.0,
        root_block_size,
        &axis,
      )
      .unwrap_or(start),
    };
    let mut next_string_start_keyword_token = string_start_keyword_token.clone();

    let mut string_slice_start = 0.0f32;
    let mut string_slice_end = 0.0f32;

    let mut next_token = token.clone();

    if !is_blank_page {
      let page_block = if axis.block_is_horizontal {
        page_style.content_size.width
      } else {
        page_style.content_size.height
      }
      .max(1.0);

      // Simple, fixed separator rule: 1px solid currentColor.
      let separator_block = 1.0;
      let footnote_style = &page_style.footnote_style;

      // Resolve @footnote border/padding along the page block axis. This consumes space on the page
      // even though the footnote bodies themselves are positioned by pagination.
      let viewport = page_style.total_size;
      let cb_width = page_style.content_size.width.max(0.0);
      let cb_height = page_style.content_size.height.max(0.0);
      let resolve_x = |len: Length| resolve_len(footnote_style, len, Some(cb_width), viewport).max(0.0);
      let resolve_y =
        |len: Length| resolve_len(footnote_style, len, Some(cb_height), viewport).max(0.0);

      let block_edges = if axis.block_is_horizontal {
        resolve_x(footnote_style.padding_left)
          + resolve_x(footnote_style.padding_right)
          + resolve_x(footnote_style.used_border_left_width())
          + resolve_x(footnote_style.used_border_right_width())
      } else {
        resolve_y(footnote_style.padding_top)
          + resolve_y(footnote_style.padding_bottom)
          + resolve_y(footnote_style.used_border_top_width())
          + resolve_y(footnote_style.used_border_bottom_width())
      };
      let footnote_overhead_block = block_edges + separator_block;

      // Resolve `@footnote { max-height: ... }` against the page content box block-size. This caps
      // the *footnote area* border-box size on pages that also contain in-flow content.
      let mut footnote_max_block_for_in_flow = page_block;
      if let Some(max_height) = footnote_style.max_height {
        let resolved = resolve_len(footnote_style, max_height, Some(page_block), viewport);
        if resolved.is_finite() {
          footnote_max_block_for_in_flow = resolved.max(0.0).min(page_block);
        }
      }

      if matches!(token, BreakToken::End) {
        // When the main-flow content is exhausted but deferred/oversize footnote bodies remain,
        // render continuation pages that contain only the remaining footnote content.
        page_running_elements =
          snapshot_running_elements_for_non_content_page(&mut running_element_state);
        let content_bounds = Rect::from_xywh(
          content_offset.x,
          content_offset.y,
          page_style.content_size.width,
          page_style.content_size.height,
        );
        document_wrapper
          .children_mut()
          .push(FragmentNode::new_block(content_bounds, Vec::new()));

        let mut remaining = (page_block - footnote_overhead_block).max(0.0);
        let mut slices: Vec<FragmentNode> = Vec::new();

        // If the page content box is too small to fit even a single fragment of footnote content
        // (after accounting for @footnote border/padding + separator), we must still drain
        // `pending_footnotes` to avoid
        // emitting an infinite sequence of empty continuation pages.
        if remaining <= EPSILON {
          pending_footnotes.pop_front();
        }

        while remaining > EPSILON {
          let Some(pending) = pending_footnotes.front_mut() else {
            break;
          };

          // Guard against pathological extents / offsets: treat non-finite extents as empty and
          // drop the pending footnote so pagination can make progress.
          if !pending.total_extent.is_finite() || pending.total_extent <= 0.0 {
            pending.total_extent = 0.0;
            pending.offset = 0.0;
            pending_footnotes.pop_front();
            continue;
          }

          if !pending.offset.is_finite() {
            pending.offset = 0.0;
          }
          pending.offset = pending.offset.max(0.0);
          if pending.offset > pending.total_extent {
            pending.offset = pending.total_extent;
          }

          if pending.offset >= pending.total_extent - EPSILON {
            pending_footnotes.pop_front();
            continue;
          }

          let next_candidate = pending.analyzer.next_boundary_with_cursor(
            pending.offset,
            remaining,
            pending.total_extent,
            &mut pending.opportunity_cursor,
          )?;
          let next = match resolve_pending_footnote_slice_boundary(
            pending.offset,
            remaining,
            pending.total_extent,
            next_candidate,
          ) {
            PendingFootnoteSliceBoundary::Advance(next) => next,
            PendingFootnoteSliceBoundary::Complete => {
              // Force completion so pagination doesn't get stuck emitting footnote-only pages.
              pending.offset = pending.total_extent;
              pending_footnotes.pop_front();
              continue;
            }
          };

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
            let is_first_fragment = pending.offset <= EPSILON;
            let is_last_fragment = next >= pending.total_extent - EPSILON;
            let break_before_forced =
              !is_first_fragment && pending.analyzer.is_forced_break_at(pending.offset);
            let break_after_forced = !is_last_fragment && pending.analyzer.is_forced_break_at(next);
            normalize_fragment_margins(
              &mut slice,
              is_first_fragment,
              is_last_fragment,
              break_before_forced,
              break_after_forced,
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

        if let Some(footnote_area) =
          build_footnote_area_fragment(&page_style, &axis, content_offset, &slices)
        {
          document_wrapper.children_mut().push(footnote_area);
        }
      } else {
        let mut reserved_pending_block = 0.0f32;
        if !pending_footnotes.is_empty() {
          // Reserve as much pending footnote body content as possible without exceeding the
          // footnote area's max-height (or the page size), accounting for the separator and
          // @footnote block-axis padding/border.
          let mut budget = (footnote_max_block_for_in_flow - footnote_overhead_block).max(0.0);
          for pending in pending_footnotes.iter() {
            if budget <= EPSILON {
              break;
            }
            let mut remaining_extent = pending.total_extent - pending.offset;
            if !remaining_extent.is_finite() {
              remaining_extent = 0.0;
            }
            remaining_extent = remaining_extent.max(0.0);
            let take = remaining_extent.min(budget);
            reserved_pending_block += take;
            budget -= take;
            if remaining_extent > take + EPSILON {
              break;
            }
          }
        }

        let page_block_for_content = (page_block - reserved_pending_block).max(1.0);
        let footnote_max_block_for_content_page =
          (footnote_max_block_for_in_flow - reserved_pending_block).max(0.0);
        let has_pending_overhead = reserved_pending_block > EPSILON;
        let planner = break_planners
          .entry(key)
          .or_insert_with(|| PageBreakPlanner::new(layout, root_axes, page_block));
        let mut end_candidate = planner
          .next_boundary(start, page_block_for_content, total_height)?
          .min(total_height);
        if end_candidate <= start + EPSILON {
          // Guard against degenerate boundary selection. Pagination must always make progress; fall
          // back to the fragmentainer limit if the analyzer returns a non-advancing boundary.
          end_candidate = (start + page_block_for_content).min(total_height);
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
          page_block_for_content,
          root_axes,
        )?;
        let mut page_footnotes: Vec<FootnoteOccurrence> = Vec::new();

        // If the page contains `float: footnote` calls, the footnote area at the bottom of the page
        // reduces the block-size available for main flow content. Use a provisional clip to
        // determine which footnotes are eligible for this page and adjust the end accordingly.
        if let Some(mut provisional) = clipped.take() {
          strip_fixed_fragments(&mut provisional);
          let is_first_fragment = page_index == 0;
          let is_last_fragment = end_candidate >= total_height - 0.01;
          let break_before_forced = start > EPSILON && planner.analyzer.is_forced_break_at(start);
          let break_after_forced =
            !is_last_fragment && planner.analyzer.is_forced_break_at(end_candidate);
          normalize_fragment_margins(
            &mut provisional,
            is_first_fragment,
            is_last_fragment,
            break_before_forced,
            break_after_forced,
            &axis,
          );
          let provisional_footnotes = collect_footnotes_for_page(&provisional, &axis);
          let adjusted_end = adjust_end_for_footnotes(
            start,
            end_candidate,
            page_block_for_content,
            footnote_max_block_for_content_page,
            footnote_overhead_block,
            has_pending_overhead,
            &provisional_footnotes,
            &axis,
          );
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

        // `clip_node` treats line boxes as indivisible: when a page boundary falls inside a line, it
        // moves the entire line to the *next* page. In that case, continuing pagination from the raw
        // boundary would cause the next page to start before this page's effective end, producing
        // overlapping/duplicated content (and sometimes a trailing blank page).
        //
        // Snap the break position back to the start of the line containing the boundary so both the
        // clipped page content and the continuation token agree on the same flow position.
        let mut containing_line = None;
        line_start_containing_pos(
          &layout.root,
          end,
          0.0,
          root_block_size,
          &axis,
          &mut containing_line,
          false,
        );
        if let Some(line_start) = containing_line {
          // When the line itself starts at (or before) the current page start (e.g. an oversized
          // line that overflows the fragmentainer), snapping would produce an empty page and stall
          // pagination. Only snap when it advances beyond the current page start.
          if line_start > start + EPSILON && line_start + EPSILON < end {
            end = line_start;
            page_footnotes.clear();
            clipped = None;
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
            page_block_for_content,
            root_axes,
          )?;
        }

        if let Some(mut content) = clipped {
          strip_fixed_fragments(&mut content);
          let break_before_forced = start > EPSILON && planner.analyzer.is_forced_break_at(start);
          let is_last_fragment = end >= total_height - 0.01;
          let break_after_forced = !is_last_fragment && planner.analyzer.is_forced_break_at(end);
          normalize_fragment_margins(
            &mut content,
            page_index == 0,
            is_last_fragment,
            break_before_forced,
            break_after_forced,
            &axis,
          );
          trim_clipped_content_start(&mut content, &axis, &token);
          if page_footnotes.is_empty() {
            page_footnotes = collect_footnotes_for_page(&content, &axis);
          }
          // Generate footnote body slices.
          //
          // Deferred footnote bodies (either because an earlier page couldn't fit them under
          // `footnote-policy: auto`, or because an oversized footnote body is being fragmented)
          // must be emitted in document order, and must never appear before their reference.
          let main_block = (end - start).max(0.0);
          let max_body_by_page = (page_block - main_block - footnote_overhead_block).max(0.0);
          let max_body_by_cap =
            (footnote_max_block_for_in_flow - footnote_overhead_block).max(0.0);
          let mut remaining_for_bodies = max_body_by_page.min(max_body_by_cap);
          let mut footnote_slices: Vec<FragmentNode> = Vec::new();

          // Helper to enqueue a deferred footnote body for placement on a later page.
          fn enqueue_pending(
            pending_footnotes: &mut VecDeque<PendingFootnote>,
            snapshot: FragmentNode,
            axes: FragmentAxes,
            page_block: f32,
          ) {
            let analyzer = FragmentationAnalyzer::new(
              &snapshot,
              FragmentationContext::Page,
              axes,
              true,
              Some(page_block),
            );
            let total_extent = analyzer.content_extent().max(EPSILON);
            pending_footnotes.push_back(PendingFootnote {
              root: snapshot,
              analyzer,
              opportunity_cursor: 0,
              offset: 0.0,
              total_extent,
            });
          }

          // Place deferred footnote content before considering new footnote calls for this page.
          if !pending_footnotes.is_empty() {
            // Pending footnote bodies are emitted before new footnote calls so bodies preserve
            // document order. If the current page cannot fit any footnote body content (e.g. due to
            // max-height), leave the pending footnotes queued for later pages.
            while remaining_for_bodies > EPSILON {
              let Some(pending) = pending_footnotes.front_mut() else {
                break;
              };

              // Guard against pathological extents / offsets: treat non-finite extents as empty and
              // drop the pending footnote so pagination can make progress.
              if !pending.total_extent.is_finite() || pending.total_extent <= 0.0 {
                pending.total_extent = 0.0;
                pending.offset = 0.0;
                pending_footnotes.pop_front();
                continue;
              }

              if !pending.offset.is_finite() {
                pending.offset = 0.0;
              }
              pending.offset = pending.offset.max(0.0);
              if pending.offset > pending.total_extent {
                pending.offset = pending.total_extent;
              }

              if pending.offset >= pending.total_extent - EPSILON {
                pending_footnotes.pop_front();
                continue;
              }

              let next_candidate = pending.analyzer.next_boundary_with_cursor(
                pending.offset,
                remaining_for_bodies,
                pending.total_extent,
                &mut pending.opportunity_cursor,
              )?;
              let next = match resolve_pending_footnote_slice_boundary(
                pending.offset,
                remaining_for_bodies,
                pending.total_extent,
                next_candidate,
              ) {
                PendingFootnoteSliceBoundary::Advance(next) => next,
                PendingFootnoteSliceBoundary::Complete => {
                  // Force completion so pagination doesn't get stuck emitting continuation pages.
                  pending.offset = pending.total_extent;
                  pending_footnotes.pop_front();
                  continue;
                }
              };

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
                remaining_for_bodies,
                root_axes,
              )? {
                let is_first_fragment = pending.offset <= EPSILON;
                let is_last_fragment = next >= pending.total_extent - EPSILON;
                let break_before_forced =
                  !is_first_fragment && pending.analyzer.is_forced_break_at(pending.offset);
                let break_after_forced =
                  !is_last_fragment && pending.analyzer.is_forced_break_at(next);
                normalize_fragment_margins(
                  &mut slice,
                  is_first_fragment,
                  is_last_fragment,
                  break_before_forced,
                  break_after_forced,
                  &axis,
                );
                let slice_block = axis.block_size(&slice.bounds).max(0.0);
                if slice_block > EPSILON {
                  remaining_for_bodies -= slice_block;
                  footnote_slices.push(slice);
                }
              }

              pending.offset = next;
              if pending.offset >= pending.total_extent - EPSILON {
                pending_footnotes.pop_front();
              }
            }
          }

          let mut defer_remaining = !pending_footnotes.is_empty();

          // If a deferred footnote body is still pending, new footnotes may not overtake it.
          // Enqueue all footnote bodies captured on this page for placement on later pages.
          if defer_remaining {
            for occ in page_footnotes.iter() {
              let mut snapshot = occ.snapshot.clone();
              let offset = Point::new(-snapshot.bounds.x(), -snapshot.bounds.y());
              snapshot.translate_root_in_place(offset);
              enqueue_pending(&mut pending_footnotes, snapshot, root_axes, page_block);
            }
          } else {
            for occ in page_footnotes.iter() {
              let mut snapshot = occ.snapshot.clone();
              let offset = Point::new(-snapshot.bounds.x(), -snapshot.bounds.y());
              snapshot.translate_root_in_place(offset);

              if defer_remaining {
                enqueue_pending(&mut pending_footnotes, snapshot, root_axes, page_block);
                continue;
              }

              let body_block = axis.block_size(&snapshot.bounds).max(0.0);
              let is_oversize =
                body_block + footnote_overhead_block > footnote_max_block_for_in_flow + EPSILON;
              if is_oversize {
                // Avoid mixing additional footnotes onto the same page once an oversize footnote has
                // started fragmenting; later footnotes should not overtake its continuation.
                if remaining_for_bodies <= EPSILON {
                  // No space left for the oversize footnote on this page; defer the body.
                  enqueue_pending(&mut pending_footnotes, snapshot, root_axes, page_block);
                  defer_remaining = true;
                  continue;
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

                let next_candidate = pending.analyzer.next_boundary_with_cursor(
                  pending.offset,
                  remaining_for_bodies,
                  pending.total_extent,
                  &mut pending.opportunity_cursor,
                )?;
                let next = match resolve_pending_footnote_slice_boundary(
                  pending.offset,
                  remaining_for_bodies,
                  pending.total_extent,
                  next_candidate,
                ) {
                  PendingFootnoteSliceBoundary::Advance(next) => next,
                  PendingFootnoteSliceBoundary::Complete => {
                    // If we can't advance (e.g. the footnote area is too small to hold even a single
                    // slice), do not enqueue a pending footnote with an unchanged offset (which can
                    // stall pagination).
                    defer_remaining = true;
                    continue;
                  }
                };

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
                  remaining_for_bodies,
                  root_axes,
                )? {
                  let is_first_fragment = pending.offset <= EPSILON;
                  let is_last_fragment = next >= pending.total_extent - EPSILON;
                  let break_before_forced =
                    !is_first_fragment && pending.analyzer.is_forced_break_at(pending.offset);
                  let break_after_forced =
                    !is_last_fragment && pending.analyzer.is_forced_break_at(next);
                  normalize_fragment_margins(
                    &mut slice,
                    is_first_fragment,
                    is_last_fragment,
                    break_before_forced,
                    break_after_forced,
                    &axis,
                  );
                  let slice_block = axis.block_size(&slice.bounds).max(0.0);
                  if slice_block > EPSILON {
                    remaining_for_bodies -= slice_block;
                    footnote_slices.push(slice);
                  }
                }

                pending.offset = next;
                if pending.offset < pending.total_extent - EPSILON {
                  pending_footnotes.push_back(pending);
                  defer_remaining = true;
                }
                continue;
              }

              if body_block <= remaining_for_bodies + EPSILON {
                footnote_slices.push(snapshot);
                remaining_for_bodies -= body_block;
                continue;
              }

              if occ.policy == FootnotePolicy::Auto {
                // `footnote-policy: auto` keeps the call on the current page; defer only the body.
                enqueue_pending(&mut pending_footnotes, snapshot, root_axes, page_block);
                defer_remaining = true;
                continue;
              }

              // Fallback: treat as deferred so the body doesn't overflow the available footnote area.
              enqueue_pending(&mut pending_footnotes, snapshot, root_axes, page_block);
              defer_remaining = true;
            }
          }

          let footnote_area =
            build_footnote_area_fragment(&page_style, &axis, content_offset, &footnote_slices);

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
          translate_fragment(&mut content, content_offset.x, content_offset.y);
          page_running_elements =
            running_elements_for_page_fragment(&content, root_axes, &mut running_element_state);
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
        let end_for_start_keyword = end;

        let mut token_pos = end;
        let mut containing_line = None;
        line_start_containing_pos(
          &layout.root,
          end,
          0.0,
          root_block_size,
          &axis,
          &mut containing_line,
          false,
        );
        if let Some(line_start) = containing_line {
          // If the page boundary falls inside a line box, prefer moving the entire line to the next
          // page rather than splitting it.
          //
          // However, when the line itself starts at (or before) the current page start (e.g. an
          // oversized line that overflows the fragmentainer), snapping back to the line start would
          // produce an empty page and stall pagination. Only snap when it advances beyond the
          // current page start.
          if line_start > start + EPSILON {
            token_pos = line_start;
          }
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
          Some((_next_start, tok)) => continuation.clone().unwrap_or(tok),
          None => continuation.clone().unwrap_or(BreakToken::End),
        };

        // Ensure the continuation token actually advances pagination. If it doesn't, fall back to a
        // geometry-based continuation at the page boundary so we never enter an infinite pagination
        // loop (which would otherwise eventually OOM).
        //
        // Break tokens that refer to blocks can be ambiguous when a box is split into multiple
        // fragments (e.g. multi-column layout): the token can resolve to the *first* fragment of the
        // box when the boundary is actually at a later fragment start, causing pagination to move
        // backwards and duplicate content. Detect that case and fall back to a continuation token at
        // the boundary.
        if !matches!(next_token, BreakToken::End) {
          let mut next_start =
            flow_start_for_token_in_layout(&layout.root, &next_token, 0.0, root_block_size, &axis)
              .unwrap_or(total_height);
          if next_start + EPSILON < token_pos {
            next_token = continuation.clone().unwrap_or(BreakToken::End);
            next_start = flow_start_for_token_in_layout(
              &layout.root,
              &next_token,
              0.0,
              root_block_size,
              &axis,
            )
            .unwrap_or(total_height);
          }
          if next_start <= start + EPSILON {
            next_token = continuation_token_for_pos(&layout.root, end, 0.0, root_block_size, &axis)
              .unwrap_or(BreakToken::End);
          }
        }
        let next_start = if matches!(next_token, BreakToken::End) {
          total_height
        } else {
          flow_start_for_token_in_layout(
            &layout.root,
            &next_token,
            0.0,
            root_block_size,
            &axis,
          )
          .unwrap_or(total_height)
        };
        string_slice_end = next_start;
        next_string_start_keyword_token = if next_start + EPSILON < end_for_start_keyword {
          continuation_token_for_pos(
            &layout.root,
            end_for_start_keyword,
            0.0,
            root_block_size,
            &axis,
          )
          .unwrap_or_else(|| next_token.clone())
        } else {
          next_token.clone()
        };
      }
    }

    for mut fixed in fixed_fragments {
      translate_fragment(&mut fixed, content_offset.x, content_offset.y);
      document_wrapper.children_mut().push(fixed);
    }

    page_root.children_mut().push(document_wrapper);
    page_root
      .children_mut()
      .extend(build_page_mark_fragments(&page_style));

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
        page_string_start_keyword_pos,
      )
    };

    if is_blank_page {
      // Blank pages still participate in margin box running element resolution by carrying the last
      // running element seen so far.
      page_running_elements =
        snapshot_running_elements_for_non_content_page(&mut running_element_state);
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
      string_start_keyword_token = next_string_start_keyword_token;
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
  start_keyword_pos: f32,
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
        if (event.abs_block - start_keyword_pos).abs() < EPSILON {
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
  /// Flow position (relative to the clipped slice start) where the footnote reference line starts.
  ///
  /// Used to determine whether the footnote can fit above the reserved footnote area on the same
  /// page.
  call_pos: f32,
  /// Flow position (relative to the clipped slice start) where pagination should break when this
  /// footnote cannot be placed on the current page.
  ///
  /// For `footnote-policy: line`, this matches `call_pos`. For `footnote-policy: block`, it is the
  /// start edge of the containing paragraph/block.
  defer_pos: f32,
  snapshot: FragmentNode,
  policy: FootnotePolicy,
}

#[derive(Debug)]
struct PendingFootnote {
  root: FragmentNode,
  analyzer: FragmentationAnalyzer,
  opportunity_cursor: usize,
  offset: f32,
  total_extent: f32,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum PendingFootnoteSliceBoundary {
  /// Continue slicing the pending footnote body up to the returned boundary.
  ///
  /// Guaranteed to be finite and advance beyond `offset` by at least `EPSILON`.
  Advance(f32),
  /// Give up on this footnote slice and treat the pending footnote as complete so pagination can
  /// make forward progress.
  Complete,
}

fn resolve_pending_footnote_slice_boundary(
  offset: f32,
  remaining: f32,
  total_extent: f32,
  candidate: f32,
) -> PendingFootnoteSliceBoundary {
  if !total_extent.is_finite() {
    return PendingFootnoteSliceBoundary::Complete;
  }

  let mut next = candidate;
  if !next.is_finite() || next <= offset + EPSILON {
    next = (offset + remaining).min(total_extent);
  }

  if !next.is_finite() || next <= offset + EPSILON {
    return PendingFootnoteSliceBoundary::Complete;
  }

  if next > total_extent {
    next = total_extent;
  }
  if next <= offset + EPSILON {
    return PendingFootnoteSliceBoundary::Complete;
  }

  PendingFootnoteSliceBoundary::Advance(next)
}

fn collect_footnotes_for_page(
  root: &FragmentNode,
  axis: &crate::layout::fragmentation::FragmentAxis,
) -> Vec<FootnoteOccurrence> {
  let mut occurrences: Vec<FootnoteOccurrence> = Vec::new();
  let root_block_size = axis.block_size(&root.bounds);
  collect_footnote_occurrences(
    root,
    0.0,
    root_block_size,
    axis,
    None,
    None,
    &mut occurrences,
  );
  occurrences
}

fn collect_footnote_occurrences(
  node: &FragmentNode,
  abs_flow_start: f32,
  parent_block_size: f32,
  axis: &crate::layout::fragmentation::FragmentAxis,
  current_line_start: Option<f32>,
  current_paragraph_start: Option<f32>,
  out: &mut Vec<FootnoteOccurrence>,
) {
  let node_block_size = axis.block_size(&node.bounds);
  let abs_block = axis
    .flow_range(abs_flow_start, parent_block_size, &node.bounds)
    .0;
  let current_line_start = if matches!(node.content, FragmentContent::Line { .. }) {
    Some(abs_block)
  } else {
    current_line_start
  };

  // Track the start of the nearest block that establishes an inline formatting context (i.e. a
  // block whose direct children include line boxes). This approximates the "containing paragraph"
  // defined by CSS GCPM's `footnote-policy: block`.
  let current_paragraph_start = if matches!(node.content, FragmentContent::Block { .. })
    && node
      .children
      .iter()
      .any(|child| matches!(child.content, FragmentContent::Line { .. }))
  {
    Some(abs_block)
  } else {
    current_paragraph_start
  };

  if let FragmentContent::FootnoteAnchor { snapshot, policy } = &node.content {
    let call_pos = current_line_start.unwrap_or(abs_block);
    let defer_pos = match policy {
      FootnotePolicy::Block => current_paragraph_start
        .filter(|pos| *pos > EPSILON)
        .unwrap_or(call_pos),
      _ => call_pos,
    };
    out.push(FootnoteOccurrence {
      call_pos,
      defer_pos,
      snapshot: (**snapshot).clone(),
      policy: *policy,
    });
  }

  for child in node.children.iter() {
    collect_footnote_occurrences(
      child,
      abs_block,
      node_block_size,
      axis,
      current_line_start,
      current_paragraph_start,
      out,
    );
  }
}

fn adjust_end_for_footnotes(
  start: f32,
  end_candidate: f32,
  page_block: f32,
  footnote_area_max_block: f32,
  footnote_overhead_block: f32,
  pending_overhead: bool,
  footnotes: &[FootnoteOccurrence],
  axis: &crate::layout::fragmentation::FragmentAxis,
) -> f32 {
  let footnotes: Vec<&FootnoteOccurrence> = footnotes
    .iter()
    .filter(|occ| occ.policy != FootnotePolicy::Auto)
    .collect();
  if footnotes.is_empty() {
    if pending_overhead {
      let main_block = (page_block - footnote_overhead_block).max(0.0);
      return (start + main_block).min(end_candidate);
    }
    return end_candidate;
  }

  let mut included = 0usize;
  let mut total_footnote_block = 0.0f32;
  for occ in footnotes.iter() {
    let body_block = axis.block_size(&occ.snapshot.bounds).max(0.0);
    let next_total = total_footnote_block + body_block;
    let next_footnote_block = footnote_overhead_block + next_total;
    let main_block = page_block - next_footnote_block;
    if next_footnote_block <= footnote_area_max_block && occ.call_pos < main_block {
      included += 1;
      total_footnote_block = next_total;
      continue;
    }
    break;
  }

  let end = if included == 0 {
    let body_block = axis.block_size(&footnotes[0].snapshot.bounds).max(0.0);
    let needed = footnote_overhead_block + body_block;
    let footnote_overflows_page = needed > footnote_area_max_block + EPSILON;
    let footnote_consumes_page = needed >= footnote_area_max_block - EPSILON;
    if (footnote_overflows_page || footnote_consumes_page) && footnotes[0].defer_pos <= EPSILON {
      // If the call is already at the start of the page and the footnote would otherwise consume
      // the entire page, reserve some space so the footnote body can start and continue on
      // subsequent pages.
      let desired_main_block = if footnote_area_max_block + EPSILON < page_block {
        // When `@footnote { max-height }` is smaller than the page, reserve the full cap for the
        // footnote area so the body can progress without creating a mostly-empty page.
        (page_block - footnote_area_max_block).max(0.0)
      } else {
        // Without an explicit max-height, use a heuristic that avoids creating a page containing
        // only footnotes while still ensuring the oversized footnote can start fragmenting.
        let min_footnote_content = 1.0;
        let max_main_block = (page_block - footnote_overhead_block - min_footnote_content).max(0.0);
        (page_block * 0.5).min(max_main_block)
      };
      let mut end = start + desired_main_block;
      // If there are additional footnote calls in this clipped slice, stop before the next call so
      // later footnotes are deferred until this overflowing footnote finishes. (When calls share
      // the same line box, we can't split them here.)
      if footnotes.len() > 1 && footnotes[1].call_pos > EPSILON {
        end = end.min(start + footnotes[1].call_pos);
      }
      end
    } else {
      // No footnote calls fit alongside their bodies; defer the first call to the next page.
      start + footnotes[0].defer_pos
    }
  } else {
    let footnote_block = footnote_overhead_block + total_footnote_block;
    let main_block = (page_block - footnote_block).max(0.0);
    let mut end = start + main_block;
    if included < footnotes.len() {
      end = end.min(start + footnotes[included].defer_pos);
    }
    end
  }
  .min(end_candidate);

  end
}

fn build_footnote_area_fragment(
  page_style: &ResolvedPageStyle,
  axis: &crate::layout::fragmentation::FragmentAxis,
  content_origin: Point,
  slices: &[FragmentNode],
) -> Option<FragmentNode> {
  if slices.is_empty() {
    return None;
  }

  let footnote_style = &page_style.footnote_style;
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

  // Resolve the @footnote box model against the page content box. The resulting fragment is
  // synthetic, so pagination is responsible for applying padding/border and stacking the separator
  // + bodies inside the footnote area's content box.
  let viewport = page_style.total_size;
  let cb_width = page_style.content_size.width.max(0.0);
  let cb_height = page_style.content_size.height.max(0.0);
  let resolve_x = |len: Length| resolve_len(footnote_style, len, Some(cb_width), viewport).max(0.0);
  let resolve_y =
    |len: Length| resolve_len(footnote_style, len, Some(cb_height), viewport).max(0.0);

  let padding_left = resolve_x(footnote_style.padding_left);
  let padding_right = resolve_x(footnote_style.padding_right);
  let padding_top = resolve_y(footnote_style.padding_top);
  let padding_bottom = resolve_y(footnote_style.padding_bottom);
  let border_left = resolve_x(footnote_style.used_border_left_width());
  let border_right = resolve_x(footnote_style.used_border_right_width());
  let border_top = resolve_y(footnote_style.used_border_top_width());
  let border_bottom = resolve_y(footnote_style.used_border_bottom_width());

  let content_offset = Point::new(border_left + padding_left, border_top + padding_top);
  let inline_edges = if axis.block_is_horizontal {
    border_top + padding_top + border_bottom + padding_bottom
  } else {
    border_left + padding_left + border_right + padding_right
  };
  let block_edges = if axis.block_is_horizontal {
    border_left + padding_left + border_right + padding_right
  } else {
    border_top + padding_top + border_bottom + padding_bottom
  };
  let content_inline = (page_inline - inline_edges).max(0.0);

  let flow_box_start_to_physical = |flow_offset: f32, block_size: f32, parent_block_size: f32| {
    if axis.block_positive {
      flow_offset
    } else {
      parent_block_size - flow_offset - block_size
    }
  };

  #[derive(Debug)]
  struct FootnotePlacement {
    snapshot: FragmentNode,
    block_size: f32,
    inline_size: f32,
    display: FootnoteDisplay,
    flow_block_start: f32,
    flow_inline_start: f32,
  }

  let mut placements: Vec<FootnotePlacement> = Vec::with_capacity(slices.len());
  for occ in slices {
    let mut snapshot = occ.clone();
    let offset = Point::new(-snapshot.bounds.x(), -snapshot.bounds.y());
    snapshot.translate_root_in_place(offset);

    let display = snapshot
      .style
      .as_deref()
      .map(|style| style.footnote_display)
      .unwrap_or(FootnoteDisplay::Block);
    let block_size = axis.block_size(&snapshot.bounds).max(0.0);
    let inline_size = axis.inline_size(&snapshot.bounds).max(0.0);

    placements.push(FootnotePlacement {
      snapshot,
      block_size,
      inline_size,
      display,
      flow_block_start: 0.0,
      flow_inline_start: 0.0,
    });
  }

  // Use the footnote area's *content* inline size for wrapping decisions so `@footnote` padding and
  // border reduce the available width for inline footnotes.
  let available_inline = if content_inline.is_finite() {
    content_inline
  } else {
    page_inline
  };

  // Lay out footnote bodies in insertion order.
  //
  // `footnote-display` applies per footnote element; for now we model it as a simple packing
  // algorithm:
  // - `block`: each footnote starts on a new line and stacks along the block axis.
  // - `inline`: footnotes are treated as atomic inline-level boxes that can share a line.
  // - `compact`: currently treated like `inline`, which matches the spec requirement that if two
  //   or more footnotes fit on the same line, they should be placed inline.
  let inline_positive = inline_axis_positive(
    page_style.page_style.writing_mode,
    page_style.page_style.direction,
  );
  let inline_box_start_to_physical =
    |flow_offset: f32, inline_size: f32, parent_inline_size: f32| {
      if inline_positive {
        flow_offset
      } else {
        parent_inline_size - flow_offset - inline_size
      }
    };

  let mut flow_block_cursor = separator_block;
  let mut line_inline_cursor = 0.0f32;
  let mut line_block_size = 0.0f32;
  let mut line_has_items = false;

  for placement in &mut placements {
    let is_inline = matches!(
      placement.display,
      FootnoteDisplay::Inline | FootnoteDisplay::Compact
    );

    if !is_inline {
      // Block footnotes always start on a new line.
      if line_has_items {
        flow_block_cursor += line_block_size;
        line_inline_cursor = 0.0;
        line_block_size = 0.0;
        line_has_items = false;
      }

      placement.flow_block_start = flow_block_cursor;
      placement.flow_inline_start = 0.0;
      flow_block_cursor += placement.block_size;
      continue;
    }

    // Inline/compact footnotes are packed into lines and wrapped when they exceed the available
    // inline size.
    if line_has_items
      && line_inline_cursor + placement.inline_size > available_inline + EPSILON
    {
      flow_block_cursor += line_block_size;
      line_inline_cursor = 0.0;
      line_block_size = 0.0;
    }

    placement.flow_block_start = flow_block_cursor;
    placement.flow_inline_start = line_inline_cursor;
    line_inline_cursor += placement.inline_size;
    line_block_size = line_block_size.max(placement.block_size);
    line_has_items = true;
  }

  if line_has_items {
    flow_block_cursor += line_block_size;
  }

  let content_block = flow_block_cursor;
  let footnote_block = block_edges + content_block;
  if footnote_block <= EPSILON || content_block <= EPSILON {
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
      content_origin.x + physical_block_start,
      content_origin.y,
      footnote_block,
      page_inline,
    )
  } else {
    Rect::from_xywh(
      content_origin.x,
      content_origin.y + physical_block_start,
      page_inline,
      footnote_block,
    )
  };

  let mut children: Vec<FragmentNode> = Vec::with_capacity(1 + placements.len());

  // Separator fragment.
  let mut separator_style = ComputedStyle::default();
  separator_style.display = Display::Block;
  separator_style.writing_mode = footnote_style.writing_mode;
  separator_style.direction = footnote_style.direction;
  separator_style.color = footnote_style.color;
  separator_style.background_color = footnote_style.color;
  let separator_style = Arc::new(separator_style);

  let separator_flow_offset = 0.0;
  let separator_block_start =
    flow_box_start_to_physical(separator_flow_offset, separator_block, content_block);
  let separator_bounds = if axis.block_is_horizontal {
    Rect::from_xywh(
      content_offset.x + separator_block_start,
      content_offset.y,
      separator_block,
      content_inline,
    )
  } else {
    Rect::from_xywh(
      content_offset.x,
      content_offset.y + separator_block_start,
      content_inline,
      separator_block,
    )
  };
  children.push(FragmentNode::new_block_styled(
    separator_bounds,
    Vec::new(),
    separator_style,
  ));

  // Position footnote bodies using the computed packing offsets.
  for mut placement in placements {
    let body_block_start = flow_box_start_to_physical(
      placement.flow_block_start,
      placement.block_size,
      content_block,
    );
    let body_inline_start = inline_box_start_to_physical(
      placement.flow_inline_start,
      placement.inline_size,
      content_inline,
    );
    let translate = if axis.block_is_horizontal {
      Point::new(content_offset.x + body_block_start, content_offset.y + body_inline_start)
    } else {
      Point::new(content_offset.x + body_inline_start, content_offset.y + body_block_start)
    };
    placement.snapshot.translate_root_in_place(translate);
    children.push(placement.snapshot);
  }

  Some(FragmentNode::new_block_styled(
    bounds,
    children,
    Arc::new(footnote_style.clone()),
  ))
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
  /// Synthetic box tree for the margin box content.
  tree: BoxTree,
  /// Running-element snapshots to substitute into the laid-out fragment tree.
  element_placeholders: HashMap<usize, FragmentNode>,
}

fn substitute_running_element_placeholders(
  root: &mut FragmentNode,
  element_placeholders: &HashMap<usize, FragmentNode>,
  placeholder_style: &Arc<ComputedStyle>,
) {
  if element_placeholders.is_empty() {
    return;
  }

  let mut stack: Vec<*mut FragmentNode> = vec![root as *mut FragmentNode];
  while let Some(node_ptr) = stack.pop() {
    unsafe {
      let node = &mut *node_ptr;
      if let FragmentContent::Replaced {
        box_id: Some(id), ..
      } = &node.content
      {
        // Box IDs are only unique within a single box tree. Running element snapshots originate from
        // the *document* box tree, while margin box content is laid out from a separate synthetic
        // box tree whose IDs start at 1. Guard the substitution by ensuring we only match the
        // placeholder fragments produced from the margin box's own box tree (identified via the
        // exact shared style pointer for that tree).
        if node
          .style
          .as_ref()
          .is_some_and(|style| Arc::ptr_eq(style, placeholder_style))
        {
          if let Some(snapshot) = element_placeholders.get(id) {
            let mut inserted = snapshot.clone();
            inserted.translate_root_in_place(Point::new(node.bounds.x(), node.bounds.y()));
            inserted.fragmentainer_index = node.fragmentainer_index;
            inserted.fragmentainer = node.fragmentainer;
            inserted.slice_info = node.slice_info;
            *node = inserted;
          }
        }
      }

      let children = node.children_mut();
      for child in children.iter_mut().rev() {
        stack.push(child as *mut FragmentNode);
      }
    }
  }
}

fn build_page_mark_fragments(style: &ResolvedPageStyle) -> Vec<FragmentNode> {
  if style.marks.is_none() {
    return Vec::new();
  }

  let bleed = style.bleed;
  if !bleed.is_finite() || bleed <= EPSILON {
    return Vec::new();
  }

  // Fixed (but bleed-clamped) mark length. Clamp to avoid generating marks that spill outside the
  // page's total bounds.
  let mut length = 10.0f32;
  if !length.is_finite() {
    length = 0.0;
  }
  length = length.min(bleed).max(0.0);
  if length <= EPSILON {
    return Vec::new();
  }

  let total = style.total_size;
  let trim = style.trim.max(0.0);
  let trimmed_origin = Point::new(bleed + trim, bleed + trim);
  let trimmed_size = Size::new(
    (style.page_size.width - 2.0 * trim).max(0.0),
    (style.page_size.height - 2.0 * trim).max(0.0),
  );
  let x0 = trimmed_origin.x;
  let y0 = trimmed_origin.y;
  let x1 = x0 + trimmed_size.width;
  let y1 = y0 + trimmed_size.height;

  // Clamp each mark arm to whatever space is available on that side of the trimmed rect.
  let len_left = length.min(x0.max(0.0));
  let len_top = length.min(y0.max(0.0));
  let len_right = length.min((total.width - x1).max(0.0));
  let len_bottom = length.min((total.height - y1).max(0.0));

  let thickness = 1.0f32;

  let mut mark_style = ComputedStyle::default();
  mark_style.display = Display::Block;
  // Use `currentColor` (the page box color) for mark painting.
  mark_style.color = style.page_style.color;
  mark_style.background_color = style.page_style.color;
  let mark_style = Arc::new(mark_style);

  let mut out: Vec<FragmentNode> = Vec::new();

  let mut push_rect = |rect: Rect| {
    let mut min_x = rect.x();
    let mut min_y = rect.y();
    let mut max_x = rect.max_x();
    let mut max_y = rect.max_y();
    if !min_x.is_finite()
      || !min_y.is_finite()
      || !max_x.is_finite()
      || !max_y.is_finite()
      || max_x - min_x <= EPSILON
      || max_y - min_y <= EPSILON
    {
      return;
    }
    if max_x < min_x {
      std::mem::swap(&mut max_x, &mut min_x);
    }
    if max_y < min_y {
      std::mem::swap(&mut max_y, &mut min_y);
    }

    min_x = min_x.max(0.0);
    min_y = min_y.max(0.0);
    max_x = max_x.min(total.width);
    max_y = max_y.min(total.height);
    let w = (max_x - min_x).max(0.0);
    let h = (max_y - min_y).max(0.0);
    if w <= EPSILON || h <= EPSILON {
      return;
    }

    out.push(FragmentNode::new_block_styled(
      Rect::from_xywh(min_x, min_y, w, h),
      Vec::new(),
      Arc::clone(&mark_style),
    ));
  };

  if style.marks.crop {
    // Top-left.
    if len_left > EPSILON {
      push_rect(Rect::from_xywh(x0 - len_left, y0 - thickness, len_left, thickness));
    }
    if len_top > EPSILON {
      push_rect(Rect::from_xywh(x0 - thickness, y0 - len_top, thickness, len_top));
    }

    // Top-right.
    if len_right > EPSILON {
      push_rect(Rect::from_xywh(x1, y0 - thickness, len_right, thickness));
    }
    if len_top > EPSILON {
      push_rect(Rect::from_xywh(x1, y0 - len_top, thickness, len_top));
    }

    // Bottom-left.
    if len_left > EPSILON {
      push_rect(Rect::from_xywh(x0 - len_left, y1, len_left, thickness));
    }
    if len_bottom > EPSILON {
      push_rect(Rect::from_xywh(x0 - thickness, y1, thickness, len_bottom));
    }

    // Bottom-right.
    if len_right > EPSILON {
      push_rect(Rect::from_xywh(x1, y1, len_right, thickness));
    }
    if len_bottom > EPSILON {
      push_rect(Rect::from_xywh(x1, y1, thickness, len_bottom));
    }
  }

  if style.marks.cross {
    let arm = (length / 2.0).max(0.0);
    if arm > EPSILON {
      let cross = |cx: f32, cy: f32, out: &mut dyn FnMut(Rect)| {
        // Horizontal segment.
        out(Rect::from_xywh(cx - arm, cy, 2.0 * arm, thickness));
        // Vertical segment.
        out(Rect::from_xywh(cx, cy - arm, thickness, 2.0 * arm));
      };

      cross(x0 - arm, y0 - arm, &mut push_rect);
      cross(x1 + arm, y0 - arm, &mut push_rect);
      cross(x0 - arm, y1 + arm, &mut push_rect);
      cross(x1 + arm, y1 + arm, &mut push_rect);
    }
  }

  out
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
    let (children, element_snapshots) = build_margin_box_children(
      box_style,
      page_index,
      page_count,
      running_strings,
      running_elements,
      &style_arc,
    );
    let root = BoxNode::new_block(style_arc.clone(), FormattingContextType::Block, children);
    let tree = BoxTree::new(root);
    let element_placeholders = build_running_element_placeholder_map(&tree, element_snapshots);
    let plan = MarginBoxPlan {
      style: style_arc,
      tree,
      element_placeholders,
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

    let config = LayoutConfig::new(Size::new(bounds.width(), bounds.height()));
    let engine = LayoutEngine::with_font_context(config, font_ctx.clone());
    if let Ok(mut tree) = engine.layout_tree(&plan.tree) {
      tree.root.bounds = Rect::from_xywh(0.0, 0.0, bounds.width(), bounds.height());
      tree.root.scroll_overflow = Rect::from_xywh(
        0.0,
        0.0,
        tree.root.scroll_overflow.width().max(bounds.width()),
        tree.root.scroll_overflow.height().max(bounds.height()),
      );
      substitute_running_element_placeholders(
        &mut tree.root,
        &plan.element_placeholders,
        &plan.style,
      );
      translate_fragment(&mut tree.root, bounds.x(), bounds.y());
      tree
        .root
        .force_stacking_context_with_z_index(plan.style.z_index.unwrap_or(0));
      fragments.push(tree.root);
    }
  }

  fragments
}

fn build_running_element_placeholder_map(
  tree: &BoxTree,
  element_snapshots: Vec<FragmentNode>,
) -> HashMap<usize, FragmentNode> {
  if element_snapshots.is_empty() {
    return HashMap::new();
  }
  let mut placeholder_ids = Vec::new();
  for child in tree.root.children.iter() {
    if let crate::tree::box_tree::BoxType::Replaced(replaced) = &child.box_type {
      if matches!(replaced.replaced_type, ReplacedType::Canvas) {
        placeholder_ids.push(child.id);
      }
    }
  }
  debug_assert_eq!(
    placeholder_ids.len(),
    element_snapshots.len(),
    "running element placeholder count mismatch"
  );
  placeholder_ids
    .into_iter()
    .zip(element_snapshots.into_iter())
    .collect()
}

fn build_margin_box_children(
  box_style: &ComputedStyle,
  page_index: usize,
  page_count: usize,
  running_strings: &HashMap<String, RunningStringValues>,
  running_elements: &HashMap<String, RunningElementValues>,
  style: &Arc<ComputedStyle>,
) -> (Vec<BoxNode>, Vec<FragmentNode>) {
  let mut children: Vec<BoxNode> = Vec::new();
  let mut element_snapshots: Vec<FragmentNode> = Vec::new();
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
            if trim_ascii_whitespace(&url.url).is_empty() {
              continue;
            }
            flush_text(&mut text_buf, &mut children, style);
            let srcset = srcset_candidates_for_url_image(url);
            children.push(BoxNode::new_replaced(
              style.clone(),
              ReplacedType::Image {
                src: url.url.clone(),
                alt: None,
                loading: ImageLoadingAttribute::Auto,
                decoding: ImageDecodingAttribute::Auto,
                crossorigin: CrossOriginAttribute::None,
                referrer_policy: None,
                srcset,
                sizes: None,
                picture_sources: Vec::new(),
              },
              None,
              None,
            ));
          }
          ContentItem::Element { .. } => {
            flush_text(&mut text_buf, &mut children, style);
            if let ContentItem::Element { ident, select } = item {
              if let Some(snapshot) = crate::layout::running_elements::select_running_element(
                ident,
                *select,
                running_elements,
              ) {
                let width = snapshot.bounds.width();
                let height = snapshot.bounds.height();
                if width > 0.0 && height > 0.0 && width.is_finite() && height.is_finite() {
                  element_snapshots.push(snapshot);
                  children.push(BoxNode::new_replaced(
                    style.clone(),
                    ReplacedType::Canvas,
                    Some(Size::new(width, height)),
                    None,
                  ));
                }
              }
            }
          }
        }
      }
    }
    ContentValue::None | ContentValue::Normal => {}
  }

  flush_text(&mut text_buf, &mut children, style);
  (children, element_snapshots)
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
    let _axes_hint = set_fragmentainer_axes_hint(Some(root_axes));
    let _offset_hint = set_fragmentainer_block_offset_hint(0.0);
    let _footnote_hint =
      set_footnote_area_inline_size_hint(footnote_area_content_inline_size(style));
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
  let axis_is_inline = physical_axis_is_inline(style.writing_mode, variable_axis);
  let intrinsic = |mode| {
    if axis_is_inline {
      intrinsic_engine.compute_intrinsic_size(&plan.tree.root, mode)
    } else {
      intrinsic_engine.compute_intrinsic_block_size(&plan.tree.root, mode)
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
  let (intrinsic_min, intrinsic_max) = (min.max(0.0), max.max(0.0));

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
  use crate::geometry::Rect;
  use crate::layout::axis::FragmentAxes;
  use crate::style::content::{StringSetAssignment, StringSetValue};
  use crate::style::content::RunningElementSelect;
  use crate::style::display::{Display, FormattingContextType};
  use crate::style::page::PageMarks;
  use crate::style::ComputedStyle;
  use crate::text::font_db::FontDatabase;
  use crate::tree::box_tree::{BoxNode, BoxTree};
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
      marks: PageMarks::default(),
      margin_boxes: BTreeMap::new(),
      footnote_style: ComputedStyle::default(),
      page_style: ComputedStyle::default(),
    };
    let mut style_neg = style.clone();
    style_neg.content_size = Size::new(-0.0, 80.0);

    let key = PageLayoutKey::new(&style, 1, 2);
    let key_neg = PageLayoutKey::new(&style_neg, 1, 2);
    assert_eq!(key, key_neg);
  }

  #[test]
  fn string_set_event_sort_breaks_ties_by_traversal_sequence() {
    let mut a_style = ComputedStyle::default();
    a_style.string_set = vec![StringSetAssignment {
      name: "a".into(),
      value: StringSetValue::Literal("A".into()),
    }];
    let mut b_style = ComputedStyle::default();
    b_style.string_set = vec![StringSetAssignment {
      name: "b".into(),
      value: StringSetValue::Literal("B".into()),
    }];

    let a_box = BoxNode::new_block(Arc::new(a_style), FormattingContextType::Block, vec![]);
    let b_box = BoxNode::new_block(Arc::new(b_style), FormattingContextType::Block, vec![]);
    let root_box = BoxNode::new_block(
      Arc::new(ComputedStyle::default()),
      FormattingContextType::Block,
      vec![a_box, b_box],
    );
    let box_tree = BoxTree::new(root_box);
    let a_id = box_tree.root.children[0].id;
    let b_id = box_tree.root.children[1].id;

    // Both children start at the same block position (0.0) to force a sort tie.
    let a_frag =
      FragmentNode::new_block_with_id(Rect::from_xywh(0.0, 0.0, 10.0, 0.0), a_id, vec![]);
    let b_frag =
      FragmentNode::new_block_with_id(Rect::from_xywh(0.0, 0.0, 10.0, 0.0), b_id, vec![]);
    let root_frag = FragmentNode::new_block_with_id(
      Rect::from_xywh(0.0, 0.0, 10.0, 0.0),
      box_tree.root.id,
      vec![a_frag, b_frag],
    );

    let collector = StringSetEventCollector::new(&box_tree);
    let mut events = collector.collect(&root_frag, FragmentAxes::default());
    assert_eq!(events.len(), 2);
    assert_eq!(events[0].name, "a");
    assert_eq!(events[1].name, "b");
    assert!(events[0].sequence < events[1].sequence);

    // Reverse the events to ensure the sort tie-break relies on `sequence`, not input order or sort
    // stability.
    events.reverse();
    sort_string_set_events(&mut events);

    assert_eq!(events[0].name, "a");
    assert_eq!(events[1].name, "b");
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
  fn running_elements_carry_to_non_content_pages() {
    let mut state = crate::layout::running_elements::RunningElementState::default();
    let snapshot = FragmentNode::new_text(Rect::from_xywh(0.0, 0.0, 10.0, 10.0), "Header", 0.0);
    state.last.insert("header".to_string(), snapshot.clone());

    let values = snapshot_running_elements_for_non_content_page(&mut state);
    let header = values
      .get("header")
      .expect("expected carried header running element values");
    let start = header
      .start
      .as_ref()
      .expect("expected carried running element snapshot");
    assert_eq!(start.content.text(), Some("Header"));
    assert_eq!(start.bounds.width(), 10.0);
    assert_eq!(start.bounds.height(), 10.0);
    assert!(header.first.is_none());
    assert!(header.last.is_none());

    assert!(
      state.first.is_empty(),
      "non-content page snapshot should not advance running element state"
    );
    assert!(
      state
        .last
        .get("header")
        .is_some_and(|node| node.content.text() == Some("Header")),
      "running element state should be preserved"
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
              // Non-zero bounds so `build_margin_box_children` emits a placeholder replaced box.
              Rect::from_xywh(0.0, 0.0, 1.0, 1.0),
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
        marks: PageMarks::default(),
        margin_boxes,
        footnote_style: ComputedStyle::default(),
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
    box_style.content_value = ContentValue::Items(vec![ContentItem::Url(
      crate::style::types::BackgroundImageUrl::new("\u{00A0}".to_string()),
    )]);
    let style = Arc::new(box_style.clone());
    let running_strings: HashMap<String, RunningStringValues> = HashMap::new();
    let running_elements: HashMap<String, RunningElementValues> = HashMap::new();

    let (children, element_snapshots) = build_margin_box_children(
      &box_style,
      0,
      1,
      &running_strings,
      &running_elements,
      &style,
    );
    assert!(element_snapshots.is_empty());
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
      call_pos: pos,
      defer_pos: pos,
      snapshot: FragmentNode::new_block(Rect::from_xywh(0.0, 0.0, 10.0, block_size), vec![]),
      policy: FootnotePolicy::Line,
    }
  }

  #[test]
  fn adjust_end_for_footnotes_reserves_space_for_included_footnotes() {
    let axis = FragmentAxis {
      block_is_horizontal: false,
      block_positive: true,
    };
    let footnotes = vec![footnote_occurrence(50.0, 10.0)];
    let end = adjust_end_for_footnotes(0.0, 100.0, 100.0, 100.0, 1.0, false, &footnotes, &axis);
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
    let end = adjust_end_for_footnotes(0.0, 100.0, 100.0, 100.0, 1.0, false, &footnotes, &axis);
    assert!(
      (end - 95.0).abs() < 0.01,
      "expected end=95 so the call moves to the next page, got {end}"
    );
  }

  #[test]
  fn pending_footnote_boundary_falls_back_when_candidate_does_not_advance() {
    let res = resolve_pending_footnote_slice_boundary(0.0, 10.0, 100.0, 0.0);
    assert_eq!(res, PendingFootnoteSliceBoundary::Advance(10.0));
  }

  #[test]
  fn pending_footnote_boundary_falls_back_when_candidate_is_nan() {
    let res = resolve_pending_footnote_slice_boundary(0.0, 10.0, 100.0, f32::NAN);
    assert_eq!(res, PendingFootnoteSliceBoundary::Advance(10.0));
  }

  #[test]
  fn pending_footnote_boundary_completes_when_fallback_does_not_advance() {
    let res = resolve_pending_footnote_slice_boundary(5.0, 10.0, 5.0, 5.0);
    assert_eq!(res, PendingFootnoteSliceBoundary::Complete);
  }

  fn fixed_margin_box_plan(width: Option<f32>, height: Option<f32>) -> MarginBoxPlan {
    let mut style = ComputedStyle::default();
    style.display = Display::Block;
    style.width = width.map(Length::px);
    style.height = height.map(Length::px);
    let style = Arc::new(style);
    let root = BoxNode::new_block(Arc::clone(&style), FormattingContextType::Block, vec![]);
    MarginBoxPlan {
      style,
      tree: BoxTree::new(root),
      element_placeholders: HashMap::new(),
    }
  }

  fn assert_rect_approx(rect: Rect, x: f32, y: f32, w: f32, h: f32) {
    let eps = 0.001;
    assert!(
      (rect.x() - x).abs() < eps
        && (rect.y() - y).abs() < eps
        && (rect.width() - w).abs() < eps
        && (rect.height() - h).abs() < eps,
      "expected rect=({x},{y},{w},{h}), got ({},{},{},{})",
      rect.x(),
      rect.y(),
      rect.width(),
      rect.height(),
    );
  }

  #[test]
  fn margin_box_bounds_place_abc_boxes_with_trim_and_bleed_offsets() {
    // Page geometry chosen so all expected coordinates are integers, keeping this test deterministic
    // and easy to audit.
    let style = ResolvedPageStyle {
      page_size: Size::new(200.0, 300.0),
      total_size: Size::new(216.0, 316.0), // page_size + 2*bleed
      content_size: Size::new(170.0, 242.0),
      content_origin: Point::new(22.0, 32.0),
      margin_top: 20.0,
      margin_right: 12.0,
      margin_bottom: 30.0,
      margin_left: 10.0,
      bleed: 8.0,
      trim: 4.0,
      marks: PageMarks::default(),
      margin_boxes: BTreeMap::new(),
      footnote_style: ComputedStyle::default(),
      page_style: ComputedStyle::default(),
    };
    let mut plans: HashMap<PageMarginArea, MarginBoxPlan> = HashMap::new();

    // Corner boxes (fixed size based on the page margins).
    plans.insert(
      PageMarginArea::TopLeftCorner,
      fixed_margin_box_plan(None, None),
    );
    plans.insert(
      PageMarginArea::TopRightCorner,
      fixed_margin_box_plan(None, None),
    );
    plans.insert(
      PageMarginArea::BottomLeftCorner,
      fixed_margin_box_plan(None, None),
    );
    plans.insert(
      PageMarginArea::BottomRightCorner,
      fixed_margin_box_plan(None, None),
    );

    // Horizontal A/B/C (variable width).
    plans.insert(PageMarginArea::TopLeft, fixed_margin_box_plan(Some(30.0), None));
    plans.insert(
      PageMarginArea::TopCenter,
      fixed_margin_box_plan(Some(40.0), None),
    );
    plans.insert(
      PageMarginArea::TopRight,
      fixed_margin_box_plan(Some(50.0), None),
    );
    plans.insert(
      PageMarginArea::BottomLeft,
      fixed_margin_box_plan(Some(20.0), None),
    );
    plans.insert(
      PageMarginArea::BottomCenter,
      fixed_margin_box_plan(Some(30.0), None),
    );
    plans.insert(
      PageMarginArea::BottomRight,
      fixed_margin_box_plan(Some(40.0), None),
    );

    // Vertical A/B/C (variable height).
    plans.insert(PageMarginArea::LeftTop, fixed_margin_box_plan(None, Some(30.0)));
    plans.insert(
      PageMarginArea::LeftMiddle,
      fixed_margin_box_plan(None, Some(40.0)),
    );
    plans.insert(
      PageMarginArea::LeftBottom,
      fixed_margin_box_plan(None, Some(50.0)),
    );
    plans.insert(
      PageMarginArea::RightTop,
      fixed_margin_box_plan(None, Some(20.0)),
    );
    plans.insert(
      PageMarginArea::RightMiddle,
      fixed_margin_box_plan(None, Some(30.0)),
    );
    plans.insert(
      PageMarginArea::RightBottom,
      fixed_margin_box_plan(None, Some(40.0)),
    );

    let intrinsic_engine = LayoutEngine::with_defaults();
    let bounds = compute_margin_box_bounds(&style, &plans, &intrinsic_engine);

    let origin = style.bleed + style.trim;
    let trimmed_width = style.page_size.width - 2.0 * style.trim;
    let trimmed_height = style.page_size.height - 2.0 * style.trim;
    let available_width = trimmed_width - style.margin_left - style.margin_right;
    let available_height = trimmed_height - style.margin_top - style.margin_bottom;

    let right_margin_x = origin + trimmed_width - style.margin_right;
    let bottom_margin_y = origin + trimmed_height - style.margin_bottom;

    // Trim/bleed regression: all bounds are offset by bleed + trim.
    assert_rect_approx(
      *bounds.get(&PageMarginArea::TopLeftCorner).unwrap(),
      origin,
      origin,
      style.margin_left,
      style.margin_top,
    );
    assert_rect_approx(
      *bounds.get(&PageMarginArea::TopRightCorner).unwrap(),
      right_margin_x,
      origin,
      style.margin_right,
      style.margin_top,
    );
    assert_rect_approx(
      *bounds.get(&PageMarginArea::BottomLeftCorner).unwrap(),
      origin,
      bottom_margin_y,
      style.margin_left,
      style.margin_bottom,
    );
    assert_rect_approx(
      *bounds.get(&PageMarginArea::BottomRightCorner).unwrap(),
      right_margin_x,
      bottom_margin_y,
      style.margin_right,
      style.margin_bottom,
    );

    // Top A/B/C placement + centering.
    let cb_x = origin + style.margin_left;
    assert_rect_approx(
      *bounds.get(&PageMarginArea::TopLeft).unwrap(),
      cb_x,
      origin,
      30.0,
      style.margin_top,
    );
    let top_center = *bounds.get(&PageMarginArea::TopCenter).unwrap();
    assert_rect_approx(top_center, cb_x + (available_width - 40.0) / 2.0, origin, 40.0, 20.0);
    assert!(
      (top_center.x() + top_center.width() / 2.0 - (cb_x + available_width / 2.0)).abs() < 0.001,
      "top-center margin box not centered: {:?}",
      top_center
    );
    let top_right = *bounds.get(&PageMarginArea::TopRight).unwrap();
    assert_rect_approx(top_right, cb_x + available_width - 50.0, origin, 50.0, 20.0);
    assert!(
      (top_right.max_x() - (cb_x + available_width)).abs() < 0.001,
      "top-right should end at the right margin edge"
    );

    // Bottom A/B/C placement.
    assert_rect_approx(
      *bounds.get(&PageMarginArea::BottomLeft).unwrap(),
      cb_x,
      bottom_margin_y,
      20.0,
      style.margin_bottom,
    );
    assert_rect_approx(
      *bounds.get(&PageMarginArea::BottomCenter).unwrap(),
      cb_x + (available_width - 30.0) / 2.0,
      bottom_margin_y,
      30.0,
      style.margin_bottom,
    );
    let bottom_right = *bounds.get(&PageMarginArea::BottomRight).unwrap();
    assert_rect_approx(
      bottom_right,
      cb_x + available_width - 40.0,
      bottom_margin_y,
      40.0,
      style.margin_bottom,
    );
    assert!(
      (bottom_right.max_x() - (cb_x + available_width)).abs() < 0.001,
      "bottom-right should end at the right margin edge"
    );

    // Left A/B/C placement + centering.
    let cb_y = origin + style.margin_top;
    assert_rect_approx(
      *bounds.get(&PageMarginArea::LeftTop).unwrap(),
      origin,
      cb_y,
      style.margin_left,
      30.0,
    );
    let left_middle = *bounds.get(&PageMarginArea::LeftMiddle).unwrap();
    assert_rect_approx(
      left_middle,
      origin,
      cb_y + (available_height - 40.0) / 2.0,
      style.margin_left,
      40.0,
    );
    assert!(
      (left_middle.y() + left_middle.height() / 2.0 - (cb_y + available_height / 2.0)).abs()
        < 0.001,
      "left-middle margin box not centered: {:?}",
      left_middle
    );
    let left_bottom = *bounds.get(&PageMarginArea::LeftBottom).unwrap();
    assert_rect_approx(
      left_bottom,
      origin,
      cb_y + available_height - 50.0,
      style.margin_left,
      50.0,
    );
    assert!(
      (left_bottom.max_y() - (cb_y + available_height)).abs() < 0.001,
      "left-bottom should end at the bottom margin edge"
    );

    // Right A/B/C placement.
    assert_rect_approx(
      *bounds.get(&PageMarginArea::RightTop).unwrap(),
      right_margin_x,
      cb_y,
      style.margin_right,
      20.0,
    );
    assert_rect_approx(
      *bounds.get(&PageMarginArea::RightMiddle).unwrap(),
      right_margin_x,
      cb_y + (available_height - 30.0) / 2.0,
      style.margin_right,
      30.0,
    );
    let right_bottom = *bounds.get(&PageMarginArea::RightBottom).unwrap();
    assert_rect_approx(
      right_bottom,
      right_margin_x,
      cb_y + available_height - 40.0,
      style.margin_right,
      40.0,
    );
    assert!(
      (right_bottom.max_y() - (cb_y + available_height)).abs() < 0.001,
      "right-bottom should end at the bottom margin edge"
    );
  }

  #[test]
  fn margin_box_minmax_constraints_clamp_used_outer_sizes_without_overlap() {
    // Simulate a CSS Page 3 A/B/C line where:
    // - A has a min-width larger than its tentative used size.
    // - C has a max-width smaller than its tentative used size.
    // - B is auto-sized using the imaginary AC box so it remains centered and doesn't overlap.
    let a = VariableMarginBox {
      generated: true,
      outer: Some(10.0),
      outer_min: 10.0,
      outer_max: 10.0,
      min_constraint: 40.0,
      max_constraint: f32::INFINITY,
      margin_start: 0.0,
      margin_end: 0.0,
    };
    let b = VariableMarginBox {
      generated: true,
      outer: None,
      outer_min: 0.0,
      outer_max: 0.0,
      min_constraint: 0.0,
      max_constraint: f32::INFINITY,
      margin_start: 0.0,
      margin_end: 0.0,
    };
    let c = VariableMarginBox {
      generated: true,
      outer: Some(30.0),
      outer_min: 30.0,
      outer_max: 30.0,
      min_constraint: 0.0,
      max_constraint: 20.0,
      margin_start: 0.0,
      margin_end: 0.0,
    };

    let available = 100.0;
    let (used_a, used_b, used_c) = compute_used_outer_sizes_with_minmax(a, b, c, available);

    assert!(
      (used_a - 40.0).abs() < 0.001,
      "expected A to clamp up to min-width (40), got {used_a}"
    );
    assert!(
      (used_c - 20.0).abs() < 0.001,
      "expected C to clamp down to max-width (20), got {used_c}"
    );
    assert!(
      (used_b - 20.0).abs() < 0.001,
      "expected B to shrink based on the imaginary AC box, got {used_b}"
    );

    // Verify that the resulting boxes can be positioned using the CSS Page 3 algorithm without
    // overlap: A at start, C at end, B centered.
    let b_start = (available - used_b) / 2.0;
    let b_end = b_start + used_b;
    let a_end = used_a;
    let c_start = available - used_c;
    assert!(
      a_end <= b_start + 0.001,
      "A overlaps B: A ends at {a_end}, B starts at {b_start}"
    );
    assert!(
      b_end <= c_start + 0.001,
      "B overlaps C: B ends at {b_end}, C starts at {c_start}"
    );
  }
}
