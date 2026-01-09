use crate::geometry::{Point, Size};
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
  if delta.is_finite() { delta } else { 0.0 }
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

/// Computes the vertical scroll overflow height for listbox `<select>` controls.
///
/// Listbox selects are painted from their `SelectControl` model (not from laid-out `<option>`
/// fragments), so layout does not produce a meaningful `scroll_overflow` size. Wheel scrolling
/// needs an approximate scroll range, so we mirror the painter's `row_height * total_rows` logic.
fn select_listbox_scroll_overflow_height(
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

  let row_height = compute_line_height_with_metrics_viewport(style, None, Some(viewport_size));
  if row_height <= 0.0 || !row_height.is_finite() {
    return None;
  }

  let total_rows = select.items.len();
  let content_height = row_height * total_rows as f32;
  if content_height.is_finite() {
    Some(content_height.max(0.0))
  } else {
    None
  }
}

fn patch_listbox_scroll_bounds(
  viewport_size: Size,
  chain: &mut [crate::scroll::ScrollChainState<'_>],
) {
  for state in chain.iter_mut() {
    let Some(style) = state.container.style.as_deref() else {
      continue;
    };
    let Some(content_height) =
      select_listbox_scroll_overflow_height(viewport_size, state.container, style)
    else {
      continue;
    };
    let viewport_height = state.viewport.height;
    if !viewport_height.is_finite() {
      continue;
    }
    let max_scroll_y = (content_height - viewport_height).max(0.0);
    if max_scroll_y.is_finite() {
      state.bounds.min_y = 0.0;
      state.bounds.max_y = max_scroll_y;
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
  let delta = Point::new(sanitize_delta(input.delta_x), sanitize_delta(input.delta_y));

  if delta.x == 0.0 && delta.y == 0.0 {
    return scroll_state.clone();
  }

  let sanitize_scroll = |value: Point| {
    Point::new(
      if value.x.is_finite() { value.x } else { 0.0 },
      if value.y.is_finite() { value.y } else { 0.0 },
    )
  };

  let original_viewport = sanitize_scroll(scroll_state.viewport);

  let options = ScrollOptions {
    source: ScrollSource::User,
    simulate_overscroll: false,
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

  let Some((root_kind, path)) = scrolled_tree.hit_test_path(page_point) else {
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
        next.viewport =
          apply_viewport_delta(fragment_tree, viewport_size, original_viewport, delta, options);
        return next;
      }

      let chain_len = chain.len();
      for (idx, state) in chain.iter_mut().enumerate() {
        if idx == chain_len - 1 {
          state.scroll = original_viewport;
        } else if let Some(id) = state.container.box_id() {
          state.scroll = sanitize_scroll(scroll_state.element_offset(id));
        }
      }
      patch_listbox_scroll_bounds(viewport_size, &mut chain);

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
        next.viewport =
          apply_viewport_delta(fragment_tree, viewport_size, original_viewport, delta, options);
        return next;
      };

      let mut chain = build_scroll_chain_with_root_mode(root, root.bounds.size, &path, false);

      for state in chain.iter_mut() {
        if let Some(id) = state.container.box_id() {
          state.scroll = sanitize_scroll(scroll_state.element_offset(id));
        }
      }
      patch_listbox_scroll_bounds(viewport_size, &mut chain);

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
          original_viewport,
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
