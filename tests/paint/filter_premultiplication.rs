use fastrender::image_loader::ImageCache;
use fastrender::paint::display_list_renderer::PaintParallelism;
use fastrender::paint::painter::{paint_tree_with_resources_scaled_offset_backend, PaintBackend};
use fastrender::scroll::ScrollState;
use fastrender::{FastRender, Point, Rgba};

fn render_with_backend(
  html: &str,
  width: u32,
  height: u32,
  background: Rgba,
  backend: PaintBackend,
) -> tiny_skia::Pixmap {
  let mut renderer = FastRender::new().expect("renderer");
  let dom = renderer.parse_html(html).expect("parsed");
  let fragment_tree = renderer
    .layout_document(&dom, width, height)
    .expect("laid out");

  let font_ctx = renderer.font_context().clone();
  let image_cache = ImageCache::new();

  paint_tree_with_resources_scaled_offset_backend(
    &fragment_tree,
    width,
    height,
    background,
    font_ctx,
    image_cache,
    1.0,
    Point::ZERO,
    PaintParallelism::default(),
    &ScrollState::default(),
    backend,
  )
  .expect("painted")
}

#[test]
fn contrast_filter_on_half_alpha_pixel_matches_spec() {
  let html = r#"
    <!doctype html>
    <style>
      body { margin: 0; background: white; }
      #target {
        width: 1px;
        height: 1px;
        background: rgba(64, 0, 0, 0.5);
        filter: contrast(2);
      }
    </style>
    <div id="target"></div>
  "#;

  let background = Rgba::WHITE;
  let legacy = render_with_backend(html, 1, 1, background, PaintBackend::Legacy);
  let display = render_with_backend(html, 1, 1, background, PaintBackend::DisplayList);

  // Chrome/Skia applies contrast in unpremultiplied space, then composites using truncating
  // `mul/255` arithmetic, resulting in a fully gray pixel.
  let expected = (127, 127, 127, 255);
  for (label, pixmap) in [("legacy", &legacy), ("display list", &display)] {
    let px = pixmap.pixels()[0];
    assert_eq!(
      (px.red(), px.green(), px.blue(), px.alpha()),
      expected,
      "{label} backend produced unexpected filtered pixel"
    );
  }

  assert_eq!(
    legacy.data(),
    display.data(),
    "contrast filter output diverged between backends"
  );
}

#[test]
fn baseline_semitransparent_fill_matches_between_backends() {
  let html = r#"
    <!doctype html>
    <style>
      body { margin: 0; background: white; }
      #target {
        width: 1px;
        height: 1px;
        background: rgba(64, 0, 0, 0.5);
      }
    </style>
    <div id="target"></div>
  "#;

  let background = Rgba::WHITE;
  let legacy = render_with_backend(html, 1, 1, background, PaintBackend::Legacy);
  let display = render_with_backend(html, 1, 1, background, PaintBackend::DisplayList);

  // Match Chrome/Skia's `source-over` compositing: premultiplied src-over using truncating
  // `mul/255` math (no rounding).
  let expected = (159, 127, 127, 255);
  for (label, pixmap) in [("legacy", &legacy), ("display list", &display)] {
    let px = pixmap.pixels()[0];
    assert_eq!(
      (px.red(), px.green(), px.blue(), px.alpha()),
      expected,
      "{label} backend produced unexpected baseline pixel"
    );
  }
}

#[test]
fn baseline_semitransparent_rounded_rect_fill_matches_chrome_blend() {
  let html = r#"
    <!doctype html>
    <style>
      body { margin: 0; background: rgb(227, 227, 227); }
      #target {
        width: 10px;
        height: 10px;
        background: rgba(0, 0, 0, 0.1);
        border-radius: 2px;
      }
    </style>
    <div id="target"></div>
  "#;

  let background = Rgba::rgb(227, 227, 227);
  let legacy = render_with_backend(html, 10, 10, background, PaintBackend::Legacy);
  let display = render_with_backend(html, 10, 10, background, PaintBackend::DisplayList);

  // Chrome/Skia uses truncating `mul/255` math for `source-over` compositing. For a fully covered
  // pixel, `(227 * (255 - round(0.1 * 255))) / 255 = 203` (truncating to an integer).
  let expected = (203, 203, 203, 255);
  for (label, pixmap) in [("legacy", &legacy), ("display list", &display)] {
    let px = pixmap.pixel(5, 5).expect("pixel");
    assert_eq!(
      (px.red(), px.green(), px.blue(), px.alpha()),
      expected,
      "{label} backend produced unexpected rounded-rect pixel"
    );
  }

  assert_eq!(
    legacy.data(),
    display.data(),
    "rounded-rect compositing diverged between backends"
  );
}

#[test]
fn baseline_semitransparent_rounded_rect_fill_under_clip_matches_chrome_blend() {
  let html = r#"
    <!doctype html>
    <style>
      body { margin: 0; background: rgb(227, 227, 227); }
      #clip {
        width: 10px;
        height: 10px;
        overflow: hidden;
      }
      #target {
        width: 20px;
        height: 20px;
        background: rgba(0, 0, 0, 0.1);
        border-radius: 2px;
      }
    </style>
    <div id="clip"><div id="target"></div></div>
  "#;

  let background = Rgba::rgb(227, 227, 227);
  let display = render_with_backend(html, 10, 10, background, PaintBackend::DisplayList);

  // Same expectation as `baseline_semitransparent_rounded_rect_fill_matches_chrome_blend`, but
  // ensure we still match when a clip mask is active.
  let expected = (203, 203, 203, 255);
  let px = display.pixel(5, 5).expect("pixel");
  assert_eq!(
    (px.red(), px.green(), px.blue(), px.alpha()),
    expected,
    "display list backend produced unexpected clipped rounded-rect pixel"
  );
}

