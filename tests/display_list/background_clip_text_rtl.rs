use fastrender::debug::runtime::RuntimeToggles;
use fastrender::{FastRender, FastRenderConfig};
use std::collections::HashMap;
use tiny_skia::Pixmap;

#[derive(Clone, Copy, Debug)]
struct Bounds {
  min_x: u32,
  min_y: u32,
  max_x: u32,
  max_y: u32,
}

fn is_black(px: tiny_skia::PremultipliedColorU8) -> bool {
  px.red() == 0 && px.green() == 0 && px.blue() == 0
}

fn non_black_bounds(pixmap: &Pixmap) -> Option<Bounds> {
  let w = pixmap.width();
  let h = pixmap.height();
  if w == 0 || h == 0 {
    return None;
  }

  let mut min_x = w;
  let mut min_y = h;
  let mut max_x = 0u32;
  let mut max_y = 0u32;
  let mut found = false;

  for y in 0..h {
    for x in 0..w {
      let Some(px) = pixmap.pixel(x, y) else {
        continue;
      };
      if is_black(px) {
        continue;
      }
      found = true;
      min_x = min_x.min(x);
      min_y = min_y.min(y);
      max_x = max_x.max(x);
      max_y = max_y.max(y);
    }
  }

  found.then_some(Bounds {
    min_x,
    min_y,
    max_x,
    max_y,
  })
}

fn brightest_non_black_pixel(pixmap: &Pixmap) -> Option<(u32, u32)> {
  let w = pixmap.width();
  let h = pixmap.height();
  let mut best = None;
  let mut best_sum = 0u32;

  for y in 0..h {
    for x in 0..w {
      let Some(px) = pixmap.pixel(x, y) else {
        continue;
      };
      if is_black(px) {
        continue;
      }
      let sum = u32::from(px.red()) + u32::from(px.green()) + u32::from(px.blue());
      if sum > best_sum {
        best_sum = sum;
        best = Some((x, y));
      }
    }
  }

  best
}

fn assert_bounds_close(label: &str, reference: Bounds, actual: Bounds, tolerance: u32) {
  let min_x_delta = reference.min_x.abs_diff(actual.min_x);
  let max_x_delta = reference.max_x.abs_diff(actual.max_x);
  let min_y_delta = reference.min_y.abs_diff(actual.min_y);
  let max_y_delta = reference.max_y.abs_diff(actual.max_y);
  assert!(
    min_x_delta <= tolerance
      && max_x_delta <= tolerance
      && min_y_delta <= tolerance
      && max_y_delta <= tolerance,
    "{label}: expected bounds close (tol={tolerance}), got ref={reference:?} actual={actual:?}",
  );
}

fn render(renderer: &mut FastRender, html: &str, width: u32, height: u32) -> Pixmap {
  renderer.render_html(html, width, height).expect("render html")
}

#[test]
fn display_list_background_clip_text_rtl_matches_text_ink_bounds() {
  let toggles = RuntimeToggles::from_map(HashMap::from([(
    "FASTR_PAINT_BACKEND".to_string(),
    "display_list".to_string(),
  )]));
  let config = FastRenderConfig::new().with_runtime_toggles(toggles);
  let mut renderer = FastRender::with_config(config).expect("create renderer");

  let text = "שלום";
  let common_css = r#"
    @font-face {
      font-family: "Hebrew";
      src: url("tests/fixtures/fonts/NotoSansHebrew-subset.ttf") format("truetype");
    }
    html, body { margin: 0; padding: 0; background: rgb(0, 0, 0); }
    #t {
      font-family: "Hebrew";
      font-size: 56px;
      line-height: 1;
      padding: 24px;
      width: 420px;
      white-space: nowrap;
      text-align: left;
    }
  "#;

  let reference_html = format!(
    r#"<!doctype html><html><head><style>
      {common_css}
      #t {{ color: rgb(255, 255, 255); }}
    </style></head><body>
      <div id="t" dir="rtl">{text}</div>
    </body></html>"#
  );

  let clipped_html = format!(
    r#"<!doctype html><html><head><style>
      {common_css}
      #t {{
        background: linear-gradient(90deg, rgb(255, 0, 0), rgb(0, 0, 255));
        -webkit-background-clip: text;
        background-clip: text;
        color: transparent;
        -webkit-text-fill-color: transparent;
      }}
    </style></head><body>
      <div id="t" dir="rtl">{text}</div>
    </body></html>"#
  );

  let viewport_w = 480;
  let viewport_h = 140;
  let reference = render(&mut renderer, &reference_html, viewport_w, viewport_h);
  let clipped = render(&mut renderer, &clipped_html, viewport_w, viewport_h);

  let reference_bounds = non_black_bounds(&reference).expect("reference text should paint");
  let clipped_bounds = non_black_bounds(&clipped).expect("clipped text should paint");
  assert_bounds_close(
    "rtl clipped text bounds",
    reference_bounds,
    clipped_bounds,
    2,
  );

  let (sample_x, sample_y) =
    brightest_non_black_pixel(&reference).expect("reference should have non-black pixels");
  let clipped_px = clipped
    .pixel(sample_x, sample_y)
    .expect("sample pixel should be in bounds");
  assert!(
    !is_black(clipped_px),
    "expected clipped text to include an ink pixel at ({sample_x},{sample_y}); got {:?}",
    (clipped_px.red(), clipped_px.green(), clipped_px.blue(), clipped_px.alpha())
  );

  let mid_y = (reference_bounds.min_y + reference_bounds.max_y) / 2;
  let left_out_x = reference_bounds.min_x.saturating_sub(8);
  let right_out_x = (reference_bounds.max_x + 8).min(viewport_w - 1);
  if left_out_x < reference_bounds.min_x {
    let px = clipped.pixel(left_out_x, mid_y).expect("pixel in bounds");
    assert!(
      is_black(px),
      "expected clipped background to remain black outside ink bounds (left probe at {left_out_x},{mid_y}), got {:?}",
      (px.red(), px.green(), px.blue(), px.alpha())
    );
  }
  if right_out_x > reference_bounds.max_x {
    let px = clipped.pixel(right_out_x, mid_y).expect("pixel in bounds");
    assert!(
      is_black(px),
      "expected clipped background to remain black outside ink bounds (right probe at {right_out_x},{mid_y}), got {:?}",
      (px.red(), px.green(), px.blue(), px.alpha())
    );
  }
}

