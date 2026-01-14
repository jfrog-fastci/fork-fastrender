use crate::geometry::{Point, Rect};
use crate::paint::rasterize::fill_rect;
use crate::scroll::ScrollState;
use crate::style::color::Rgba;
use crate::text::caret::selection_segments_for_char_range;
use crate::text::pipeline::ShapedRun;
use crate::tree::fragment_tree::{FragmentContent, FragmentNode, FragmentTree};
use regex::RegexBuilder;
use std::ops::Range;
use std::sync::Arc;

const MAX_FIND_MATCHES: usize = 10_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FindOptions {
  pub case_sensitive: bool,
}

impl Default for FindOptions {
  fn default() -> Self {
    Self {
      case_sensitive: false,
    }
  }
}

#[derive(Debug, Clone)]
pub struct FindMatch {
  pub rects: Vec<Rect>,
  pub bounds: Rect,
  /// Best-effort originating box identifier for the first span of this match.
  ///
  /// When present, this can be used as a stable identifier for scroll anchoring priority
  /// candidates (e.g. the element containing the active find-in-page match).
  pub first_box_id: Option<usize>,
}

#[derive(Debug, Clone)]
struct TextFragment {
  abs_bounds: Rect,
  text: Arc<str>,
  shaped: Option<Arc<Vec<ShapedRun>>>,
  char_boundaries: Vec<usize>,
  box_id: Option<usize>,
}

#[derive(Debug, Clone)]
struct Segment {
  haystack_range: Range<usize>,
  frag_index: usize,
}

#[derive(Debug, Clone, Default)]
pub struct FindIndex {
  haystack: String,
  segments: Vec<Segment>,
  fragments: Vec<TextFragment>,
}

/// Apply find-in-page highlights to an already rendered pixmap.
///
/// FastRender applies highlights as a post-processing overlay (mutating the rendered pixmap). When
/// callers reuse a previous frame's pixmap (e.g. scroll blit + stripe repaint), the reused pixels
/// already contain highlights and must not be highlighted again (or alpha will be double-applied).
///
/// `clip_device_rects` can be used by incremental paint paths to restrict the overlay to only the
/// regions that were repainted. Rectangles are in **device pixels**.
pub fn apply_find_highlight_overlay(
  matches: &[FindMatch],
  active_match_index: Option<usize>,
  scroll_state: &ScrollState,
  viewport_css: (u32, u32),
  device_pixel_ratio: f32,
  pixmap: &mut tiny_skia::Pixmap,
  clip_device_rects: Option<&[Rect]>,
) {
  if matches.is_empty() {
    return;
  }

  let dpr = if device_pixel_ratio.is_finite() && device_pixel_ratio > 0.0 {
    device_pixel_ratio
  } else {
    1.0
  };

  let viewport_w = viewport_css.0 as f32;
  let viewport_h = viewport_css.1 as f32;
  let viewport_css = Rect::from_xywh(0.0, 0.0, viewport_w, viewport_h);
  let viewport_page = Rect::from_xywh(
    scroll_state.viewport.x,
    scroll_state.viewport.y,
    viewport_w,
    viewport_h,
  );

  let highlight = Rgba::new(255, 235, 59, 0.25);
  let highlight_active = Rgba::new(255, 193, 7, 0.35);

  let apply_device_rect = |pixmap: &mut tiny_skia::Pixmap,
                           rect_device: Rect,
                           color: Rgba,
                           clip_device_rects: Option<&[Rect]>| {
    if rect_device.width() <= 0.0 || rect_device.height() <= 0.0 {
      return;
    }
    if let Some(clips) = clip_device_rects {
      for clip in clips {
        if let Some(intersection) = rect_device.intersection(*clip) {
          fill_rect(
            pixmap,
            intersection.x(),
            intersection.y(),
            intersection.width(),
            intersection.height(),
            color,
          );
        }
      }
    } else {
      fill_rect(
        pixmap,
        rect_device.x(),
        rect_device.y(),
        rect_device.width(),
        rect_device.height(),
        color,
      );
    }
  };

  let paint_match = |pixmap: &mut tiny_skia::Pixmap, m: &FindMatch, color: Rgba| {
    if m.rects.is_empty() || m.bounds == Rect::ZERO {
      return;
    }
    if m.bounds.intersection(viewport_page).is_none() {
      return;
    }

    for rect in &m.rects {
      let local = Rect::from_xywh(
        rect.x() - scroll_state.viewport.x,
        rect.y() - scroll_state.viewport.y,
        rect.width(),
        rect.height(),
      );
      let Some(visible) = local.intersection(viewport_css) else {
        continue;
      };
      let device_rect = Rect::from_xywh(
        visible.x() * dpr,
        visible.y() * dpr,
        visible.width() * dpr,
        visible.height() * dpr,
      );
      apply_device_rect(pixmap, device_rect, color, clip_device_rects);
    }
  };

  for (idx, m) in matches.iter().enumerate() {
    if Some(idx) == active_match_index {
      continue;
    }
    paint_match(pixmap, m, highlight);
  }

  let Some(active) = active_match_index else {
    return;
  };
  let Some(m) = matches.get(active) else {
    return;
  };
  paint_match(pixmap, m, highlight_active);
}

