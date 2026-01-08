use crate::geometry::Point;
use crate::scroll::ScrollState;
use crate::style::types::Overflow;
use crate::tree::fragment_tree::FragmentTree;

pub struct ScrollWheelInput {
  pub delta_x: f32,
  pub delta_y: f32,
}

pub fn apply_wheel_scroll_at_point(
  fragment_tree: &FragmentTree,
  scroll_state: &ScrollState,
  page_point: Point,
  input: ScrollWheelInput,
) -> ScrollState {
  if input.delta_x == 0.0 && input.delta_y == 0.0 {
    return scroll_state.clone();
  }

  for fragment in fragment_tree.hit_test(page_point) {
    let Some(box_id) = fragment.box_id() else {
      continue;
    };
    let Some(style) = fragment.get_style() else {
      continue;
    };

    let scroll_x =
      input.delta_x != 0.0 && matches!(style.overflow_x, Overflow::Auto | Overflow::Scroll);
    let scroll_y =
      input.delta_y != 0.0 && matches!(style.overflow_y, Overflow::Auto | Overflow::Scroll);
    if !scroll_x && !scroll_y {
      continue;
    }

    let viewport = fragment.bounds.size;
    let content = fragment.scroll_overflow.size;
    let max_scroll_x = (content.width - viewport.width).max(0.0);
    let max_scroll_y = (content.height - viewport.height).max(0.0);

    let current = scroll_state.element_offset(box_id);
    let mut next = current;

    if scroll_x {
      let delta = if input.delta_x.is_finite() {
        input.delta_x
      } else {
        0.0
      };
      let value = current.x + delta;
      next.x = if value.is_finite() {
        value.clamp(0.0, max_scroll_x)
      } else {
        current.x
      };
    }

    if scroll_y {
      let delta = if input.delta_y.is_finite() {
        input.delta_y
      } else {
        0.0
      };
      let value = current.y + delta;
      next.y = if value.is_finite() {
        value.clamp(0.0, max_scroll_y)
      } else {
        current.y
      };
    }

    if next == current {
      continue;
    }

    let mut updated = scroll_state.clone();
    updated.elements.insert(box_id, next);
    return updated;
  }

  scroll_state.clone()
}