#[test]
fn display_list_background_clip_text_mixed_direction_preserves_rtl_run_position() {
  let toggles = RuntimeToggles::from_map(HashMap::from([(
    "FASTR_PAINT_BACKEND".to_string(),
    "display_list".to_string(),
  )]));
  let config = FastRenderConfig::new().with_runtime_toggles(toggles);
  let mut renderer = FastRender::with_config(config).expect("create renderer");

  let text_latin = "ABC";
  let text_hebrew = "שלום";
  let common_css = r#"
    @font-face {
      font-family: "Hebrew";
      src: url("tests/fixtures/fonts/NotoSansHebrew-subset.ttf") format("truetype");
    }
    @font-face {
      font-family: "Latin";
      src: url("tests/fixtures/fonts/NotoSans-subset.ttf") format("truetype");
    }
    html, body { margin: 0; padding: 0; background: rgb(0, 0, 0); }
    #t {
      font-family: "Hebrew";
      font-size: 52px;
      line-height: 1;
      padding: 24px;
      width: 520px;
      white-space: nowrap;
      text-align: left;
    }
    .latin { font-family: "Latin"; }
  "#;

  // Reference: hide the Latin portion by matching the background color so we can probe Hebrew ink
  // positions in the presence of mixed-direction shaping.
  let reference_html = format!(
    r#"<!doctype html><html><head><style>
      {common_css}
      .latin {{ color: rgb(0, 0, 0); }}
      .hebrew {{ color: rgb(255, 255, 255); }}
    </style></head><body>
      <div id="t" dir="rtl"><span class="latin">{text_latin}</span> <span class="hebrew">{text_hebrew}</span></div>
    </body></html>"#
  );

  let clipped_html = format!(
    r#"<!doctype html><html><head><style>
      {common_css}
      #t {{
        background: linear-gradient(90deg, rgb(255, 0, 0), rgb(0, 0, 255));
        -webkit-background-clip: text;
        background-clip: text;
        color: transparent;
        -webkit-text-fill-color: transparent;
      }}
    </style></head><body>
      <div id="t" dir="rtl"><span class="latin">{text_latin}</span> <span class="hebrew">{text_hebrew}</span></div>
    </body></html>"#
  );

  let viewport_w = 600;
  let viewport_h = 160;
  let reference = render(&mut renderer, &reference_html, viewport_w, viewport_h);
  let clipped = render(&mut renderer, &clipped_html, viewport_w, viewport_h);

  let (sample_x, sample_y) =
    brightest_non_black_pixel(&reference).expect("reference Hebrew should paint non-black pixels");
  let clipped_px = clipped
    .pixel(sample_x, sample_y)
    .expect("sample pixel should be in bounds");
  assert!(
    !is_black(clipped_px),
    "expected clipped text to include Hebrew ink pixel at ({sample_x},{sample_y}); got {:?}",
    (clipped_px.red(), clipped_px.green(), clipped_px.blue(), clipped_px.alpha())
  );
}
