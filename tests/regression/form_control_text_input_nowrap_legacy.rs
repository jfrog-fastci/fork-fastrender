use fastrender::debug::runtime::RuntimeToggles;
use fastrender::resource::ResourcePolicy;
use fastrender::{FastRender, FontConfig, LayoutParallelism, PaintParallelism, RenderOptions};
use std::collections::HashMap;
use std::sync::Once;

static INIT: Once = Once::new();

fn ensure_test_env() {
  INIT.call_once(|| {
    // See `tests/animation/support.rs` for background.
    crate::common::init_rayon_for_tests(1);
  });
}

fn bbox_for_ink(pixmap: &fastrender::Pixmap) -> Option<(u32, u32, u32, u32)> {
  let mut min_x = u32::MAX;
  let mut min_y = u32::MAX;
  let mut max_x = 0u32;
  let mut max_y = 0u32;

  for y in 0..pixmap.height() {
    for x in 0..pixmap.width() {
      let px = pixmap.pixel(x, y).expect("pixel in bounds");
      // We render on an opaque white background; treat any non-white pixel as text ink.
      if px.alpha() != 0 && (px.red() < 250 || px.green() < 250 || px.blue() < 250) {
        min_x = min_x.min(x);
        min_y = min_y.min(y);
        max_x = max_x.max(x);
        max_y = max_y.max(y);
      }
    }
  }

  (min_x != u32::MAX).then_some((min_x, min_y, max_x, max_y))
}

#[test]
fn legacy_text_input_forces_single_line_value_no_wrap() {
  ensure_test_env();

  // Force the legacy paint backend so this test exercises the code path in `src/paint/painter.rs`
  // (display-list painting already forces nowrap for `<input>` text values).
  let toggles = RuntimeToggles::from_map(HashMap::from([(
    "FASTR_PAINT_BACKEND".to_string(),
    "legacy".to_string(),
  )]));

  let mut renderer = FastRender::builder()
    .font_sources(FontConfig::bundled_only())
    .runtime_toggles(toggles.clone())
    .resource_policy(
      ResourcePolicy::default()
        .allow_http(false)
        .allow_https(false),
    )
    .paint_parallelism(PaintParallelism::disabled())
    .layout_parallelism(LayoutParallelism::disabled())
    .build()
    .expect("renderer");

  let html = r#"
    <!doctype html>
    <style>
      html, body { margin: 0; background: white; }
      input {
        width: 30px;
        height: 80px;
        padding: 0;
        border: none;
        background: transparent;
        font-size: 16px;
        color: black;
      }
    </style>
    <input type="text" value="MMMM MMMM MMMM MMMM MMMM MMMM MMMM MMMM" />
  "#;

  let options = RenderOptions::new()
    .with_viewport(120, 120)
    .with_runtime_toggles(toggles);
  let pixmap = renderer
    .render_html_with_options(html, options)
    .expect("render");

  let (_, min_y, _, max_y) = bbox_for_ink(&pixmap).expect("expected input value text to paint ink");
  let ink_height = max_y - min_y + 1;

  // With the legacy wrapping bug, the long value would soft-wrap into multiple lines because the
  // input is narrow but tall enough to fit them. We expect a single line of text, so the vertical
  // ink height should stay well below two line boxes.
  assert!(
    ink_height < 30,
    "expected `<input>` value to paint as a single line in legacy backend; ink height={ink_height} (y={min_y}..={max_y})"
  );
}

#[test]
fn legacy_text_input_forces_single_line_placeholder_no_wrap() {
  ensure_test_env();

  let toggles = RuntimeToggles::from_map(HashMap::from([(
    "FASTR_PAINT_BACKEND".to_string(),
    "legacy".to_string(),
  )]));

  let mut renderer = FastRender::builder()
    .font_sources(FontConfig::bundled_only())
    .runtime_toggles(toggles.clone())
    .resource_policy(
      ResourcePolicy::default()
        .allow_http(false)
        .allow_https(false),
    )
    .paint_parallelism(PaintParallelism::disabled())
    .layout_parallelism(LayoutParallelism::disabled())
    .build()
    .expect("renderer");

  let html = r#"
    <!doctype html>
    <style>
      html, body { margin: 0; background: white; }
      input {
        width: 30px;
        height: 80px;
        padding: 0;
        border: none;
        background: transparent;
        font-size: 16px;
        color: black;
      }
      /* Ensure placeholder pseudo styles would normally allow wrapping. */
      input::placeholder {
        white-space: pre-wrap;
        text-wrap: wrap;
        color: black;
        opacity: 1;
      }
    </style>
    <input type="text" placeholder="MMMM MMMM MMMM MMMM MMMM MMMM MMMM MMMM" />
  "#;

  let options = RenderOptions::new()
    .with_viewport(120, 120)
    .with_runtime_toggles(toggles);
  let pixmap = renderer
    .render_html_with_options(html, options)
    .expect("render");

  let (_, min_y, _, max_y) =
    bbox_for_ink(&pixmap).expect("expected input placeholder text to paint ink");
  let ink_height = max_y - min_y + 1;

  assert!(
    ink_height < 30,
    "expected `<input>` placeholder to paint as a single line in legacy backend; ink height={ink_height} (y={min_y}..={max_y})"
  );
}