#[test]
fn filters_on_semitransparent_pixels_match_legacy_backend() {
  let filters = [
    "brightness(1.5)",
    "contrast(0.25)",
    "grayscale(1)",
    "sepia(1)",
    "saturate(2)",
    "hue-rotate(90deg)",
    "invert(1)",
    "opacity(0.25)",
  ];

  for filter in filters {
    let html = format!(
      r#"
        <!doctype html>
        <style>
          body {{ margin: 0; background: white; }}
          #target {{
            width: 1px;
            height: 1px;
            background: rgba(64, 32, 16, 0.5);
            filter: {filter};
          }}
        </style>
        <div id="target"></div>
      "#
    );

    let background = Rgba::WHITE;
    let legacy = render_with_backend(&html, 1, 1, background, PaintBackend::Legacy);
    let display = render_with_backend(&html, 1, 1, background, PaintBackend::DisplayList);

    assert_eq!(
      legacy.data(),
      display.data(),
      "filter `{filter}` diverged between legacy and display list backends"
    );
  }
}

#[test]
fn backdrop_filter_uses_unpremultiplied_channels() {
  let html = r#"
    <!doctype html>
    <style>
      body { margin: 0; background: white; }
      .container { position: relative; width: 1px; height: 1px; }
      .backdrop {
        position: absolute;
        inset: 0;
        background: rgba(64, 0, 0, 0.5);
      }
      .overlay {
        position: absolute;
        inset: 0;
        backdrop-filter: contrast(2);
      }
    </style>
    <div class="container">
      <div class="backdrop"></div>
      <div class="overlay"></div>
    </div>
  "#;

  let background = Rgba::WHITE;
  let legacy = render_with_backend(html, 1, 1, background, PaintBackend::Legacy);
  let display = render_with_backend(html, 1, 1, background, PaintBackend::DisplayList);

  assert_eq!(
    legacy.data(),
    display.data(),
    "backdrop-filter contrast should match across backends"
  );
}

#[test]
fn drop_shadow_blur_matches_between_backends() {
  let html = r#"
    <!doctype html>
    <style>
      body { margin: 0; background: transparent; }
      #target {
        position: absolute;
        left: 16px;
        top: 16px;
        width: 4px;
        height: 4px;
        background: rgba(255, 255, 255, 1);
        filter: drop-shadow(6px 4px 6px rgba(20, 40, 80, 0.6));
      }
    </style>
    <div id="target"></div>
  "#;

  let legacy = render_with_backend(html, 64, 64, Rgba::TRANSPARENT, PaintBackend::Legacy);
  let display = render_with_backend(html, 64, 64, Rgba::TRANSPARENT, PaintBackend::DisplayList);

  assert_eq!(
    legacy.data(),
    display.data(),
    "blurred drop shadow should match legacy backend"
  );

  let corner_alpha = display.pixels()[0].alpha();
  assert_eq!(corner_alpha, 0, "shadow should not bleed to canvas edges");
}

#[test]
fn drop_shadow_spread_preserves_color_ratio() {
  let html = r#"
    <!doctype html>
    <style>
      body { margin: 0; background: transparent; }
      #target {
        position: absolute;
        left: 4px;
        top: 4px;
        width: 1px;
        height: 1px;
        background: rgba(255, 255, 255, 1);
        filter: drop-shadow(0 0 0 2px rgba(20, 40, 80, 0.5));
      }
    </style>
    <div id="target"></div>
  "#;

  let legacy = render_with_backend(html, 10, 10, Rgba::new(0, 0, 0, 0.0), PaintBackend::Legacy);
  let display = render_with_backend(
    html,
    10,
    10,
    Rgba::new(0, 0, 0, 0.0),
    PaintBackend::DisplayList,
  );

  assert_eq!(
    legacy.data(),
    display.data(),
    "drop-shadow spread should be consistent between backends"
  );

  assert_shadow_ratio(&display, (4, 4), [20.0 / 255.0, 40.0 / 255.0, 80.0 / 255.0]);
}

fn assert_shadow_ratio(pixmap: &tiny_skia::Pixmap, source: (u32, u32), expected: [f32; 3]) {
  let mut seen = false;
  for (idx, px) in pixmap.pixels().iter().enumerate() {
    let alpha = px.alpha();
    if alpha == 0 {
      continue;
    }
    let x = (idx as u32) % pixmap.width();
    let y = (idx as u32) / pixmap.width();
    if (x, y) == source {
      continue;
    }
    seen = true;
    let a = alpha as f32;
    let ratios = [
      px.red() as f32 / a,
      px.green() as f32 / a,
      px.blue() as f32 / a,
    ];
    for (ratio, expected_ratio) in ratios.iter().zip(expected) {
      assert!(
        (ratio - expected_ratio).abs() < 0.01,
        "shadow ratio drifted at ({x}, {y}): {ratios:?} vs expected {expected:?}"
      );
    }
  }
  assert!(seen, "no shadow pixels were rendered");
}
