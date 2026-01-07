//! Object-fit and object-position utilities shared by paint and display list building.
use crate::style::types::ObjectFit;
use crate::style::types::ObjectPosition;
use crate::style::types::PositionComponent;
use crate::style::types::PositionKeyword;

/// CSS initial value for `object-position` (50% 50%).
pub fn default_object_position() -> ObjectPosition {
  ObjectPosition {
    x: PositionComponent::Keyword(PositionKeyword::Center),
    y: PositionComponent::Keyword(PositionKeyword::Center),
  }
}

/// Resolve a single position component (x or y) against the available free space.
pub fn resolve_object_position(
  comp: PositionComponent,
  free: f32,
  font_size: f32,
  root_font_size: f32,
  viewport: Option<(f32, f32)>,
) -> f32 {
  match comp {
    PositionComponent::Keyword(PositionKeyword::Start) => 0.0,
    PositionComponent::Keyword(PositionKeyword::Center) => free * 0.5,
    PositionComponent::Keyword(PositionKeyword::End) => free,
    PositionComponent::Length(len) => {
      crate::paint::paint_bounds::resolve_length_for_paint(
        &len,
        font_size,
        root_font_size,
        free,
        viewport,
      )
    }
    PositionComponent::Percentage(pct) => free * pct,
  }
}

