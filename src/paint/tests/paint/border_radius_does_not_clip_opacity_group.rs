use super::util::create_stacking_context_bounds_renderer;
use tiny_skia::Pixmap;

fn rgba_at(pixmap: &Pixmap, x: u32, y: u32) -> (u8, u8, u8, u8) {
  let p = pixmap.pixel(x, y).expect("pixel");
  (p.red(), p.green(), p.blue(), p.alpha())
}

fn assert_rgba_approx(actual: (u8, u8, u8, u8), expected: (u8, u8, u8, u8), tol: u8, msg: &str) {
  let within = |a: u8, b: u8| (a as i32 - b as i32).abs() <= tol as i32;
  assert!(
    within(actual.0, expected.0)
      && within(actual.1, expected.1)
      && within(actual.2, expected.2)
      && within(actual.3, expected.3),
    "{msg}: expected approx rgba{expected:?} ±{tol}, got rgba{actual:?}"
  );
}

#[test]
fn border_radius_does_not_clip_opacity_group_descendants() {
  let html = r#"<!doctype html>
    <style>
      body { margin: 0; background: white; }
      #parent {
        position: absolute;
        left: 0px;
        top: 0px;
        width: 60px;
        height: 60px;
        border-radius: 20px;
        opacity: 0.5;
        background: transparent;
        overflow: visible;
      }
      #child {
        width: 60px;
        height: 60px;
        background: rgb(255, 0, 0);
      }
    </style>
    <div id="parent"><div id="child"></div></div>
  "#;

  let mut renderer = create_stacking_context_bounds_renderer();
  let pixmap = renderer.render_html(html, 60, 60).expect("render");

  // Outside the rounded corner (would be clipped if border-radius became an implicit stacking
  // context mask).
  assert_rgba_approx(
    rgba_at(&pixmap, 2, 2),
    (255, 127, 127, 255),
    2,
    "top-left corner pixel should include the child (with opacity applied)",
  );

  // Sanity: well inside the radius should also see the opacity group output.
  assert_rgba_approx(
    rgba_at(&pixmap, 30, 30),
    (255, 127, 127, 255),
    2,
    "center pixel should include the child (with opacity applied)",
  );
}

#[test]
fn backdrop_filter_is_clipped_to_border_radius_but_child_is_not() {
  let html = r#"<!doctype html>
    <style>
      body { margin: 0; background: rgb(0, 255, 0); }
      #parent {
        position: absolute;
        left: 0px;
        top: 0px;
        width: 60px;
        height: 60px;
        border-radius: 20px;
        backdrop-filter: invert(1);
        overflow: visible;
      }
      #child {
        position: absolute;
        left: 0px;
        top: 0px;
        width: 10px;
        height: 10px;
        background: rgb(255, 0, 0);
      }
    </style>
    <div id="parent"><div id="child"></div></div>
  "#;

  let mut renderer = create_stacking_context_bounds_renderer();
  let pixmap = renderer.render_html(html, 60, 60).expect("render");

  // Inside the rounded region (and outside the child): backdrop-filter inverts green to magenta.
  assert_rgba_approx(
    rgba_at(&pixmap, 15, 5),
    (255, 0, 255, 255),
    2,
    "pixel inside rounded region should show inverted backdrop (magenta)",
  );

  // Outside the rounded region but inside the child: child should still be visible (not clipped by
  // border-radius).
  assert_rgba_approx(
    rgba_at(&pixmap, 2, 2),
    (255, 0, 0, 255),
    2,
    "pixel outside rounded region but inside child should remain red",
  );

  // Outside the rounded region and outside the child: no backdrop-filter effect, so remains green.
  assert_rgba_approx(
    rgba_at(&pixmap, 0, 10),
    (0, 255, 0, 255),
    2,
    "pixel outside rounded region should remain the original backdrop (green)",
  );
}

