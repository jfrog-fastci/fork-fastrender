use crate::api::PreparedDocument;
use crate::geometry::{Point, Rect, Size};
use crate::layout::contexts::inline::baseline::compute_line_height_with_metrics_viewport;
use crate::scroll::{
  apply_scroll_chain, build_scroll_chain, build_scroll_chain_with_root_mode, ScrollOptions,
  ScrollSource, ScrollState,
};
use crate::tree::box_tree::{FormControlKind, ReplacedType as BoxReplacedType};
use crate::tree::fragment_tree::{FragmentContent, FragmentTree, HitTestRoot};

pub struct ScrollWheelInput {
  pub delta_x: f32,
  pub delta_y: f32,
}

fn sanitize_delta(delta: f32) -> f32 {
  if delta.is_finite() {
    delta
  } else {
    0.0
  }
}

fn inset_rect(rect: Rect, left: f32, top: f32, right: f32, bottom: f32) -> Rect {
  Rect::from_xywh(
    rect.x() + left,
    rect.y() + top,
    (rect.width() - left - right).max(0.0),
    (rect.height() - top - bottom).max(0.0),
  )
}

fn select_content_rect(
  border_rect: Rect,
  style: &crate::style::ComputedStyle,
  viewport_size: Size,
) -> Rect {
  let base = border_rect.width().max(0.0);
  let viewport = if viewport_size.width.is_finite() && viewport_size.height.is_finite() {
    (viewport_size.width, viewport_size.height)
  } else {
    (base, base)
  };

  let font_size = style.font_size;
  let root_font_size = style.root_font_size;

  // Mirror the painter's `background_rects` logic: border rect -> padding rect -> content rect.
  let border_left = crate::paint::paint_bounds::resolve_length_for_paint(
    &style.used_border_left_width(),
    font_size,
    root_font_size,
    base,
    Some(viewport),
  );
  let border_right = crate::paint::paint_bounds::resolve_length_for_paint(
    &style.used_border_right_width(),
    font_size,
    root_font_size,
    base,
    Some(viewport),
  );
  let border_top = crate::paint::paint_bounds::resolve_length_for_paint(
    &style.used_border_top_width(),
    font_size,
    root_font_size,
    base,
    Some(viewport),
  );
  let border_bottom = crate::paint::paint_bounds::resolve_length_for_paint(
    &style.used_border_bottom_width(),
    font_size,
    root_font_size,
    base,
    Some(viewport),
  );

  let padding_left = crate::paint::paint_bounds::resolve_length_for_paint(
    &style.padding_left,
    font_size,
    root_font_size,
    base,
    Some(viewport),
  );
  let padding_right = crate::paint::paint_bounds::resolve_length_for_paint(
    &style.padding_right,
    font_size,
    root_font_size,
    base,
    Some(viewport),
  );
  let padding_top = crate::paint::paint_bounds::resolve_length_for_paint(
    &style.padding_top,
    font_size,
    root_font_size,
    base,
    Some(viewport),
  );
  let padding_bottom = crate::paint::paint_bounds::resolve_length_for_paint(
    &style.padding_bottom,
    font_size,
    root_font_size,
    base,
    Some(viewport),
  );

  let padding_rect = inset_rect(
    border_rect,
    border_left,
    border_top,
    border_right,
    border_bottom,
  );
  inset_rect(
    padding_rect,
    padding_left,
    padding_top,
    padding_right,
    padding_bottom,
  )
}

pub fn apply_wheel_scroll(
  fragment_tree: &FragmentTree,
  scroll_state: &ScrollState,
  page_point: Point,
  delta: Point,
) -> ScrollState {
  apply_wheel_scroll_at_point(
    fragment_tree,
    scroll_state,
    fragment_tree.viewport_size(),
    page_point,
    ScrollWheelInput {
      delta_x: delta.x,
      delta_y: delta.y,
    },
  )
}