/// Compute the destination rectangle for a replaced element with object-fit/object-position.
///
/// Returns the offset inside the element's box along with the used width/height.
pub fn compute_object_fit(
  fit: ObjectFit,
  position: ObjectPosition,
  box_width: f32,
  box_height: f32,
  image_width: f32,
  image_height: f32,
  font_size: f32,
  root_font_size: f32,
  viewport: Option<(f32, f32)>,
) -> Option<(f32, f32, f32, f32)> {
  if box_width <= 0.0 || box_height <= 0.0 || image_width <= 0.0 || image_height <= 0.0 {
    return None;
  }

  let scale_x = box_width / image_width;
  let scale_y = box_height / image_height;

  let scale = match fit {
    ObjectFit::Fill => (scale_x, scale_y),
    ObjectFit::Contain => {
      let s = scale_x.min(scale_y);
      (s, s)
    }
    ObjectFit::Cover => {
      let s = scale_x.max(scale_y);
      (s, s)
    }
    ObjectFit::None => (1.0, 1.0),
    ObjectFit::ScaleDown => {
      if image_width <= box_width && image_height <= box_height {
        (1.0, 1.0)
      } else {
        let s = scale_x.min(scale_y);
        (s, s)
      }
    }
  };

  let dest_w = image_width * scale.0;
  let dest_h = image_height * scale.1;
  let free_x = box_width - dest_w;
  let free_y = box_height - dest_h;

  let offset_x = resolve_object_position(position.x, free_x, font_size, root_font_size, viewport);
  let offset_y = resolve_object_position(position.y, free_y, font_size, root_font_size, viewport);

  Some((offset_x, offset_y, dest_w, dest_h))
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::style::values::CalcLength;
  use crate::style::values::Length;
  use crate::style::values::LengthUnit;

  #[test]
  fn contain_centers_image() {
    let fit = ObjectFit::Contain;
    let position = ObjectPosition {
      x: PositionComponent::Keyword(PositionKeyword::Center),
      y: PositionComponent::Keyword(PositionKeyword::Center),
    };

    let (offset_x, offset_y, dest_w, dest_h) = compute_object_fit(
      fit,
      position,
      200.0,
      100.0,
      100.0,
      100.0,
      16.0,
      16.0,
      None,
    )
    .expect("fit computed");
    assert_eq!(dest_h, 100.0);
    assert_eq!(dest_w, 100.0);
    assert!((offset_x - 50.0).abs() < 0.01);
    assert!((offset_y - 0.0).abs() < 0.01);
  }

  #[test]
  fn oversized_image_allows_negative_alignment_space() {
    let position = ObjectPosition {
      x: PositionComponent::Keyword(PositionKeyword::Center),
      y: PositionComponent::Keyword(PositionKeyword::Center),
    };

    let (offset_x, offset_y, dest_w, dest_h) = compute_object_fit(
      ObjectFit::None,
      position,
      100.0,
      100.0,
      200.0,
      50.0,
      16.0,
      16.0,
      None,
    )
    .expect("fit computed");
    assert_eq!(dest_w, 200.0);
    assert_eq!(dest_h, 50.0);
    // Alignment space is negative; center should shift left instead of clamping to zero.
    assert!((offset_x + 50.0).abs() < 0.01);
    assert!((offset_y - 25.0).abs() < 0.01);
  }

  #[test]
  fn percentage_position_uses_alignment_space() {
    let position = ObjectPosition {
      x: PositionComponent::Percentage(0.25),
      y: PositionComponent::Percentage(0.75),
    };

    let (offset_x, offset_y, _, _) = compute_object_fit(
      ObjectFit::Contain,
      position,
      120.0,
      60.0,
      60.0,
      60.0,
      16.0,
      16.0,
      None,
    )
    .expect("fit computed");
    // Contain yields dest_w=60,dest_h=60 so free_x=60, free_y=0.
    assert!((offset_x - 15.0).abs() < 0.01);
    assert!((offset_y - 0.0).abs() < 0.01);
  }

  #[test]
  fn font_relative_lengths_use_font_size() {
    let position = ObjectPosition {
      x: PositionComponent::Length(Length::em(2.0)),
      y: PositionComponent::Keyword(PositionKeyword::Start),
    };

    // free_x is 50 (100 - 50). 2em at 20px font size => 40px.
    let (offset_x, offset_y, _, _) = compute_object_fit(
      ObjectFit::None,
      position,
      100.0,
      50.0,
      50.0,
      50.0,
      20.0,
      30.0,
      None,
    )
    .expect("fit computed");
    assert!((offset_x - 40.0).abs() < 0.01);
    assert!((offset_y - 0.0).abs() < 0.01);
  }

  #[test]
  fn rem_lengths_use_root_font_size() {
    let position = ObjectPosition {
      x: PositionComponent::Length(Length::rem(2.0)),
      y: PositionComponent::Keyword(PositionKeyword::Start),
    };

    // free_x is 50 (100 - 50). 2rem at 20px root font size => 40px.
    let (offset_x, offset_y, _, _) = compute_object_fit(
      ObjectFit::None,
      position,
      100.0,
      50.0,
      50.0,
      50.0,
      10.0,
      20.0,
      None,
    )
    .expect("fit computed");
    assert!((offset_x - 40.0).abs() < 0.01);
    assert!((offset_y - 0.0).abs() < 0.01);
  }

  #[test]
  fn calc_rem_lengths_use_root_font_size() {
    let calc = CalcLength::single(LengthUnit::Rem, 2.0)
      .add_scaled(&CalcLength::single(LengthUnit::Px, 1.0), 1.0)
      .expect("calc terms");
    let position = ObjectPosition {
      x: PositionComponent::Length(Length::calc(calc)),
      y: PositionComponent::Keyword(PositionKeyword::Start),
    };

    // free_x is 50 (100 - 50). 2rem at 20px root font size => 40px + 1px => 41px.
    let (offset_x, offset_y, _, _) = compute_object_fit(
      ObjectFit::None,
      position,
      100.0,
      50.0,
      50.0,
      50.0,
      10.0,
      20.0,
      None,
    )
    .expect("fit computed");
    assert!((offset_x - 41.0).abs() < 0.01);
    assert!((offset_y - 0.0).abs() < 0.01);
  }

  #[test]
  fn viewport_units_resolve_when_available() {
    let position = ObjectPosition {
      x: PositionComponent::Length(Length::new(10.0, LengthUnit::Vw)),
      y: PositionComponent::Keyword(PositionKeyword::Start),
    };

    // free_x = 50 (100-50). 10vw with 200px viewport => 20px offset.
    let (offset_x, offset_y, _, _) = compute_object_fit(
      ObjectFit::None,
      position,
      100.0,
      50.0,
      50.0,
      50.0,
      16.0,
      16.0,
      Some((200.0, 100.0)),
    )
    .expect("fit computed");
    assert!((offset_x - 20.0).abs() < 0.01);
    assert!((offset_y - 0.0).abs() < 0.01);
  }

  #[test]
  fn viewport_units_require_viewport() {
    let position = ObjectPosition {
      x: PositionComponent::Length(Length::new(10.0, LengthUnit::Vw)),
      y: PositionComponent::Keyword(PositionKeyword::Start),
    };

    let (offset_x, offset_y, _, _) = compute_object_fit(
      ObjectFit::None,
      position,
      100.0,
      50.0,
      50.0,
      50.0,
      16.0,
      16.0,
      None,
    )
    .expect("fit computed");
    assert_eq!(offset_x, 0.0);
    assert!((offset_y - 0.0).abs() < 0.01);
  }

  #[test]
  fn calc_positions_resolve_with_available_context() {
    // Percentage + absolute calc resolves against the free space.
    let calc = CalcLength::single(LengthUnit::Percent, 50.0)
      .add_scaled(&CalcLength::single(LengthUnit::Px, 10.0), 1.0)
      .expect("calc terms");
    let position = ObjectPosition {
      x: PositionComponent::Length(Length::calc(calc)),
      y: PositionComponent::Keyword(PositionKeyword::Start),
    };

    let (offset_x, _, _, _) = compute_object_fit(
      ObjectFit::None,
      position,
      120.0,
      50.0,
      20.0,
      20.0,
      16.0,
      16.0,
      None,
    )
    .expect("fit computed");
    // free_x = 100 (box 120 - dest_w 20). 50% + 10px => 60px.
    assert!((offset_x - 60.0).abs() < 0.01);

    // Viewport-relative calc resolves when a viewport is provided.
    let viewport_calc = CalcLength::single(LengthUnit::Vw, 10.0);
    let position = ObjectPosition {
      x: PositionComponent::Length(Length::calc(viewport_calc)),
      y: PositionComponent::Keyword(PositionKeyword::Start),
    };
    let (offset_x, _, _, _) = compute_object_fit(
      ObjectFit::None,
      position,
      100.0,
      50.0,
      50.0,
      50.0,
      16.0,
      16.0,
      Some((200.0, 100.0)),
    )
    .expect("fit computed");
    assert!((offset_x - 20.0).abs() < 0.01);
  }

  #[test]
  fn right_offset_subtracts_from_alignment_space() {
    let calc = CalcLength::single(LengthUnit::Percent, 100.0)
      .add_scaled(&CalcLength::single(LengthUnit::Px, 10.0), -1.0)
      .expect("calc terms");
    let position = ObjectPosition {
      x: PositionComponent::Length(Length::calc(calc)),
      y: PositionComponent::Keyword(PositionKeyword::Center),
    };

    let (offset_x, _, _, _) = compute_object_fit(
      ObjectFit::None,
      position,
      100.0,
      50.0,
      50.0,
      50.0,
      16.0,
      16.0,
      None,
    )
    .expect("fit computed");
    // free_x = 50 (box 100 - dest_w 50). right 10px => 50 - 10.
    assert!((offset_x - 40.0).abs() < 0.01);
  }
}
