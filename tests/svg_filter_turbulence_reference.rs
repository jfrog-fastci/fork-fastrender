use fastrender::geometry::Rect;
use fastrender::image_loader::ImageCache;
use fastrender::paint::svg_filter::{apply_svg_filter, parse_svg_filter_from_svg_document};
use tiny_skia::Pixmap;

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

fn render_svg_resvg(cache: &ImageCache, svg: &str, width: u32, height: u32) -> Pixmap {
  cache
    .render_svg_pixmap_at_size(svg, width, height, "test://svg", 1.0)
    .expect("render svg")
    .as_ref()
    .clone()
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

struct RenderedPair {
  actual: Pixmap,
  expected: Pixmap,
}

fn render_filter_pair(svg: &str, filter_id: &str, bbox_css_px: Rect, viewport: (u32, u32)) -> RenderedPair {
  let cache = ImageCache::new();
  let (viewport_w, viewport_h) = viewport;

  let source_svg = strip_filter_reference(svg, filter_id);
  let source = render_svg_resvg(&cache, &source_svg, viewport_w, viewport_h);

  let filter = parse_svg_filter_from_svg_document(svg, Some(filter_id), &cache)
    .unwrap_or_else(|| panic!("parse_svg_filter_from_svg_document: missing filter #{filter_id}"));
  let mut actual = source.clone();
  apply_svg_filter(filter.as_ref(), &mut actual, 1.0, bbox_css_px).expect("apply_svg_filter");

  let expected = render_svg_resvg(&cache, svg, viewport_w, viewport_h);

  RenderedPair { actual, expected }
}

fn assert_svg_filter_matches_resvg(svg: &str, filter_id: &str, bbox_css_px: Rect, viewport: (u32, u32), tolerance: u8) {
  let pair = render_filter_pair(svg, filter_id, bbox_css_px, viewport);
  assert_pixmaps_match_with_tolerance(&pair.actual, &pair.expected, tolerance);
}

fn user_space_turbulence_svg(
  size: (u32, u32),
  kind: &str,
  base_frequency: &str,
  seed: &str,
  octaves: u32,
  stitch_tiles: bool,
  primitive_region: Option<(u32, u32, u32, u32)>,
) -> String {
  let (w, h) = size;
  let stitch_attr = if stitch_tiles { "stitch" } else { "noStitch" };
  let region_attr = primitive_region
    .map(|(x, y, w, h)| format!(" x=\"{x}\" y=\"{y}\" width=\"{w}\" height=\"{h}\""))
    .unwrap_or_default();
  format!(
    r#"
      <svg xmlns="http://www.w3.org/2000/svg" width="{w}" height="{h}" shape-rendering="crispEdges">
        <defs>
          <filter id="f" x="0" y="0" width="{w}" height="{h}" filterUnits="userSpaceOnUse" primitiveUnits="userSpaceOnUse">
            <feTurbulence type="{kind}" baseFrequency="{base_frequency}" seed="{seed}" numOctaves="{octaves}" stitchTiles="{stitch_attr}"{region_attr} />
          </filter>
        </defs>
        <rect x="0" y="0" width="{w}" height="{h}" fill="white" filter="url(#f)" />
      </svg>
    "#
  )
}

fn object_bbox_turbulence_svg(size: (u32, u32), base_frequency: &str, seed: &str) -> String {
  let (w, h) = size;
  format!(
    r#"
      <svg xmlns="http://www.w3.org/2000/svg" width="{w}" height="{h}" shape-rendering="crispEdges">
        <defs>
          <filter id="f" x="0" y="0" width="1" height="1" filterUnits="objectBoundingBox" primitiveUnits="objectBoundingBox">
            <feTurbulence type="turbulence" baseFrequency="{base_frequency}" seed="{seed}" numOctaves="2" />
          </filter>
        </defs>
        <rect x="0" y="0" width="{w}" height="{h}" fill="white" filter="url(#f)" />
      </svg>
    "#
  )
}

#[test]
fn fe_turbulence_type_matches_resvg() {
  for kind in ["fractalNoise", "turbulence"] {
    let svg = user_space_turbulence_svg((32, 32), kind, "0.12 0.08", "2", 2, false, None);
    assert_svg_filter_matches_resvg(&svg, "f", Rect::from_xywh(0.0, 0.0, 32.0, 32.0), (32, 32), 0);
  }
}

#[test]
fn fe_turbulence_seed_truncation_matches_resvg() {
  for seed in ["1.2", "1.6", "-1.2", "-1.6"] {
    let svg = user_space_turbulence_svg((32, 32), "turbulence", "0.09 0.07", seed, 2, false, None);
    assert_svg_filter_matches_resvg(&svg, "f", Rect::from_xywh(0.0, 0.0, 32.0, 32.0), (32, 32), 0);
  }
}

#[test]
fn fe_turbulence_stitch_tiles_matches_resvg_and_stitches_edges() {
  let svg = user_space_turbulence_svg((32, 32), "turbulence", "0.08 0.1", "7", 2, true, None);
  let pair = render_filter_pair(&svg, "f", Rect::from_xywh(0.0, 0.0, 32.0, 32.0), (32, 32));
  assert_pixmaps_match_with_tolerance(&pair.actual, &pair.expected, 0);

  // Seam checks on FastRender output.
  //
  // `stitchTiles="stitch"` is intended to make the turbulence output tile seamlessly. While the
  // sampled bytes are not necessarily identical between the first/last pixels, large discontinuity
  // at the edges usually indicates that stitchTiles was ignored.
  let w = pair.actual.width();
  let h = pair.actual.height();

  let max_delta = |a: [u8; 4], b: [u8; 4]| -> u8 {
    [
      a[0].abs_diff(b[0]),
      a[1].abs_diff(b[1]),
      a[2].abs_diff(b[2]),
      a[3].abs_diff(b[3]),
    ]
    .into_iter()
    .max()
    .unwrap_or(0)
  };

  let mut max_lr = 0u8;
  let mut max_lr_at = 0u32;
  for y in 0..h {
    let delta = max_delta(rgba_at(&pair.actual, 0, y), rgba_at(&pair.actual, w - 1, y));
    if delta > max_lr {
      max_lr = delta;
      max_lr_at = y;
    }
  }
  let mut max_tb = 0u8;
  let mut max_tb_at = 0u32;
  for x in 0..w {
    let delta = max_delta(rgba_at(&pair.actual, x, 0), rgba_at(&pair.actual, x, h - 1));
    if delta > max_tb {
      max_tb = delta;
      max_tb_at = x;
    }
  }

  const MAX_EDGE_DELTA: u8 = 160;
  assert!(
    max_lr <= MAX_EDGE_DELTA,
    "unexpectedly large left/right edge delta for stitchTiles=\"stitch\": max Δ={max_lr} at y={max_lr_at}"
  );
  assert!(
    max_tb <= MAX_EDGE_DELTA,
    "unexpectedly large top/bottom edge delta for stitchTiles=\"stitch\": max Δ={max_tb} at x={max_tb_at}"
  );
}

#[test]
fn fe_turbulence_object_bounding_box_matches_resvg_at_multiple_sizes() {
  for (w, h) in [(20u32, 12u32), (48u32, 30u32)] {
    let svg = object_bbox_turbulence_svg((w, h), "0.08 0.1", "3");
    assert_svg_filter_matches_resvg(
      &svg,
      "f",
      Rect::from_xywh(0.0, 0.0, w as f32, h as f32),
      (w, h),
      0,
    );
  }
}

#[test]
fn fe_turbulence_primitive_region_offset_matches_resvg() {
  // Ensure noise coordinates are stable in userSpaceOnUse and do not reset to (0,0) at the start
  // of the primitive subregion.
  let svg = user_space_turbulence_svg(
    (32, 32),
    "turbulence",
    "0.11 0.07",
    "5",
    1,
    false,
    Some((7, 9, 18, 15)),
  );
  assert_svg_filter_matches_resvg(&svg, "f", Rect::from_xywh(0.0, 0.0, 32.0, 32.0), (32, 32), 0);
}
