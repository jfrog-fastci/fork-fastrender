use fastrender::style::types::BorderStyle;
use fastrender::style::values::Length;
use fastrender::style::ComputedStyle;

#[test]
fn used_border_width_snaps_common_half_pixel_values_to_whole_pixels() {
  let mut style = ComputedStyle::default();
  style.border_left_style = BorderStyle::Solid;

  style.border_left_width = Length::px(1.5);
  assert_eq!(style.used_border_left_width(), Length::px(1.0));

  style.border_left_width = Length::px(2.5);
  assert_eq!(style.used_border_left_width(), Length::px(2.0));

  // Hairline borders below 1px should remain representable.
  style.border_left_width = Length::px(0.5);
  assert_eq!(style.used_border_left_width(), Length::px(0.5));
}

