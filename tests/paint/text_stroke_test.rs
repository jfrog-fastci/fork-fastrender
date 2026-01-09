use super::util::create_stacking_context_bounds_renderer;

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

