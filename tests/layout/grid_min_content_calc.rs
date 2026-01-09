use fastrender::layout::constraints::LayoutConstraints;
use fastrender::layout::contexts::grid::GridFormattingContext;
use fastrender::layout::formatting_context::FormattingContext;
use fastrender::style::display::{Display, FormattingContextType};
use fastrender::style::types::GridTrack;
use fastrender::style::values::{CalcLength, Length, LengthUnit};
use fastrender::style::ComputedStyle;
use fastrender::tree::box_tree::BoxNode;
use std::sync::Arc;

fn assert_approx(val: f32, expected: f32, msg: &str) {
  assert!(
    (val - expected).abs() <= 0.5,
    "{msg}: got {val} expected {expected}",
  );
}

#[test]
fn min_content_row_considers_calc_font_relative_heights() {
  // Regression test for grid track sizing when the only definite block-size comes from a
  // `calc()` that includes font-relative units (e.g. `rem`). The MDN CSS `transform` fixture uses:
  //   height: calc(5.625rem + 1px)  // 90px + 1px at 16px root font size
  // and relies on the first `min-content` row track expanding to match that size.

  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Grid;
  container_style.grid_template_columns = vec![GridTrack::Length(Length::px(200.0))];
  container_style.grid_template_rows = vec![GridTrack::MinContent, GridTrack::Auto];

  let root_font_size = 16.0;
  let banner_height_px = 5.625 * root_font_size + 1.0;
  let calc_height = CalcLength::single(LengthUnit::Rem, 5.625)
    .add_scaled(&CalcLength::single(LengthUnit::Px, 1.0), 1.0)
    .expect("calc should fit within MAX_CALC_TERMS");

  let mut banner_style = ComputedStyle::default();
  banner_style.display = Display::Block;
  banner_style.font_size = root_font_size;
  banner_style.root_font_size = root_font_size;
  banner_style.height = Some(Length::calc(calc_height));
  let banner = BoxNode::new_block(Arc::new(banner_style), FormattingContextType::Block, vec![]);

  let mut header_style = ComputedStyle::default();
  header_style.display = Display::Block;
  header_style.height = Some(Length::px(10.0));
  let header = BoxNode::new_block(Arc::new(header_style), FormattingContextType::Block, vec![]);

  let container = BoxNode::new_block(
    Arc::new(container_style),
    FormattingContextType::Grid,
    vec![banner, header],
  );

  let fc = GridFormattingContext::new();
  let fragment = fc
    .layout(&container, &LayoutConstraints::definite(200.0, 400.0))
    .expect("grid layout");

  assert_eq!(fragment.children.len(), 2, "grid should have two item fragments");
  let banner_fragment = &fragment.children[0];
  let header_fragment = &fragment.children[1];

  assert_approx(
    banner_fragment.bounds.height(),
    banner_height_px,
    "banner fragment height should resolve calc(rem + px) against root font size",
  );
  assert_approx(
    header_fragment.bounds.y(),
    banner_height_px,
    "header should be placed after the min-content banner row",
  );
}
