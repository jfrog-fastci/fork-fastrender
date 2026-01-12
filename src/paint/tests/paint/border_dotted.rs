use super::util::{
  create_stacking_context_bounds_renderer, create_stacking_context_bounds_renderer_legacy,
};
use crate::Pixmap;

fn brightness(pixmap: &Pixmap, x: u32, y: u32) -> u8 {
  let p = pixmap.pixel(x, y).expect("pixel in bounds");
  ((u16::from(p.red()) + u16::from(p.green()) + u16::from(p.blue())) / 3) as u8
}

fn assert_dotted_border_has_gaps(pixmap: &Pixmap, x0: u32, x1: u32, y0: u32, y1: u32) {
  // Dotted borders are drawn using anti-aliased round dots; gap pixels may not be fully white due
  // to nearby coverage. Use a relatively high threshold to classify "background-like" pixels.
  const GAP_THRESHOLD: u8 = 220;

  // Find the row within [y0, y1) with the most "ink" pixels (anything not near-white). This makes
  // the assertion robust to small differences in stroke alignment between backends.
  let mut best_y = y0;
  let mut best_ink = 0u32;
  for y in y0..y1 {
    let mut ink = 0u32;
    for x in x0..x1 {
      if brightness(pixmap, x, y) < GAP_THRESHOLD {
        ink += 1;
      }
    }
    if ink > best_ink {
      best_ink = ink;
      best_y = y;
    }
  }

  assert!(best_ink > 0, "expected border row to contain ink pixels");

  let mut gaps = 0u32;
  let mut transitions = 0u32;
  let mut prev_is_gap = None;
  for x in x0..x1 {
    let is_gap = brightness(pixmap, x, best_y) >= GAP_THRESHOLD;
    if is_gap {
      gaps += 1;
    }
    if let Some(prev) = prev_is_gap {
      if prev != is_gap {
        transitions += 1;
      }
    }
    prev_is_gap = Some(is_gap);
  }

  // A dotted border should contain background-colored gaps and have frequent transitions between
  // dots and gaps. (A solid line would have ~0 gaps and only two transitions.)
  assert!(
    gaps > 10,
    "expected dotted border to have gaps; found {gaps} gap pixels in row y={best_y}"
  );
  assert!(
    transitions > 10,
    "expected dotted border to alternate; found {transitions} transitions in row y={best_y}"
  );
}

fn dotted_border_fixture(renderer: &mut crate::FastRender) -> Pixmap {
  let html = r#"
    <style>
      body { margin: 0; background: rgb(255, 255, 255); }
      #target {
        position: absolute;
        left: 10px;
        top: 10px;
        width: 120px;
        height: 30px;
        border-top: 1px dotted rgb(0, 0, 0);
      }
    </style>
    <div id="target"></div>
  "#;
  renderer.render_html(html, 160, 60).expect("render")
}

#[test]
fn dotted_border_has_gaps_display_list_backend() {
  std::thread::Builder::new()
    .stack_size(64 * 1024 * 1024)
    .spawn(|| {
      let mut renderer = create_stacking_context_bounds_renderer();
      let pixmap = dotted_border_fixture(&mut renderer);

      // The border spans (10,10)-(130,11). Probe the area excluding the ends to avoid edge effects.
      assert_dotted_border_has_gaps(&pixmap, 15, 125, 9, 13);
    })
    .unwrap()
    .join()
    .unwrap();
}

#[test]
fn dotted_border_has_gaps_legacy_backend() {
  std::thread::Builder::new()
    .stack_size(64 * 1024 * 1024)
    .spawn(|| {
      let mut renderer = create_stacking_context_bounds_renderer_legacy();
      let pixmap = dotted_border_fixture(&mut renderer);

      assert_dotted_border_has_gaps(&pixmap, 15, 125, 9, 13);
    })
    .unwrap()
    .join()
    .unwrap();
}
