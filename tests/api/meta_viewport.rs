use fastrender::api::{FastRender, FastRenderConfig};
use fastrender::debug::runtime::RuntimeToggles;
use fastrender::RenderOptions;
use std::collections::HashMap;
use tiny_skia::Pixmap;

fn bounding_box_for_color<F>(pixmap: &Pixmap, predicate: F) -> Option<(u32, u32, u32, u32)>
where
  F: Fn((u8, u8, u8, u8)) -> bool,
{
  let mut min_x = u32::MAX;
  let mut min_y = u32::MAX;
  let mut max_x = 0u32;
  let mut max_y = 0u32;
  let mut seen = false;

  for y in 0..pixmap.height() {
    for x in 0..pixmap.width() {
      let p = pixmap.pixel(x, y).unwrap();
      let rgba = (p.red(), p.green(), p.blue(), p.alpha());
      if predicate(rgba) {
        seen = true;
        min_x = min_x.min(x);
        min_y = min_y.min(y);
        max_x = max_x.max(x);
        max_y = max_y.max(y);
      }
    }
  }

  seen.then_some((min_x, min_y, max_x, max_y))
}

fn render_with_meta(meta_content: Option<&str>, width: u32, height: u32) -> Pixmap {
  let mut renderer =
    FastRender::with_config(FastRenderConfig::new().with_meta_viewport(true)).unwrap();

  let meta = meta_content
    .map(|content| format!(r#"<meta name="viewport" content="{content}">"#))
    .unwrap_or_default();

  let html = format!(
    r#"
    <html>
      <head>
        {meta}
        <style>
          html, body {{ margin: 0; padding: 0; width: 100%; height: 100%; }}
          #box {{ width: 50vw; height: 50vh; background: rgb(255, 0, 0); }}
          @media (min-width: 450px) {{
            #box {{ background: rgb(0, 200, 0); width: 60vw; height: 60vh; }}
          }}
        </style>
      </head>
      <body>
        <div id="box"></div>
      </body>
    </html>
    "#
  );

  let prepared = renderer
    .prepare_html(&html, RenderOptions::default().with_viewport(width, height))
    .unwrap();
  prepared.paint_default().unwrap()
}

#[test]
fn meta_viewport_alters_layout_viewport_dimensions() {
  let _lock = crate::common::global_test_lock();
  let mut renderer =
    FastRender::with_config(FastRenderConfig::new().with_meta_viewport(true)).unwrap();

  let cases = [
    ("no meta", None, (800, 600)),
    ("device width", Some("width=device-width"), (800, 600)),
    ("explicit width", Some("width=320"), (320, 240)),
    ("height only", Some("height=400"), (533, 400)),
    (
      "width and height",
      Some("width=300, height=400"),
      (300, 400),
    ),
    ("initial scale only", Some("initial-scale=2"), (400, 300)),
    (
      "width with max scale",
      Some("width=320, maximum-scale=1"),
      (320, 600),
    ),
    (
      "height derived scale",
      Some("height=400, maximum-scale=2"),
      (533, 400),
    ),
  ];

  for (label, meta, expected) in cases {
    let meta_tag = meta
      .map(|content| format!(r#"<meta name="viewport" content="{content}">"#))
      .unwrap_or_default();
    let html = format!(r#"<html><head>{meta_tag}</head><body></body></html>"#);
    let prepared = renderer
      .prepare_html(&html, RenderOptions::default().with_viewport(800, 600))
      .unwrap();
    let viewport = prepared.layout_viewport();
    assert_eq!(
      (
        viewport.width.round() as u32,
        viewport.height.round() as u32,
      ),
      expected,
      "{label} should resolve to the expected layout viewport"
    );
  }
}

#[test]
fn meta_viewport_drives_vw_vh_and_media_queries() {
  let _lock = crate::common::global_test_lock();
  let cases = [
    ("narrow red", "width=320", (400, 300), (255, 0, 0, 255)),
    (
      "zoomed green",
      "width=device-width, initial-scale=2",
      (800, 360),
      (0, 200, 0, 255),
    ),
    (
      "height derived green",
      "height=400",
      (480, 360),
      (0, 200, 0, 255),
    ),
    (
      "narrower layout",
      "width=300, height=400",
      (400, 533),
      (255, 0, 0, 255),
    ),
    (
      "scaled but wide",
      "initial-scale=0.5",
      (480, 360),
      (0, 200, 0, 255),
    ),
  ];

  for (label, meta, expected_size, expected_color) in cases {
    let pixmap = render_with_meta(Some(meta), 800, 600);
    let bbox = bounding_box_for_color(&pixmap, |(r, g, b, a)| {
      a > 0 && (r, g, b) == (expected_color.0, expected_color.1, expected_color.2)
    })
    .unwrap_or_else(|| panic!("{label}: box color not found in render"));
    let width = bbox.2 - bbox.0 + 1;
    let height = bbox.3 - bbox.1 + 1;

    assert!(
      (width as i32 - expected_size.0 as i32).abs() <= 1,
      "{label}: expected width {} got {}",
      expected_size.0,
      width
    );
    assert!(
      (height as i32 - expected_size.1 as i32).abs() <= 1,
      "{label}: expected height {} got {}",
      expected_size.1,
      height
    );
  }
}

#[test]
fn meta_viewport_width_or_height_derived_zoom_preserves_output_dimensions() {
  let _lock = crate::common::global_test_lock();
  let mut renderer =
    FastRender::with_config(FastRenderConfig::new().with_meta_viewport(true)).unwrap();

  // Ensure environment/runtime toggle overrides (e.g. FASTR_FULL_PAGE) cannot expand the canvas and
  // invalidate dimension assertions.
  let runtime_toggles = RuntimeToggles::from_map(HashMap::from([(
    "FASTR_FULL_PAGE".to_string(),
    "0".to_string(),
  )]));

  // When meta viewport implies a zoom derived from a single axis (width or height), the renderer
  // should still honor the requested output surface dimensions.
  let cases = [
    ("width derived zoom", "width=300"),
    ("height derived zoom", "height=200"),
  ];

  for (label, meta) in cases {
    let html =
      format!(r#"<html><head><meta name="viewport" content="{meta}"></head><body></body></html>"#);
    let options = RenderOptions::new()
      .with_viewport(900, 600)
      .with_runtime_toggles(runtime_toggles.clone());
    let pixmap = renderer
      .render_html_with_options(&html, options)
      .unwrap_or_else(|err| panic!("{label}: render failed: {err:?}"));
    assert_eq!(
      (pixmap.width(), pixmap.height()),
      (900, 600),
      "{label}: output dimensions should remain stable"
    );
  }
}