impl FindIndex {
  pub fn build(fragment_tree: &FragmentTree) -> Self {
    let mut index = Self::default();
    index.collect_from_root(&fragment_tree.root);
    for root in &fragment_tree.additional_fragments {
      index.collect_from_root(root);
    }
    index
  }

  pub fn find(&self, query: &str, options: FindOptions) -> Vec<FindMatch> {
    if query.is_empty() || self.haystack.is_empty() {
      return Vec::new();
    }

    let mut builder = RegexBuilder::new(&regex::escape(query));
    if !options.case_sensitive {
      builder.case_insensitive(true);
    }

    let Ok(re) = builder.build() else {
      return Vec::new();
    };

    let mut out = Vec::new();
    let mut segment_cursor = 0usize;

    for mat in re.find_iter(&self.haystack) {
      if out.len() >= MAX_FIND_MATCHES {
        break;
      }

      let match_start = mat.start();
      let match_end = mat.end();

      while segment_cursor < self.segments.len()
        && self.segments[segment_cursor].haystack_range.end <= match_start
      {
        segment_cursor += 1;
      }

      let mut rects: Vec<Rect> = Vec::new();
      let mut bounds: Option<Rect> = None;
      let mut first_box_id: Option<usize> = None;

      let mut seg_idx = segment_cursor;
      while seg_idx < self.segments.len() && self.segments[seg_idx].haystack_range.start < match_end {
        let seg = &self.segments[seg_idx];
        let overlap_start = match_start.max(seg.haystack_range.start);
        let overlap_end = match_end.min(seg.haystack_range.end);
        if overlap_start < overlap_end {
          let frag = &self.fragments[seg.frag_index];
          if first_box_id.is_none() {
            first_box_id = frag.box_id;
          }
          let local_start = overlap_start - seg.haystack_range.start;
          let local_end = overlap_end - seg.haystack_range.start;

          let start_char = char_idx_for_byte(&frag.char_boundaries, local_start);
          let end_char = char_idx_for_byte(&frag.char_boundaries, local_end);

          let runs: &[ShapedRun] = frag
            .shaped
            .as_deref()
            .map(|runs| runs.as_slice())
            .unwrap_or(&[]);

          for (x1, x2) in selection_segments_for_char_range(&frag.text, runs, start_char, end_char) {
            let width = x2 - x1;
            if !width.is_finite() || width <= f32::EPSILON {
              continue;
            }
            let rect = Rect::from_xywh(
              frag.abs_bounds.x() + x1,
              frag.abs_bounds.y(),
              width,
              frag.abs_bounds.height(),
            );
            if !rect.width().is_finite()
              || !rect.height().is_finite()
              || rect.width() <= f32::EPSILON
              || rect.height() <= f32::EPSILON
            {
              continue;
            }
            bounds = Some(bounds.map_or(rect, |existing| existing.union(rect)));
            rects.push(rect);
          }
        }
        seg_idx += 1;
      }

      out.push(FindMatch {
        rects,
        bounds: bounds.unwrap_or(Rect::ZERO),
        first_box_id,
      });
    }

    out
  }

  fn collect_from_root(&mut self, root: &FragmentNode) {
    #[derive(Debug, Clone, Copy)]
    enum VisitState {
      Enter,
      Exit,
    }

    #[derive(Debug, Clone, Copy)]
    struct Frame<'a> {
      node: &'a FragmentNode,
      origin: Point,
      state: VisitState,
    }

    let mut stack: Vec<Frame<'_>> = Vec::new();
    stack.push(Frame {
      node: root,
      origin: Point::ZERO,
      state: VisitState::Enter,
    });

