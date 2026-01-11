use super::util::{bounding_box_for_color, create_stacking_context_bounds_renderer};

const VIEWPORT_WIDTH: u32 = 200;
const VIEWPORT_HEIGHT: u32 = 120;

fn non_black_pixels(pixmap: &tiny_skia::Pixmap) -> usize {
  pixmap
    .pixels()
    .iter()
    .filter(|px| px.red() != 0 || px.green() != 0 || px.blue() != 0)
    .count()
}

#[test]
fn webkit_text_stroke_renders_when_fill_is_transparent() {
  let baseline_html = r#"
    <style>
      body { margin: 0; background: black; }
      #t {
        position: absolute;
        left: 20px;
        top: 20px;
        font: 64px/1 sans-serif;
        color: transparent;
      }
    </style>
    <div id="t">H</div>
  "#;

  let stroked_html = r#"
    <style>
      body { margin: 0; background: black; }
      #t {
        position: absolute;
        left: 20px;
        top: 20px;
        font: 64px/1 sans-serif;
        color: transparent;
        -webkit-text-stroke: 4px rgb(255, 0, 0);
      }
    </style>
    <div id="t">H</div>
  "#;

  let mut renderer = create_stacking_context_bounds_renderer();
  let baseline = renderer
    .render_html(baseline_html, VIEWPORT_WIDTH, VIEWPORT_HEIGHT)
    .expect("baseline render");
  assert_eq!(
    non_black_pixels(&baseline),
    0,
    "baseline render should remain all-black when text fill is transparent and no stroke is set"
  );

  let stroked = renderer
    .render_html(stroked_html, VIEWPORT_WIDTH, VIEWPORT_HEIGHT)
    .expect("stroked render");
  let stroked_non_black = non_black_pixels(&stroked);
  assert!(
    stroked_non_black > 0,
    "stroke render should contain at least one non-black pixel (got {stroked_non_black})"
  );
}

#[test]
fn webkit_text_stroke_width_is_in_device_pixels() {
  // Regression: text strokes are specified in device pixels. Our glyph outlines are in font units
  // and are scaled to device pixels by `glyph_transform`. tiny-skia applies that transform to the
  // stroke width too, so we must convert CSS px → font units when stroking.
  //
  // Without that conversion, the stroke is scaled down by `font_size/units_per_em` and becomes
  // far too thin (notably netflix.com's "Top 10" ranking numbers).

  let baseline_html = r#"
    <style>
      body { margin: 0; background: black; }
      #t {
        position: absolute;
        left: 20px;
        top: 20px;
        font: 80px/1 sans-serif;
        color: rgb(0, 255, 0);
      }
    </style>
    <div id="t">H</div>
  "#;

  let stroked_html = r#"
    <style>
      body { margin: 0; background: black; }
      #t {
        position: absolute;
        left: 20px;
        top: 20px;
        font: 80px/1 sans-serif;
        color: rgb(0, 255, 0);
        -webkit-text-stroke: 12px rgb(255, 0, 0);
      }
    </style>
    <div id="t">H</div>
  "#;

  let mut renderer = create_stacking_context_bounds_renderer();
  let baseline = renderer
    .render_html(baseline_html, VIEWPORT_WIDTH, VIEWPORT_HEIGHT)
    .expect("baseline render");
  let stroked = renderer
    .render_html(stroked_html, VIEWPORT_WIDTH, VIEWPORT_HEIGHT)
    .expect("stroked render");

  let baseline_bbox = bounding_box_for_color(&baseline, |(r, g, b, a)| {
    a > 0 && (r != 0 || g != 0 || b != 0)
  })
  .expect("baseline should contain non-black pixels");
  let stroked_bbox = bounding_box_for_color(&stroked, |(r, g, b, a)| {
    a > 0 && (r != 0 || g != 0 || b != 0)
  })
  .expect("stroked should contain non-black pixels");

  let (bx0, by0, bx1, by1) = baseline_bbox;
  let (sx0, sy0, sx1, sy1) = stroked_bbox;

  let left_extra = bx0 as i32 - sx0 as i32;
  let top_extra = by0 as i32 - sy0 as i32;
  let right_extra = sx1 as i32 - bx1 as i32;
  let bottom_extra = sy1 as i32 - by1 as i32;

  let min_expected = 4;
  assert!(
    left_extra >= min_expected
      && top_extra >= min_expected
      && right_extra >= min_expected
      && bottom_extra >= min_expected,
    "stroke bbox should extend beyond fill bbox (baseline={baseline_bbox:?}, stroked={stroked_bbox:?}, extras=({left_extra},{top_extra},{right_extra},{bottom_extra}))"
  );
}
