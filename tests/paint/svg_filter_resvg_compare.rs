use fastrender::geometry::Rect;
use fastrender::image_loader::ImageCache;
use fastrender::paint::svg_filter::{apply_svg_filter, parse_svg_filter_from_svg_document};
use resvg::usvg;
use tiny_skia::{Pixmap, Transform};

fn rgba_at(pixmap: &Pixmap, x: u32, y: u32) -> [u8; 4] {
  let px = pixmap.pixel(x, y).expect("pixel in bounds");
  [px.red(), px.green(), px.blue(), px.alpha()]
}

fn assert_pixmaps_match_with_tolerance(actual: &Pixmap, expected: &Pixmap, tolerance: u8) {
  assert_eq!(
    (actual.width(), actual.height()),
    (expected.width(), expected.height()),
    "pixmap size mismatch: actual={}x{} expected={}x{}",
    actual.width(),
    actual.height(),
    expected.width(),
    expected.height()
  );

  let mut max_delta = 0u8;
  let mut max_x = 0u32;
  let mut max_y = 0u32;
  let mut max_actual = [0u8; 4];
  let mut max_expected = [0u8; 4];
  let mut max_delta_rgba = [0u8; 4];

  for y in 0..actual.height() {
    for x in 0..actual.width() {
      let a = rgba_at(actual, x, y);
      let e = rgba_at(expected, x, y);
      let delta = [
        a[0].abs_diff(e[0]),
        a[1].abs_diff(e[1]),
        a[2].abs_diff(e[2]),
        a[3].abs_diff(e[3]),
      ];
      let local_max = delta.into_iter().max().unwrap_or(0);
      if local_max > max_delta {
        max_delta = local_max;
        max_x = x;
        max_y = y;
        max_actual = a;
        max_expected = e;
        max_delta_rgba = delta;
      }
    }
  }

  if max_delta > tolerance {
    panic!(
      "pixmap mismatch (tolerance={tolerance}): max_delta={max_delta} at ({max_x},{max_y})\n  expected RGBA={max_expected:?}\n    actual RGBA={max_actual:?}\n           Δ={max_delta_rgba:?}"
    );
  }
}

fn render_svg_resvg(svg: &str, width: u32, height: u32) -> Pixmap {
  let tree = usvg::Tree::from_str(svg, &usvg::Options::default()).expect("parse SVG for resvg");
  let size = tree.size();
  let source_w = size.width() as f32;
  let source_h = size.height() as f32;
  assert!(
    source_w.is_finite() && source_h.is_finite() && source_w > 0.0 && source_h > 0.0,
    "resvg reported invalid SVG size: {source_w}x{source_h}"
  );
  let scale_x = width as f32 / source_w;
  let scale_y = height as f32 / source_h;
  let transform = Transform::from_scale(scale_x, scale_y);
  let mut pixmap = Pixmap::new(width, height).expect("pixmap");
  resvg::render(&tree, transform, &mut pixmap.as_mut());
  pixmap
}

fn strip_filter_reference(svg: &str, filter_id: &str) -> String {
  let mut out = svg.to_string();
  let mut replaced = false;
  for pattern in [
    format!(" filter=\"url(#{filter_id})\""),
    format!(" filter='url(#{filter_id})'"),
    format!("filter=\"url(#{filter_id})\""),
    format!("filter='url(#{filter_id})'"),
  ] {
    if out.contains(&pattern) {
      replaced = true;
      out = out.replace(&pattern, "");
    }
  }
  assert!(
    replaced,
    "SVG did not contain a filter reference for id '{filter_id}'"
  );
  out
}

fn assert_svg_filter_matches_resvg_custom(
  svg_source: &str,
  svg_expected: &str,
  strip_filter_id: &str,
  parse_filter_id: &str,
  bbox_css_px: Rect,
  viewport: (u32, u32),
  tolerance: u8,
) {
  let (viewport_w, viewport_h) = viewport;

  let source_svg = strip_filter_reference(svg_source, strip_filter_id);
  let source_pixmap = render_svg_resvg(&source_svg, viewport_w, viewport_h);

  let filter = parse_svg_filter_from_svg_document(svg_source, Some(parse_filter_id), &ImageCache::new())
    .unwrap_or_else(|| {
      panic!("parse_svg_filter_from_svg_document: missing filter #{parse_filter_id}")
    });
  let mut fastrender = source_pixmap.clone();
  apply_svg_filter(filter.as_ref(), &mut fastrender, 1.0, bbox_css_px).expect("apply_svg_filter");

  let expected = render_svg_resvg(svg_expected, viewport_w, viewport_h);

  assert_pixmaps_match_with_tolerance(&fastrender, &expected, tolerance);
}

fn assert_svg_filter_matches_resvg(
  svg: &str,
  filter_id: &str,
  bbox_css_px: Rect,
  viewport: (u32, u32),
  tolerance: u8,
) {
  assert_svg_filter_matches_resvg_custom(
    svg,
    svg,
    filter_id,
    filter_id,
    bbox_css_px,
    viewport,
    tolerance,
  );
}

