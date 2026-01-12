use super::util::{
  create_stacking_context_bounds_renderer, create_stacking_context_bounds_renderer_legacy,
};
use tiny_skia::Pixmap;

// 1×1 RGBA red PNG.
const RED_PNG_DATA_URL: &str = "data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR4nGP4z8DwHwAFAAH/iZk9HQAAAABJRU5ErkJggg==";

fn pixel_rgb(pixmap: &Pixmap, x: u32, y: u32) -> (u8, u8, u8) {
  let p = pixmap.pixel(x, y).expect("pixel");
  (p.red(), p.green(), p.blue())
}

fn assert_center_pixel_is_red(pixmap: &Pixmap) {
  let (r, g, b) = pixel_rgb(pixmap, pixmap.width() / 2, pixmap.height() / 2);
  assert!(
    r > 200 && g < 50 && b < 50,
    "expected lazy image to be painted (red), got r={r} g={g} b={b}"
  );
}

fn assert_center_pixel_is_green(pixmap: &Pixmap) {
  let (r, g, b) = pixel_rgb(pixmap, pixmap.width() / 2, pixmap.height() / 2);
  assert!(
    g > 200 && r < 50 && b < 50,
    "expected lazy image to be deferred (green background), got r={r} g={g} b={b}"
  );
}

#[test]
fn loading_lazy_images_in_viewport_paints_in_display_list_backend() {
  let mut renderer = create_stacking_context_bounds_renderer();
  let html = format!(
    r#"
      <style>
        body {{ margin: 0; background: rgb(0, 255, 0); }}
        img {{ display: block; width: 40px; height: 40px; }}
      </style>
      <img loading="lazy" src="{RED_PNG_DATA_URL}">
    "#
  );

  let pixmap = renderer.render_html(&html, 40, 40).expect("render");
  assert_center_pixel_is_red(&pixmap);
}

#[test]
fn loading_lazy_images_in_viewport_paints_in_legacy_backend() {
  let mut renderer = create_stacking_context_bounds_renderer_legacy();
  let html = format!(
    r#"
      <style>
        body {{ margin: 0; background: rgb(0, 255, 0); }}
        img {{ display: block; width: 40px; height: 40px; }}
      </style>
      <img loading="lazy" src="{RED_PNG_DATA_URL}">
    "#
  );

  let pixmap = renderer.render_html(&html, 40, 40).expect("render");
  assert_center_pixel_is_red(&pixmap);
}

#[test]
fn loading_lazy_images_below_viewport_does_not_paint_in_display_list_backend() {
  let mut renderer = create_stacking_context_bounds_renderer();
  let html = format!(
    r#"
      <style>
        body {{ margin: 0; background: rgb(0, 255, 0); }}
        img {{ display: block; width: 40px; height: 40px; }}
      </style>
      <div style="height: 2000px"></div>
      <img loading="lazy" src="{RED_PNG_DATA_URL}">
    "#
  );

  let pixmap = renderer.render_html(&html, 40, 40).expect("render");
  assert_center_pixel_is_green(&pixmap);
}

#[test]
fn loading_lazy_images_below_viewport_does_not_paint_in_legacy_backend() {
  let mut renderer = create_stacking_context_bounds_renderer_legacy();
  let html = format!(
    r#"
      <style>
        body {{ margin: 0; background: rgb(0, 255, 0); }}
        img {{ display: block; width: 40px; height: 40px; }}
      </style>
      <div style="height: 2000px"></div>
      <img loading="lazy" src="{RED_PNG_DATA_URL}">
    "#
  );

  let pixmap = renderer.render_html(&html, 40, 40).expect("render");
  assert_center_pixel_is_green(&pixmap);
}
