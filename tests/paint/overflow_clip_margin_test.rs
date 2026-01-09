use super::util::{
  create_stacking_context_bounds_renderer, create_stacking_context_bounds_renderer_legacy,
};
use tiny_skia::Pixmap;

fn rgba_at(pixmap: &Pixmap, x: u32, y: u32) -> [u8; 4] {
  let pixel = pixmap.pixel(x, y).expect("pixel");
  [pixel.red(), pixel.green(), pixel.blue(), pixel.alpha()]
}

fn assert_red(px: [u8; 4]) {
  assert!(
    px[0] > 200 && px[1] < 50 && px[2] < 50 && px[3] > 200,
    "expected red pixel, got {px:?}"
  );
}

fn assert_white(px: [u8; 4]) {
  assert_eq!(px, [255, 255, 255, 255], "expected white pixel, got {px:?}");
}

fn overflow_clip_margin_expands_clip_area(renderer: &mut fastrender::FastRender) {
  let html = r#"
  <style>
    body { margin: 0; background: white; }
    #outer {
      position: absolute;
      left: 10px;
      top: 10px;
      width: 50px;
      height: 50px;
      overflow: clip;
      overflow-clip-margin: 10px;
    }
    #child {
      position: absolute;
      left: 45px;
      top: 45px;
      width: 20px;
      height: 20px;
      background: rgb(255, 0, 0);
    }
  </style>
  <div id="outer">
    <div id="child"></div>
  </div>
  "#;

  let pixmap = renderer.render_html(html, 100, 100).expect("render");

  // Inside the original padding box.
  assert_red(rgba_at(&pixmap, 58, 58));
  // 5px beyond the padding box, but within `overflow-clip-margin: 10px`.
  assert_red(rgba_at(&pixmap, 65, 65));
  // Beyond the expanded clip edge: still clipped.
  assert_white(rgba_at(&pixmap, 72, 72));
}

fn overflow_clip_margin_preserves_rounded_corners(renderer: &mut fastrender::FastRender) {
  let html = r#"
  <style>
    body { margin: 0; background: white; }
    #outer {
      position: absolute;
      left: 10px;
      top: 10px;
      width: 50px;
      height: 50px;
      overflow: clip;
      overflow-clip-margin: 10px;
      border-radius: 10px;
    }
    #child {
      position: absolute;
      left: -10px;
      top: -10px;
      width: 70px;
      height: 70px;
      background: rgb(255, 0, 0);
    }
  </style>
  <div id="outer">
    <div id="child"></div>
  </div>
  "#;

  let pixmap = renderer.render_html(html, 100, 100).expect("render");

  // Sanity: the child paints within the original box.
  assert_red(rgba_at(&pixmap, 25, 25));
  // Expanded region should be visible.
  assert_red(rgba_at(&pixmap, 18, 2));
  // Rounded corner of the expanded clip should still clip.
  assert_white(rgba_at(&pixmap, 2, 7));
}

#[test]
fn overflow_clip_margin_expands_clip_area_display_list() {
  let mut renderer = create_stacking_context_bounds_renderer();
  overflow_clip_margin_expands_clip_area(&mut renderer);
}

#[test]
fn overflow_clip_margin_expands_clip_area_legacy() {
  let mut renderer = create_stacking_context_bounds_renderer_legacy();
  overflow_clip_margin_expands_clip_area(&mut renderer);
}

#[test]
fn overflow_clip_margin_preserves_rounded_corners_display_list() {
  let mut renderer = create_stacking_context_bounds_renderer();
  overflow_clip_margin_preserves_rounded_corners(&mut renderer);
}

#[test]
fn overflow_clip_margin_preserves_rounded_corners_legacy() {
  let mut renderer = create_stacking_context_bounds_renderer_legacy();
  overflow_clip_margin_preserves_rounded_corners(&mut renderer);
}