#[test]
fn svg_filter_resvg_color_matrix_matrix_defaults_to_linear_rgb() {
  let svg = r#"
    <svg xmlns="http://www.w3.org/2000/svg" width="16" height="16" shape-rendering="crispEdges">
      <defs>
        <!-- Omit color-interpolation-filters to exercise the default (linearRGB). -->
        <filter id="f" x="0" y="0" width="16" height="16" filterUnits="userSpaceOnUse">
          <feColorMatrix type="matrix"
            values="2 0 0 0 0
                    0 2 0 0 0
                    0 0 2 0 0
                    0 0 0 1 0" />
        </filter>
      </defs>
      <g filter="url(#f)">
        <rect x="0" y="0" width="16" height="16" fill="rgb(128, 128, 128)" />
      </g>
    </svg>
  "#;
  assert_svg_filter_matches_resvg(
    svg,
    "f",
    Rect::from_xywh(0.0, 0.0, 16.0, 16.0),
    (16, 16),
    1,
  );
}

#[test]
fn svg_filter_resvg_composite_over_matches_porter_duff_premultiplied() {
  let svg = r#"
    <svg xmlns="http://www.w3.org/2000/svg" width="8" height="8" shape-rendering="crispEdges">
      <defs>
        <filter id="f" x="0" y="0" width="8" height="8" filterUnits="userSpaceOnUse">
          <feFlood flood-color="rgb(255, 0, 0)" flood-opacity="0.5" result="flood" />
          <feComposite in="flood" in2="SourceGraphic" operator="over" />
        </filter>
      </defs>
      <g filter="url(#f)">
        <rect x="0" y="0" width="8" height="8" fill="rgb(0, 0, 255)" />
      </g>
    </svg>
  "#;
  assert_svg_filter_matches_resvg(
    svg,
    "f",
    Rect::from_xywh(0.0, 0.0, 8.0, 8.0),
    (8, 8),
    1,
  );
}

#[test]
fn svg_filter_resvg_composite_out_matches_porter_duff_premultiplied() {
  let svg = r#"
    <svg xmlns="http://www.w3.org/2000/svg" width="8" height="8" shape-rendering="crispEdges">
      <defs>
        <filter id="f" x="0" y="0" width="8" height="8" filterUnits="userSpaceOnUse">
          <feFlood flood-color="black" flood-opacity="0.5" result="mask" />
          <feComposite in="SourceGraphic" in2="mask" operator="out" />
        </filter>
      </defs>
      <g filter="url(#f)">
        <rect x="0" y="0" width="8" height="8" fill="rgb(0, 255, 0)" fill-opacity="0.5" />
      </g>
    </svg>
  "#;
  assert_svg_filter_matches_resvg(
    svg,
    "f",
    Rect::from_xywh(0.0, 0.0, 8.0, 8.0),
    (8, 8),
    0,
  );
}

#[test]
fn svg_filter_resvg_morphology_dilate_matches_alpha_and_rgb() {
  let svg = r#"
    <svg xmlns="http://www.w3.org/2000/svg" width="9" height="9" shape-rendering="crispEdges">
      <defs>
        <filter id="fast" x="0" y="0" width="9" height="9" filterUnits="userSpaceOnUse">
          <feMorphology in="SourceGraphic" operator="dilate" radius="1 1" />
        </filter>
        <filter id="ref" x="0" y="0" width="9" height="9" filterUnits="userSpaceOnUse">
          <feConvolveMatrix in="SourceAlpha" order="3 3"
            kernelMatrix="1 1 1 1 1 1 1 1 1"
            divisor="1" edgeMode="duplicate" result="mask" />
          <feFlood flood-color="rgb(255, 0, 0)" result="color" />
          <feComposite in="color" in2="mask" operator="in" />
        </filter>
      </defs>
      <g filter="url(#ref)">
        <!-- A 3px-thick cross. Erosion should shrink it; dilation should expand it. -->
        <rect x="3" y="1" width="3" height="7" fill="rgb(255, 0, 0)" />
        <rect x="1" y="3" width="7" height="3" fill="rgb(255, 0, 0)" />
      </g>
    </svg>
  "#;
  assert_svg_filter_matches_resvg_custom(
    svg,
    svg,
    "ref",
    "fast",
    Rect::from_xywh(1.0, 1.0, 7.0, 7.0),
    (9, 9),
    0,
  );
}

