use super::util::{
  bounding_box_for_color, create_stacking_context_bounds_renderer, create_stacking_context_bounds_renderer_legacy,
};
use std::cmp::max;
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

fn assert_is_red(rgba: (u8, u8, u8, u8), msg: &str) {
  let (r, g, b, a) = rgba;
  assert!(
    a > 240 && r > 220 && g < 80 && b < 80,
    "{msg}: expected red fill, got rgba=({r},{g},{b},{a})"
  );
}

fn assert_is_blue(rgba: (u8, u8, u8, u8), msg: &str) {
  let (r, g, b, a) = rgba;
  assert!(
    a > 240 && b > 220 && r < 80 && g < 80,
    "{msg}: expected blue fill, got rgba=({r},{g},{b},{a})"
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
fn progress_and_meter_paint_fill_proportion_and_accent_color() {
  let html = r#"
    <!doctype html>
    <style>
      body { margin: 0; background: white; }
      progress, meter {
        position: absolute;
        left: 0px;
        width: 200px;
        height: 20px;
        border: 0;
        padding: 0;
        border-radius: 0;
        background: rgb(200, 200, 200);
      }
      #p25 { top: 0px; accent-color: rgb(0, 255, 0); }
      #p75 { top: 30px; accent-color: rgb(255, 0, 0); }
      #m50 { top: 60px; accent-color: rgb(0, 0, 255); }
    </style>
    <progress id="p25" value="25" max="100"></progress>
    <progress id="p75" value="75" max="100"></progress>
    <meter id="m50" value="0.5" min="0" max="1"></meter>
  "#;

  let (dl, legacy) = render_both(html, 220, 90);
  for (backend, pixmap) in [("display_list", &dl), ("legacy", &legacy)] {
    // Progress 25%: fill should be green on the left, gray on the far right.
    assert_is_green(
      rgba_at(pixmap, 10, 10),
      &format!("{backend}: progress@25% left sample"),
    );
    assert_is_track_gray(
      rgba_at(pixmap, 190, 10),
      &format!("{backend}: progress@25% right sample"),
    );

    // Progress 75%: fill should be red well past the midpoint, gray on the far right.
    assert_is_red(
      rgba_at(pixmap, 140, 40),
      &format!("{backend}: progress@75% fill sample"),
    );
    assert_is_track_gray(
      rgba_at(pixmap, 190, 40),
      &format!("{backend}: progress@75% track sample"),
    );

    // Meter 50%: fill should be blue on the left, gray past the fill.
    assert_is_blue(
      rgba_at(pixmap, 10, 70),
      &format!("{backend}: meter@50% fill sample"),
    );
    assert_is_track_gray(
      rgba_at(pixmap, 150, 70),
      &format!("{backend}: meter@50% track sample"),
    );

    // Bounding boxes for fill regions encode the expected proportions (roughly).
    let green_bbox = bounding_box_for_color(pixmap, |(r, g, b, a)| a > 240 && g > 220 && r < 80 && b < 80)
      .expect("expected green pixels for 25% progress");
    let green_width = green_bbox.2.saturating_sub(green_bbox.0) + 1;
    assert!(
      (green_width as i32 - 50).abs() <= 2,
      "{backend}: expected ~50px green fill width, got {green_width} (bbox={green_bbox:?})"
    );

    let red_bbox = bounding_box_for_color(pixmap, |(r, g, b, a)| a > 240 && r > 220 && g < 80 && b < 80)
      .expect("expected red pixels for 75% progress");
    let red_width = red_bbox.2.saturating_sub(red_bbox.0) + 1;
    assert!(
      (red_width as i32 - 150).abs() <= 2,
      "{backend}: expected ~150px red fill width, got {red_width} (bbox={red_bbox:?})"
    );

    // Meter's blue fill should be roughly half the bar width.
    let blue_bbox = bounding_box_for_color(pixmap, |(r, g, b, a)| a > 240 && b > 220 && r < 80 && g < 80)
      .expect("expected blue pixels for 50% meter");
    let blue_width = blue_bbox.2.saturating_sub(blue_bbox.0) + 1;
    assert!(
      (blue_width as i32 - 100).abs() <= 2,
      "{backend}: expected ~100px blue fill width, got {blue_width} (bbox={blue_bbox:?})"
    );

    // Sanity: bounding boxes should stay inside the 200px bars.
    assert!(max(green_bbox.2, max(red_bbox.2, blue_bbox.2)) <= 199);
  }
}

