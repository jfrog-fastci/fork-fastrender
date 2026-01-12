use super::util::create_stacking_context_bounds_renderer;

#[test]
fn single_edge_bottom_border_snaps_like_chrome() {
  // Chrome/Skia paints a `border-bottom: 1px solid` that starts on a fractional device pixel
  // lower than a naïve 1px fill-rect. This prevents 1px "stripe swaps" on text-heavy pages where
  // block positions accumulate fractional line-heights.
  //
  // Repro: place a bottom-only 1px border at a fractional y coordinate so the border's top edge
  // falls between two device pixels.
  let html = r#"
    <style>
      html, body { margin: 0; background: #ff6600; }
      #line {
        position: absolute;
        left: 0;
        top: 0.35px;
        width: 20px;
        height: 0;
        border-bottom: 1px solid #fff;
      }
    </style>
    <div id="line"></div>
  "#;

  let mut renderer = create_stacking_context_bounds_renderer();
  let pixmap = renderer.render_html(html, 30, 10).expect("render");

  let above = pixmap.pixel(5, 0).expect("above pixel");
  assert_eq!(
    (above.red(), above.green(), above.blue(), above.alpha()),
    (255, 102, 0, 255),
    "expected pixel above border to remain background"
  );

  let border = pixmap.pixel(5, 1).expect("border pixel");
  assert_eq!(
    (border.red(), border.green(), border.blue(), border.alpha()),
    (255, 255, 255, 255),
    "expected bottom border to paint the lower device pixel row"
  );
}

#[test]
fn single_edge_bottom_border_snaps_like_chrome_even_width() {
  // Like `single_edge_bottom_border_snaps_like_chrome`, but with a 2px bottom border.
  //
  // Regression: even-width bottom borders can still land on fractional device pixels (e.g. when
  // the border box height is fractional due to subpixel line-heights). Chrome snaps these down to
  // the next device pixel row, while a naïve fill-rect snap paints one row too high.
  let html = r#"
    <style>
      html, body { margin: 0; background: #ff6600; }
      #line {
        position: absolute;
        left: 0;
        top: 0.35px;
        width: 20px;
        height: 0;
        border-bottom: 2px solid #fff;
      }
    </style>
    <div id="line"></div>
  "#;

  let mut renderer = create_stacking_context_bounds_renderer();
  let pixmap = renderer.render_html(html, 30, 10).expect("render");

  let above = pixmap.pixel(5, 0).expect("above pixel");
  assert_eq!(
    (above.red(), above.green(), above.blue(), above.alpha()),
    (255, 102, 0, 255),
    "expected pixel above border to remain background"
  );

  for y in [1u32, 2u32] {
    let border = pixmap.pixel(5, y).expect("border pixel");
    assert_eq!(
      (border.red(), border.green(), border.blue(), border.alpha()),
      (255, 255, 255, 255),
      "expected 2px bottom border to paint device pixel row {y}"
    );
  }

  let below = pixmap.pixel(5, 3).expect("below pixel");
  assert_eq!(
    (below.red(), below.green(), below.blue(), below.alpha()),
    (255, 102, 0, 255),
    "expected pixel below 2px border to remain background"
  );
}

#[test]
fn single_edge_bottom_border_does_not_shift_when_pixel_aligned() {
  // The half-pixel bias used to match Chrome's fractional snapping should not affect borders whose
  // top edge is already aligned to device pixels. Otherwise, a 1px bottom border can be painted one
  // row too low.
  let html = r#"
    <style>
      html, body { margin: 0; background: #ff6600; }
      #line {
        position: absolute;
        left: 0;
        top: 0px;
        width: 20px;
        height: 0;
        border-bottom: 1px solid #fff;
      }
    </style>
    <div id="line"></div>
  "#;

  let mut renderer = create_stacking_context_bounds_renderer();
  let pixmap = renderer.render_html(html, 30, 10).expect("render");

  let border = pixmap.pixel(5, 0).expect("border pixel");
  assert_eq!(
    (border.red(), border.green(), border.blue(), border.alpha()),
    (255, 255, 255, 255),
    "expected bottom border to remain on the first device pixel row"
  );

  let below = pixmap.pixel(5, 1).expect("below pixel");
  assert_eq!(
    (below.red(), below.green(), below.blue(), below.alpha()),
    (255, 102, 0, 255),
    "expected pixel below border to remain background"
  );
}

#[test]
fn single_edge_bottom_border_does_not_shift_when_integer_aligned() {
  // The Chrome/Skia bias applied by `render_border` is only correct when the border's top edge
  // lands on a fractional device pixel. When the edge is already integer-aligned, shifting the
  // border down by half a pixel incorrectly moves it *outside* the border box (swapping a stripe
  // of border/background color).
  let html = r#"
    <style>
      html, body { margin: 0; background: #ff6600; }
      #box {
        position: absolute;
        left: 0;
        top: 0;
        width: 20px;
        height: 51px;
        border-bottom: 1px solid #fff;
      }
    </style>
    <div id="box"></div>
  "#;

  let mut renderer = create_stacking_context_bounds_renderer();
  let pixmap = renderer.render_html(html, 30, 60).expect("render");

  let above = pixmap.pixel(5, 50).expect("above pixel");
  assert_eq!(
    (above.red(), above.green(), above.blue(), above.alpha()),
    (255, 102, 0, 255),
    "expected pixel above border to remain background"
  );

  let border = pixmap.pixel(5, 51).expect("border pixel");
  assert_eq!(
    (border.red(), border.green(), border.blue(), border.alpha()),
    (255, 255, 255, 255),
    "expected bottom border to paint the last row *inside* the border box"
  );

  let below = pixmap.pixel(5, 52).expect("below pixel");
  assert_eq!(
    (below.red(), below.green(), below.blue(), below.alpha()),
    (255, 102, 0, 255),
    "expected pixel below border to remain background"
  );
}