/// Computes the max vertical scroll offset for listbox `<select>` controls.
///
/// Listbox selects are painted from their `SelectControl` model (not from laid-out `<option>`
/// fragments), so layout does not produce a meaningful `scroll_overflow` size. Wheel scrolling
/// needs an approximate scroll range, so we mirror the painter's row/viewport math.
fn select_listbox_max_scroll_y(
  viewport_size: Size,
  fragment: &crate::tree::fragment_tree::FragmentNode,
  style: &crate::style::ComputedStyle,
) -> Option<f32> {
  let FragmentContent::Replaced { replaced_type, .. } = &fragment.content else {
    return None;
  };
  let BoxReplacedType::FormControl(control) = replaced_type else {
    return None;
  };
  let FormControlKind::Select(select) = &control.control else {
    return None;
  };
  if !(select.multiple || select.size > 1) {
    return None;
  }

  // Mirror paint-time `line-height: normal` resolution so wheel scrolling uses the same row
  // geometry as listbox paint/hit-testing.
  //
  // Only resolve full font metrics when needed. (This keeps listbox wheel interaction fast in the
  // common case where `line-height` is numeric/absolute, while still handling `normal` accurately.)
  let metrics = if matches!(style.line_height, crate::style::types::LineHeight::Normal) {
    super::resolve_scaled_metrics_for_interaction(style)
  } else {
    None
  };
  let line_height =
    compute_line_height_with_metrics_viewport(style, metrics.as_ref(), Some(viewport_size), None);
  if line_height <= 0.0 || !line_height.is_finite() {
    return None;
  }

  let border_rect = Rect::from_xywh(0.0, 0.0, fragment.bounds.width(), fragment.bounds.height());
  let content_rect = select_content_rect(border_rect, style, viewport_size);

  // Keep row geometry consistent with painting/hit-testing: when the listbox is explicitly larger
  // than its intrinsic size, stretch rows so exactly `size` rows fill the content rect.
  let mut row_height = line_height;
  let visible_rows = select.size.max(1) as f32;
  let stretched_row_height = content_rect.height().max(0.0) / visible_rows;
  if stretched_row_height.is_finite() && stretched_row_height > row_height {
    row_height = stretched_row_height;
  }

  let total_rows = select.items.len();
  let content_height = row_height * total_rows as f32;
  let viewport_height = content_rect.height().max(0.0);
  if !content_height.is_finite() || !viewport_height.is_finite() {
    return None;
  }

  let max_scroll_y = (content_height - viewport_height).max(0.0);
  max_scroll_y.is_finite().then_some(max_scroll_y)
}

/// Computes the max vertical scroll offset for `<textarea>` controls.
///
/// Like listbox `<select>`, textareas are painted from their `FormControlKind` model rather than
/// real laid-out text fragments, so layout does not currently produce a meaningful scroll overflow
/// size. Wheel scrolling needs an approximate scroll range, so we mirror textarea paint/wrapping
/// heuristics.
fn textarea_max_scroll_y(
  viewport_size: Size,
  fragment: &crate::tree::fragment_tree::FragmentNode,
  style: &crate::style::ComputedStyle,
) -> Option<f32> {
  let FragmentContent::Replaced { replaced_type, .. } = &fragment.content else {
    return None;
  };
  let BoxReplacedType::FormControl(control) = replaced_type else {
    return None;
  };
  let FormControlKind::TextArea { value, .. } = &control.control else {
    return None;
  };

  let metrics = if matches!(style.line_height, crate::style::types::LineHeight::Normal) {
    super::resolve_scaled_metrics_for_interaction(style)
  } else {
    None
  };
  let line_height =
    compute_line_height_with_metrics_viewport(style, metrics.as_ref(), Some(viewport_size), None);
  if line_height <= 0.0 || !line_height.is_finite() {
    return None;
  }

  let border_rect = Rect::from_xywh(0.0, 0.0, fragment.bounds.width(), fragment.bounds.height());
  let content_rect = select_content_rect(border_rect, style, viewport_size);
  let text_rect = inset_rect(content_rect, 2.0, 2.0, 2.0, 2.0);
  let viewport_height = text_rect.height().max(0.0);
  if viewport_height <= 0.0 || !viewport_height.is_finite() {
    return None;
  }

  let chars_per_line = crate::textarea::textarea_chars_per_line(style, text_rect.width());
  let layout = crate::textarea::build_textarea_visual_lines(value, chars_per_line);
  let content_height = layout.lines.len() as f32 * line_height;
  if !content_height.is_finite() {
    return None;
  }

  let max_scroll_y = (content_height - viewport_height).max(0.0);
  max_scroll_y.is_finite().then_some(max_scroll_y)
}