#[test]
fn svg_filter_resvg_morphology_erode_matches_alpha_and_rgb() {
  let svg = r#"
    <svg xmlns="http://www.w3.org/2000/svg" width="9" height="9" shape-rendering="crispEdges">
      <defs>
        <filter id="fast" x="0" y="0" width="9" height="9" filterUnits="userSpaceOnUse">
          <feMorphology in="SourceGraphic" operator="erode" radius="1 1" />
        </filter>
        <filter id="ref" x="0" y="0" width="9" height="9" filterUnits="userSpaceOnUse">
          <feComponentTransfer in="SourceAlpha" result="inv">
            <feFuncA type="table" tableValues="1 0" />
          </feComponentTransfer>
          <feConvolveMatrix in="inv" order="3 3"
            kernelMatrix="1 1 1 1 1 1 1 1 1"
            divisor="1" edgeMode="duplicate" result="dilated" />
          <feComponentTransfer in="dilated" result="mask">
            <feFuncA type="table" tableValues="1 0" />
          </feComponentTransfer>
          <feFlood flood-color="rgb(255, 0, 0)" result="color" />
          <feComposite in="color" in2="mask" operator="in" />
        </filter>
      </defs>
      <g filter="url(#ref)">
        <rect x="3" y="1" width="3" height="7" fill="rgb(255, 0, 0)" />
        <rect x="1" y="3" width="7" height="3" fill="rgb(255, 0, 0)" />
      </g>
    </svg>
  "#;
  assert_svg_filter_matches_resvg_custom(
    svg,
    svg,
    "ref",
    "fast",
    Rect::from_xywh(1.0, 1.0, 7.0, 7.0),
    (9, 9),
    0,
  );
}

#[test]
fn svg_filter_resvg_convolve_matrix_edge_mode_wrap() {
  let svg = r#"
    <svg xmlns="http://www.w3.org/2000/svg" width="5" height="5" shape-rendering="crispEdges">
      <defs>
        <filter id="f" x="0" y="0" width="5" height="5" filterUnits="userSpaceOnUse">
          <feConvolveMatrix order="3 1" kernelMatrix="1 0 0" divisor="1" targetX="1" targetY="0" edgeMode="wrap" />
        </filter>
      </defs>
      <g filter="url(#f)">
        <rect x="0" y="0" width="1" height="5" fill="rgb(255, 0, 0)" />
        <rect x="1" y="0" width="1" height="5" fill="rgb(0, 255, 0)" />
        <rect x="2" y="0" width="1" height="5" fill="rgb(0, 0, 255)" />
        <rect x="3" y="0" width="1" height="5" fill="rgb(255, 255, 0)" />
        <rect x="4" y="0" width="1" height="5" fill="rgb(0, 0, 0)" />
      </g>
    </svg>
  "#;
  assert_svg_filter_matches_resvg(
    svg,
    "f",
    Rect::from_xywh(0.0, 0.0, 5.0, 5.0),
    (5, 5),
    0,
  );
}

#[test]
fn svg_filter_resvg_convolve_matrix_edge_mode_duplicate() {
  let svg = r#"
    <svg xmlns="http://www.w3.org/2000/svg" width="5" height="5" shape-rendering="crispEdges">
      <defs>
        <filter id="f" x="0" y="0" width="5" height="5" filterUnits="userSpaceOnUse">
          <feConvolveMatrix order="3 1" kernelMatrix="1 0 0" divisor="1" targetX="1" targetY="0" edgeMode="duplicate" />
        </filter>
      </defs>
      <g filter="url(#f)">
        <rect x="0" y="0" width="1" height="5" fill="rgb(255, 0, 0)" />
        <rect x="1" y="0" width="1" height="5" fill="rgb(0, 255, 0)" />
        <rect x="2" y="0" width="1" height="5" fill="rgb(0, 0, 255)" />
        <rect x="3" y="0" width="1" height="5" fill="rgb(255, 255, 0)" />
        <rect x="4" y="0" width="1" height="5" fill="rgb(0, 0, 0)" />
      </g>
    </svg>
  "#;
  assert_svg_filter_matches_resvg(
    svg,
    "f",
    Rect::from_xywh(0.0, 0.0, 5.0, 5.0),
    (5, 5),
    0,
  );
}

#[test]
fn svg_filter_resvg_offset_fractional_dx_dy_interpolates() {
  let svg_source = r#"
    <svg xmlns="http://www.w3.org/2000/svg" width="10" height="10">
      <defs>
        <filter id="f" x="0" y="0" width="10" height="10" filterUnits="userSpaceOnUse">
          <feOffset dx="0.5" dy="0.5" />
        </filter>
      </defs>
      <g filter="url(#f)">
        <rect x="4" y="4" width="1" height="1" fill="rgb(255, 0, 0)" shape-rendering="crispEdges" />
      </g>
    </svg>
  "#;
  let svg_expected = r#"
    <svg xmlns="http://www.w3.org/2000/svg" width="10" height="10">
      <rect x="4.5" y="4.5" width="1" height="1" fill="rgb(255, 0, 0)" />
    </svg>
  "#;
  assert_svg_filter_matches_resvg_custom(
    svg_source,
    svg_expected,
    "f",
    "f",
    Rect::from_xywh(4.0, 4.0, 1.0, 1.0),
    (10, 10),
    1,
  );
}