    while let Some(frame) = stack.pop() {
      match frame.state {
        VisitState::Enter => {
          if matches!(
            frame.node.content,
            FragmentContent::RunningAnchor { .. } | FragmentContent::FootnoteAnchor { .. }
          ) {
            continue;
          }

          let abs_origin = frame.origin.translate(frame.node.bounds.origin);
          let abs_bounds = Rect::new(abs_origin, frame.node.bounds.size);

          if let FragmentContent::Text { text, shaped, .. } = &frame.node.content {
            let box_id = frame.node.box_id();
            let frag_index = self.fragments.len();
            self.fragments.push(TextFragment {
              abs_bounds,
              text: text.clone(),
              shaped: shaped.clone(),
              char_boundaries: char_boundary_byte_offsets(text),
              box_id,
            });

            let start = self.haystack.len();
            self.haystack.push_str(text);
            let end = self.haystack.len();
            self.segments.push(Segment {
              haystack_range: start..end,
              frag_index,
            });
          }

          stack.push(Frame {
            node: frame.node,
            origin: frame.origin,
            state: VisitState::Exit,
          });

          for child in frame.node.children.iter().rev() {
            stack.push(Frame {
              node: child,
              origin: abs_origin,
              state: VisitState::Enter,
            });
          }
        }
        VisitState::Exit => {
          if matches!(frame.node.content, FragmentContent::Line { .. }) {
            self.haystack.push('\n');
          }
        }
      }
    }
  }
}

fn char_boundary_byte_offsets(text: &str) -> Vec<usize> {
  let mut out: Vec<usize> = text.char_indices().map(|(idx, _)| idx).collect();
  out.push(text.len());
  out
}

fn char_idx_for_byte(boundaries: &[usize], byte_idx: usize) -> usize {
  match boundaries.binary_search(&byte_idx) {
    Ok(idx) => idx,
    Err(idx) => idx,
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::tree::fragment_tree::FragmentNode;

  fn build_tree(lines: Vec<FragmentNode>) -> FragmentTree {
    FragmentTree::new(FragmentNode::new_block(
      Rect::from_xywh(0.0, 0.0, 400.0, 400.0),
      lines,
    ))
  }

  #[test]
  fn find_spans_line_break_only_with_newline_query() {
    let line1 = FragmentNode::new_line(
      Rect::from_xywh(0.0, 0.0, 200.0, 20.0),
      16.0,
      vec![FragmentNode::new_text(
        Rect::from_xywh(0.0, 0.0, 200.0, 20.0),
        "Hello",
        16.0,
      )],
    );
    let line2 = FragmentNode::new_line(
      Rect::from_xywh(0.0, 20.0, 200.0, 20.0),
      16.0,
      vec![FragmentNode::new_text(
        Rect::from_xywh(0.0, 0.0, 200.0, 20.0),
        "world",
        16.0,
      )],
    );

    let tree = build_tree(vec![line1, line2]);
    let index = FindIndex::build(&tree);

    let opts = FindOptions { case_sensitive: true };
    assert_eq!(index.find("Hello world", opts).len(), 0);

    let matches = index.find("Hello\nworld", opts);
    assert_eq!(matches.len(), 1);
    let m0 = &matches[0];
    assert_eq!(m0.rects.len(), 2);
    assert_eq!(m0.rects[0].y(), 0.0);
    assert_eq!(m0.rects[1].y(), 20.0);
    assert!(m0.rects[0].width() > 0.0);
    assert!(m0.rects[1].width() > 0.0);
    assert_eq!(m0.bounds.min_y(), 0.0);
    assert_eq!(m0.bounds.max_y(), 40.0);
  }

  #[test]
  fn find_multiple_occurrences_in_order_and_width_scales() {
    let line = FragmentNode::new_line(
      Rect::from_xywh(0.0, 0.0, 200.0, 20.0),
      16.0,
      vec![FragmentNode::new_text(
        Rect::from_xywh(0.0, 0.0, 200.0, 20.0),
        "foo foo",
        16.0,
      )],
    );

    let tree = build_tree(vec![line]);
    let index = FindIndex::build(&tree);
    let opts = FindOptions { case_sensitive: true };

    let matches = index.find("foo", opts);
    assert_eq!(matches.len(), 2);
    assert_eq!(matches[0].rects.len(), 1);
    assert_eq!(matches[1].rects.len(), 1);
    assert!(matches[0].bounds.x() < matches[1].bounds.x());
    assert_eq!(matches[0].bounds.y(), 0.0);
    assert!(matches[0].bounds.width() > 0.0);

    let matches_long = index.find("foo foo", opts);
    assert_eq!(matches_long.len(), 1);
    assert!(matches_long[0].bounds.width() > matches[0].bounds.width());
  }

  #[test]
  fn find_case_insensitive_ascii() {
    let line = FragmentNode::new_line(
      Rect::from_xywh(0.0, 0.0, 200.0, 20.0),
      16.0,
      vec![FragmentNode::new_text(
        Rect::from_xywh(0.0, 0.0, 200.0, 20.0),
        "Hello hello",
        16.0,
      )],
    );

    let tree = build_tree(vec![line]);
    let index = FindIndex::build(&tree);

    assert_eq!(
      index.find("hello", FindOptions { case_sensitive: true }).len(),
      1
    );
    assert_eq!(
      index
        .find("hello", FindOptions { case_sensitive: false })
        .len(),
      2
    );
  }
}