fn patch_form_control_scroll_bounds(
  viewport_size: Size,
  chain: &mut [crate::scroll::ScrollChainState<'_>],
) {
  for state in chain.iter_mut() {
    let Some(style) = state.container.style.as_deref() else {
      continue;
    };

    let max_scroll_y = select_listbox_max_scroll_y(viewport_size, state.container, style)
      .or_else(|| textarea_max_scroll_y(viewport_size, state.container, style));
    if let Some(max_scroll_y) = max_scroll_y {
      if max_scroll_y.is_finite() {
        state.bounds.min_y = 0.0;
        state.bounds.max_y = max_scroll_y;
      }
    }
  }
}

pub fn apply_wheel_scroll_at_point(
  fragment_tree: &FragmentTree,
  scroll_state: &ScrollState,
  viewport_size: Size,
  page_point: Point,
  input: ScrollWheelInput,
) -> ScrollState {
  let sanitize_scroll = |value: Point| {
    Point::new(
      if value.x.is_finite() { value.x } else { 0.0 },
      if value.y.is_finite() { value.y } else { 0.0 },
    )
  };

  let delta = Point::new(sanitize_delta(input.delta_x), sanitize_delta(input.delta_y));

  if delta.x == 0.0 && delta.y == 0.0 {
    return scroll_state.clone();
  }

  let original_viewport = sanitize_scroll(scroll_state.viewport);

  let options = ScrollOptions {
    source: ScrollSource::User,
    simulate_overscroll: false,
    apply_snap: false,
  };

  let mut scrolled_tree = fragment_tree.clone();
  let sanitized_elements = scroll_state
    .elements
    .iter()
    .filter_map(|(&id, &offset)| {
      let offset = sanitize_scroll(offset);
      (offset != Point::ZERO).then_some((id, offset))
    })
    .collect();
  let sanitized_scroll_state = ScrollState::from_parts(original_viewport, sanitized_elements);
  crate::scroll::apply_scroll_offsets(&mut scrolled_tree, &sanitized_scroll_state);
  crate::scroll::apply_viewport_scroll_cancel(&mut scrolled_tree, &sanitized_scroll_state);

  apply_wheel_scroll_at_point_with_hit_test_tree(
    fragment_tree,
    &scrolled_tree,
    scroll_state,
    &sanitized_scroll_state,
    viewport_size,
    page_point,
    delta,
    options,
    sanitize_scroll,
  )
}

/// Like [`apply_wheel_scroll_at_point`], but performs wheel hit testing in the prepared document's
/// paint-time geometry coordinate space (element scroll offsets + sticky offsets).
///
/// This should be preferred by browser UI code that has access to a [`PreparedDocument`], because
/// sticky positioning depends on paint-time geometry. Without applying sticky offsets, wheel events
/// over stuck elements (e.g. `position: sticky` headers) can hit-test the wrong scroll container.
pub fn apply_wheel_scroll_at_point_prepared(
  prepared: &PreparedDocument,
  scroll_state: &ScrollState,
  viewport_size: Size,
  page_point: Point,
  input: ScrollWheelInput,
) -> ScrollState {
  let sanitize_scroll = |value: Point| {
    Point::new(
      if value.x.is_finite() { value.x } else { 0.0 },
      if value.y.is_finite() { value.y } else { 0.0 },
    )
  };

  let delta = Point::new(sanitize_delta(input.delta_x), sanitize_delta(input.delta_y));
  if delta.x == 0.0 && delta.y == 0.0 {
    return scroll_state.clone();
  }

  let original_viewport = sanitize_scroll(scroll_state.viewport);
  let sanitized_elements = scroll_state
    .elements
    .iter()
    .filter_map(|(&id, &offset)| {
      let offset = sanitize_scroll(offset);
      (offset != Point::ZERO).then_some((id, offset))
    })
    .collect();
  let sanitized_scroll_state = ScrollState::from_parts(original_viewport, sanitized_elements);

  let options = ScrollOptions {
    source: ScrollSource::User,
    simulate_overscroll: false,
    apply_snap: false,
  };

  // Mirror the paint pipeline for hit testing: sticky offsets are applied relative to scroll state,
  // then scroll offsets translate scroll container contents.
  let hit_test_tree = prepared.fragment_tree_for_geometry(&sanitized_scroll_state);

  apply_wheel_scroll_at_point_with_hit_test_tree(
    prepared.fragment_tree(),
    &hit_test_tree,
    scroll_state,
    &sanitized_scroll_state,
    viewport_size,
    page_point,
    delta,
    options,
    sanitize_scroll,
  )
}

fn apply_wheel_scroll_at_point_with_hit_test_tree(
  fragment_tree: &FragmentTree,
  hit_test_tree: &FragmentTree,
  scroll_state: &ScrollState,
  sanitized_scroll_state: &ScrollState,
  viewport_size: Size,
  page_point: Point,
  delta: Point,
  options: ScrollOptions,
  sanitize_scroll: impl Copy + Fn(Point) -> Point,
) -> ScrollState {
  let Some((root_kind, path)) = hit_test_tree.hit_test_path(page_point) else {
    let mut next = scroll_state.clone();
    next.viewport = apply_viewport_delta(
      fragment_tree,
      viewport_size,
      sanitize_scroll(next.viewport),
      delta,
      options,
    );
    return next;
  };

  let mut next = scroll_state.clone();

  match root_kind {
    HitTestRoot::Root => {
      let mut chain = build_scroll_chain(&fragment_tree.root, viewport_size, &path);
      if chain.is_empty() {
        next.viewport = apply_viewport_delta(
          fragment_tree,
          viewport_size,
          sanitized_scroll_state.viewport,
          delta,
          options,
        );
        return next;
      }

      let chain_len = chain.len();
      for (idx, state) in chain.iter_mut().enumerate() {
        if idx == chain_len - 1 {
          state.scroll = sanitized_scroll_state.viewport;
        } else if let Some(id) = state.container.box_id() {
          state.scroll = sanitized_scroll_state.element_offset(id);
        }
      }
      patch_form_control_scroll_bounds(viewport_size, &mut chain);

      apply_scroll_chain(&mut chain, delta, options);

      for (idx, state) in chain.iter().enumerate() {
        if idx == chain_len - 1 {
          next.viewport = state.scroll;
        } else if let Some(id) = state.container.box_id() {
          next.elements.insert(id, state.scroll);
        }
      }
    }
    HitTestRoot::Additional(idx) => {
      let Some(root) = fragment_tree.additional_fragments.get(idx) else {
        next.viewport = apply_viewport_delta(
          fragment_tree,
          viewport_size,
          sanitized_scroll_state.viewport,
          delta,
          options,
        );
        return next;
      };

      let mut chain = build_scroll_chain_with_root_mode(root, root.bounds.size, &path, false);

      for state in chain.iter_mut() {
        if let Some(id) = state.container.box_id() {
          state.scroll = sanitized_scroll_state.element_offset(id);
        }
      }
      patch_form_control_scroll_bounds(viewport_size, &mut chain);

      let result = apply_scroll_chain(&mut chain, delta, options);

      for state in chain.iter() {
        if let Some(id) = state.container.box_id() {
          next.elements.insert(id, state.scroll);
        }
      }
      if result.remaining != Point::ZERO {
        next.viewport = apply_viewport_delta(
          fragment_tree,
          viewport_size,
          sanitized_scroll_state.viewport,
          result.remaining,
          options,
        );
      }
    }
  }

  // Keep a canonical representation so "missing" and "zero" element offsets don't create spurious
  // scroll state diffs.
  next.elements.retain(|_, offset| *offset != Point::ZERO);

  next
}

fn apply_viewport_delta(
  fragment_tree: &FragmentTree,
  viewport_size: Size,
  viewport_scroll: Point,
  delta: Point,
  options: ScrollOptions,
) -> Point {
  let mut chain = build_scroll_chain(&fragment_tree.root, viewport_size, &[]);
  if chain.is_empty() {
    return viewport_scroll;
  }

  chain[0].scroll = viewport_scroll;
  apply_scroll_chain(&mut chain, delta, options);
  chain[0].scroll
}
