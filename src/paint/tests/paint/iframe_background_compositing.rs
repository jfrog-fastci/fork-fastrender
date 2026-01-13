use super::util::create_stacking_context_bounds_renderer;

fn pixel_rgba(pixmap: &tiny_skia::Pixmap, x: u32, y: u32) -> (u8, u8, u8, u8) {
  let p = pixmap.pixel(x, y).expect("pixel should exist");
  (p.red(), p.green(), p.blue(), p.alpha())
}

fn assert_rgba_near(actual: (u8, u8, u8, u8), expected: (u8, u8, u8, u8), tolerance: u8) {
  let diff = |a: u8, b: u8| -> u8 { a.abs_diff(b) };
  assert!(
    diff(actual.0, expected.0) <= tolerance
      && diff(actual.1, expected.1) <= tolerance
      && diff(actual.2, expected.2) <= tolerance
      && diff(actual.3, expected.3) <= tolerance,
    "expected rgba ~= {:?} (±{}), got {:?}",
    expected,
    tolerance,
    actual
  );
}

#[test]
fn iframe_element_background_not_double_composited_for_transparent_srcdoc() {
  let mut renderer = create_stacking_context_bounds_renderer();
  let html = r#"<!doctype html>
    <style>
      html, body { margin: 0; padding: 0; background: rgb(0, 0, 255); }
      iframe {
        display: block;
        width: 20px;
        height: 20px;
        border: 0;
        background: rgba(255, 0, 0, 0.5);
      }
    </style>
    <iframe srcdoc="<html><head><style>html, body { margin: 0; padding: 0; background: transparent; }</style></head><body></body></html>"></iframe>
  "#;

  let pixmap = renderer
    .render_html(html, 40, 40)
    .expect("render should succeed");

  // FastRender renders iframe documents onto an opaque white canvas by default. Even if the iframe
  // element itself has a semi-transparent background, the nested canvas should cover it.
  assert_rgba_near(pixel_rgba(&pixmap, 10, 10), (255, 255, 255, 255), 0);
}

#[test]
fn iframe_opaque_background_painted_by_parent_for_transparent_srcdoc() {
  let mut renderer = create_stacking_context_bounds_renderer();
  let html = r#"<!doctype html>
    <style>
      html, body { margin: 0; padding: 0; background: rgb(0, 0, 255); }
      iframe {
        display: block;
        width: 20px;
        height: 20px;
        border: 0;
        background: rgb(255, 0, 0);
      }
    </style>
    <iframe srcdoc="<html><head><style>html, body { margin: 0; padding: 0; background: transparent; }</style></head><body></body></html>"></iframe>
  "#;

  let pixmap = renderer
    .render_html(html, 40, 40)
    .expect("render should succeed");

  // The nested iframe canvas defaults to opaque white, covering the parent element background.
  assert_rgba_near(pixel_rgba(&pixmap, 10, 10), (255, 255, 255, 255), 0);
}
