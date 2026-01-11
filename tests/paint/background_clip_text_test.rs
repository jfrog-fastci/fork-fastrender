use super::util::{
  bounding_box_for_color, create_stacking_context_bounds_renderer,
  create_stacking_context_bounds_renderer_legacy,
};
use tiny_skia::Pixmap;

const WIDTH: u32 = 200;
const HEIGHT: u32 = 100;

fn rgba_at(pixmap: &Pixmap, x: u32, y: u32) -> (u8, u8, u8, u8) {
  let pixel = pixmap.pixel(x, y).expect("pixel in bounds");
  (pixel.red(), pixel.green(), pixel.blue(), pixel.alpha())
}

fn assert_is_white(rgba: (u8, u8, u8, u8), msg: &str) {
  let (r, g, b, a) = rgba;
  assert!(
    r > 240 && g > 240 && b > 240 && a > 240,
    "{msg}: expected white, got rgba=({r},{g},{b},{a})"
  );
}

fn assert_is_blue(rgba: (u8, u8, u8, u8), msg: &str) {
  let (r, g, b, a) = rgba;
  assert!(
    b > 200 && r < 50 && g < 50 && a > 200,
    "{msg}: expected blue, got rgba=({r},{g},{b},{a})"
  );
}

fn render_both(html: &str, width: u32, height: u32) -> (Pixmap, Pixmap) {
  let mut dl = create_stacking_context_bounds_renderer();
  let dl_pixmap = dl.render_html(html, width, height).expect("render display_list");

  let mut legacy = create_stacking_context_bounds_renderer_legacy();
  let legacy_pixmap = legacy.render_html(html, width, height).expect("render legacy");

  (dl_pixmap, legacy_pixmap)
}

#[test]
fn background_clip_text_clips_background_to_glyph_shapes() {
  let html = format!(
    r#"<!doctype html>
      <style>
        html, body {{ margin: 0; padding: 0; background: white; }}
        #target {{
          width: {WIDTH}px;
          height: {HEIGHT}px;
          background: rgb(255, 0, 0);
          color: transparent;
          font-family: "DejaVu Sans Subset";
          font-size: 32px;
          line-height: 32px;
          background-clip: text;
          -webkit-background-clip: text;
        }}
      </style>
      <div id="target">Hello</div>
    "#
  );

  let (dl, legacy) = render_both(&html, WIDTH, HEIGHT);
  for (backend, pixmap) in [("display_list", dl), ("legacy", legacy)] {
    assert_is_white(
      rgba_at(&pixmap, 10, 90),
      &format!("{backend}: background-clip:text should not paint outside glyph shapes"),
    );

    let bbox = bounding_box_for_color(&pixmap, |(r, g, b, a)| r > 200 && g < 50 && b < 50 && a > 200);
    assert!(
      bbox.is_some(),
      "{backend}: expected some red pixels to be painted inside glyph shapes"
    );
  }
}

#[test]
fn background_clip_text_only_clips_layers_that_request_it() {
  let html = format!(
    r#"<!doctype html>
      <style>
        html, body {{ margin: 0; padding: 0; background: white; }}
        #target {{
          width: {WIDTH}px;
          height: {HEIGHT}px;
          background-image:
            linear-gradient(rgb(255, 0, 0), rgb(255, 0, 0)),
            linear-gradient(rgb(0, 0, 255), rgb(0, 0, 255));
          background-clip: text, border-box;
          -webkit-background-clip: text, border-box;
          color: transparent;
          font-family: "DejaVu Sans Subset";
          font-size: 32px;
          line-height: 32px;
        }}
      </style>
      <div id="target">Hello</div>
    "#
  );

  let (dl, legacy) = render_both(&html, WIDTH, HEIGHT);
  for (backend, pixmap) in [("display_list", dl), ("legacy", legacy)] {
    assert_is_blue(
      rgba_at(&pixmap, 10, 90),
      &format!("{backend}: non-text layer should still paint outside glyph shapes"),
    );

    let red_bbox =
      bounding_box_for_color(&pixmap, |(r, g, b, a)| r > 200 && g < 50 && b < 50 && a > 200);
    assert!(
      red_bbox.is_some(),
      "{backend}: expected some red pixels from the text-clipped top layer"
    );
  }
}
