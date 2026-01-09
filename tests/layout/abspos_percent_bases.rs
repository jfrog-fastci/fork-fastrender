use fastrender::geometry::{Point, Rect, Size};
use fastrender::layout::contexts::positioned::PositionedLayout;
use fastrender::style::types::BoxSizing;
use fastrender::{
  AbsoluteLayout, AbsoluteLayoutInput, ContainingBlock, EdgeOffsets, LengthOrAuto, Position,
  PositionedStyle,
};

fn default_style() -> PositionedStyle {
  PositionedStyle {
    border_width: EdgeOffsets::ZERO,
    ..Default::default()
  }
}

#[test]
fn percent_insets_resolve_against_zero_sized_containing_block() {
  // Regression test: `PositionedLayout::determine_containing_block` used to treat a `0px` height as
  // "indefinite", which in turn made percentage `top/bottom` resolve to `auto` instead of `0`.
  let layout = PositionedLayout::new();
  let viewport = Size::new(100.0, 100.0);
  let positioned_rect = Rect::from_xywh(0.0, 0.0, 100.0, 0.0);
  let cb = layout.determine_containing_block(Position::Absolute, viewport, Some(positioned_rect), None);

  let abs = AbsoluteLayout::new();
  let mut style = default_style();
  style.position = Position::Absolute;
  style.top = LengthOrAuto::percent(50.0);
  style.height = LengthOrAuto::px(10.0);

  // Make the static position non-zero so `top:auto` would be observable.
  let input = AbsoluteLayoutInput::new(style, Size::new(0.0, 0.0), Point::new(0.0, 10.0));
  let result = abs.layout_absolute(&input, &cb).unwrap();

  assert!(
    (result.position.y - 0.0).abs() < 0.001,
    "expected 50% of a 0px CB height to resolve to 0px (got y={})",
    result.position.y
  );
}

#[test]
fn abspos_percent_sizes_respect_border_box_sizing() {
  let abs = AbsoluteLayout::new();
  let cb = ContainingBlock::viewport(Size::new(200.0, 100.0));

  let mut style = default_style();
  style.position = Position::Absolute;
  style.left = LengthOrAuto::px(0.0);
  style.top = LengthOrAuto::px(0.0);
  style.width = LengthOrAuto::percent(50.0); // 100px border-box
  style.height = LengthOrAuto::percent(50.0); // 50px border-box
  style.box_sizing = BoxSizing::BorderBox;
  style.padding = EdgeOffsets::new(10.0, 10.0, 10.0, 10.0);
  style.border_width = EdgeOffsets::new(5.0, 5.0, 5.0, 5.0);

  let input = AbsoluteLayoutInput::new(style, Size::new(0.0, 0.0), Point::ZERO);
  let result = abs.layout_absolute(&input, &cb).unwrap();

  // Border-box width = 100; horizontal edges = 10+10+5+5 = 30 => content width = 70.
  assert!(
    (result.size.width - 70.0).abs() < 0.001,
    "expected border-box % width to subtract padding+border (got {})",
    result.size.width
  );
  // Border-box height = 50; vertical edges = 30 => content height = 20.
  assert!(
    (result.size.height - 20.0).abs() < 0.001,
    "expected border-box % height to subtract padding+border (got {})",
    result.size.height
  );
}

fn pixel(pixmap: &resvg::tiny_skia::Pixmap, x: u32, y: u32) -> [u8; 4] {
  let idx = (y as usize * pixmap.width() as usize + x as usize) * 4;
  let data = pixmap.data();
  [data[idx], data[idx + 1], data[idx + 2], data[idx + 3]]
}

#[test]
fn logical_inset_percentages_use_correct_axes_in_vertical_writing_mode() {
  std::thread::Builder::new()
    .stack_size(64 * 1024 * 1024)
    .spawn(|| {
      let mut renderer = fastrender::FastRender::new().expect("renderer");
      let html = r#"
        <style>
          body { margin: 0; background: white; }
          .cb {
            position: relative;
            writing-mode: vertical-rl;
            width: 200px;
            height: 100px;
            background: white;
          }
          .abs {
            position: absolute;
            inset-inline-start: 10%;
            inset-block-start: 10%;
            width: 20px;
            height: 20px;
            background: rgb(255, 0, 0);
          }
        </style>
        <div class="cb"><div class="abs"></div></div>
      "#;

      let pixmap = renderer.render_html(html, 220, 120).expect("render");
      // In `writing-mode: vertical-rl`, block-start is physical right (10% of 200 => 20px),
      // and inline-start is physical top (10% of 100 => 10px), so the red square should start at
      // (x=200-20-20=160, y=10).
      assert_eq!(
        pixel(&pixmap, 165, 15),
        [255, 0, 0, 255],
        "expected red square at the vertical-rl logical inset position"
      );
      // Guard against swapping the percentage bases (e.g. using 10% of 100 for the horizontal inset).
      assert_eq!(
        pixel(&pixmap, 185, 15),
        [255, 255, 255, 255],
        "expected pixel outside the red square to remain white"
      );
    })
    .unwrap()
    .join()
    .unwrap();
}

#[test]
fn abspos_static_position_in_inline_flow_respects_float_offset() {
  std::thread::Builder::new()
    .stack_size(64 * 1024 * 1024)
    .spawn(|| {
      let mut renderer = fastrender::FastRender::new().expect("renderer");
      let html = r#"
        <style>
          body { margin: 0; background: white; }
          .cb { position: relative; width: 200px; height: 40px; background: white; }
          .float { float: left; width: 50px; height: 20px; background: rgb(0, 255, 0); }
          .abs { position: absolute; display: inline-block; width: 20px; height: 20px; background: rgb(255, 0, 0); }
        </style>
        <div class="cb">
          <div class="float"></div><span class="abs"></span>
        </div>
      "#;

      let pixmap = renderer.render_html(html, 220, 60).expect("render");
      assert_eq!(
        pixel(&pixmap, 5, 5),
        [0, 255, 0, 255],
        "expected float to paint green at the block start"
      );
      assert_eq!(
        pixel(&pixmap, 55, 5),
        [255, 0, 0, 255],
        "expected abspos static position to start after the float"
      );
    })
    .unwrap()
    .join()
    .unwrap();
}

#[test]
fn abspos_static_position_nested_in_inline_flow_respects_float_offset() {
  // Like `abspos_static_position_in_inline_flow_respects_float_offset`, but with the positioned
  // element nested inside an (otherwise empty) inline box so the static-position anchor is not a
  // direct child of the block formatting context.
  std::thread::Builder::new()
    .stack_size(64 * 1024 * 1024)
    .spawn(|| {
      let mut renderer = fastrender::FastRender::new().expect("renderer");
      let html = r#"
        <style>
          body { margin: 0; background: white; }
          .cb { position: relative; width: 200px; height: 40px; background: white; }
          .float { float: left; width: 50px; height: 20px; background: rgb(0, 255, 0); }
          .abs { position: absolute; display: inline-block; width: 20px; height: 20px; background: rgb(255, 0, 0); }
        </style>
        <div class="cb">
          <div class="float"></div><span class="wrap"><span class="abs"></span></span>
        </div>
      "#;

      let pixmap = renderer.render_html(html, 220, 60).expect("render");
      assert_eq!(
        pixel(&pixmap, 5, 5),
        [0, 255, 0, 255],
        "expected float to paint green at the block start"
      );
      assert_eq!(
        pixel(&pixmap, 55, 5),
        [255, 0, 0, 255],
        "expected abspos static position to start after the float even when nested"
      );
    })
    .unwrap()
    .join()
    .unwrap();
}
