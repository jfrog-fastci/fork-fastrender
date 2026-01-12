use fastrender::{Color, Rgba};

// Regression tests for CSS Color 4 math functions inside rgb()/rgba().
//
// Real-world stylesheets (including kotlinlang.org) use patterns like:
//   rgb(calc(25 + var(--coef) * 230), ...)
// to derive theme-aware colors from numeric custom properties.

#[test]
fn rgb_calc_channels_parse() {
  let color = Color::parse("rgb(calc(25 + 1*230), calc(25 + 1*230), calc(28 + 1*227))").unwrap();
  assert_eq!(color.to_rgba(Rgba::BLACK), Rgba::WHITE);

  let color = Color::parse("rgb(calc(25 + 0*230) calc(25 + 0*230) calc(28 + 0*227))").unwrap();
  assert_eq!(color.to_rgba(Rgba::BLACK), Rgba::rgb(25, 25, 28));
}

#[test]
fn rgb_calc_alpha_parse() {
  let color = Color::parse("rgb(255 0 0 / calc(1/2))").unwrap();
  assert_eq!(color.to_rgba(Rgba::BLACK), Rgba::new(255, 0, 0, 0.5));
}

