use super::util::{create_stacking_context_bounds_renderer, create_stacking_context_bounds_renderer_legacy};
use tiny_skia::Pixmap;

fn rgba_at(pixmap: &Pixmap, x: u32, y: u32) -> (u8, u8, u8, u8) {
  let pixel = pixmap.pixel(x, y).expect("pixel in bounds");
  (pixel.red(), pixel.green(), pixel.blue(), pixel.alpha())
}

fn assert_is_track_gray(rgba: (u8, u8, u8, u8), msg: &str) {
  let (r, g, b, a) = rgba;
  assert!(
    a > 240 && (r as i32 - 200).abs() <= 15 && (g as i32 - 200).abs() <= 15 && (b as i32 - 200).abs() <= 15,
    "{msg}: expected gray track, got rgba=({r},{g},{b},{a})"
  );
}

fn assert_is_green(rgba: (u8, u8, u8, u8), msg: &str) {
  let (r, g, b, a) = rgba;
  assert!(
    a > 240 && g > 220 && r < 80 && b < 80,
    "{msg}: expected green fill, got rgba=({r},{g},{b},{a})"
  );
}

fn render_both(html: &str, width: u32, height: u32) -> (Pixmap, Pixmap) {
  let mut dl = create_stacking_context_bounds_renderer();
  let dl_pixmap = dl.render_html(html, width, height).expect("render display_list");

  let mut legacy = create_stacking_context_bounds_renderer_legacy();
  let legacy_pixmap = legacy
    .render_html(html, width, height)
    .expect("render legacy");

  (dl_pixmap, legacy_pixmap)
}

#[test]
fn range_paints_fill_from_right_in_rtl() {
  let html = r#"
    <!doctype html>
    <style>
      body { margin: 0; background: white; }
      #slider {
        position: absolute;
        left: 0px;
        top: 0px;
        width: 200px;
        height: 20px;
        padding: 0;
        border: 0;
        border-radius: 0;
        accent-color: rgb(0, 255, 0);
        direction: rtl;
      }
      #slider::-webkit-slider-thumb,
      #slider::-moz-range-thumb {
        width: 1px;
        height: 1px;
        border: 0;
        background: transparent;
      }
    </style>
    <input id="slider" type="range" value="25" min="0" max="100" />
  "#;

  let (dl, legacy) = render_both(html, 220, 40);
  for (backend, pixmap) in [("display_list", &dl), ("legacy", &legacy)] {
    assert_is_track_gray(
      rgba_at(pixmap, 10, 10),
      &format!("{backend}: rtl range left sample"),
    );
    assert_is_track_gray(
      rgba_at(pixmap, 100, 10),
      &format!("{backend}: rtl range mid sample"),
    );
    assert_is_green(
      rgba_at(pixmap, 190, 10),
      &format!("{backend}: rtl range right sample"),
    );
  }
}

