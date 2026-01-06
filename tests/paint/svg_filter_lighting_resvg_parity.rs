use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine;
use fastrender::geometry::Rect;
use fastrender::image_loader::ImageCache;
use fastrender::image_output::{encode_image, OutputFormat};
use fastrender::paint::svg_filter::{apply_svg_filter, parse_svg_filter_from_svg_document};
use tiny_skia::{Pixmap, PremultipliedColorU8, Transform};

const PIXEL_TOLERANCE: u8 = 3;

fn make_bump_map_pixmap(width: u32, height: u32) -> Pixmap {
  let mut pixmap = Pixmap::new(width, height).expect("pixmap");
  if width == 0 || height == 0 {
    return pixmap;
  }

  let w = width.max(1) as f32;
  let h = height.max(1) as f32;

  // Asymmetric alpha ramp with a smooth bump so the lighting normal varies across the surface.
  // A purely linear gradient would make kernelUnitLength largely irrelevant (finite differences
  // converge exactly), so keep a non-linear component in the height map.
  let bump_cx = w * 0.72;
  let bump_cy = h * 0.28;
  let sigma_x = (w * 0.16).max(1.0);
  let sigma_y = (h * 0.22).max(1.0);

  let denom_x = (width.saturating_sub(1)).max(1) as f32;
  let denom_y = (height.saturating_sub(1)).max(1) as f32;

  for y in 0..height {
    let yf = y as f32 / denom_y;
    for x in 0..width {
      let xf = x as f32 / denom_x;

      let mut alpha = xf * 180.0 + yf * 40.0;
      let dx = x as f32 - bump_cx;
      let dy = y as f32 - bump_cy;
      let bump = 90.0
        * (-((dx * dx) / (2.0 * sigma_x * sigma_x) + (dy * dy) / (2.0 * sigma_y * sigma_y))).exp();
      alpha += bump;

      let a = alpha.round().clamp(0.0, 255.0) as u8;
      let idx = (y as usize * width as usize + x as usize) as usize;
      pixmap.pixels_mut()[idx] = PremultipliedColorU8::from_rgba(0, 0, 0, a).unwrap();
    }
  }

  pixmap
}

fn pixmap_to_data_url_png(pixmap: &Pixmap) -> String {
  let encoded = encode_image(pixmap, OutputFormat::Png).expect("encode bump map png");
  format!("data:image/png;base64,{}", BASE64.encode(encoded))
}

fn render_with_resvg(svg_str: &str, width: u32, height: u32) -> Pixmap {
  let mut options = resvg::usvg::Options::default();
  options.resources_dir = None;

  let tree = resvg::usvg::Tree::from_str(svg_str, &options).expect("parse SVG via resvg");
  let mut pixmap = Pixmap::new(width, height).expect("pixmap");
  let size = tree.size();
  let scale_x = if size.width() > 0.0 {
    width as f32 / size.width()
  } else {
    1.0
  };
  let scale_y = if size.height() > 0.0 {
    height as f32 / size.height()
  } else {
    1.0
  };
  let transform = Transform::from_scale(scale_x, scale_y);
  resvg::render(&tree, transform, &mut pixmap.as_mut());
  pixmap
}

fn render_with_fastrender_filter(
  svg_str: &str,
  filter_id: &str,
  bump_map_pixmap: &Pixmap,
  bbox: Rect,
) -> Pixmap {
  let filter = parse_svg_filter_from_svg_document(svg_str, Some(filter_id), &ImageCache::new())
    .expect("parse filter");
  let mut pixmap = bump_map_pixmap.clone();
  apply_svg_filter(&filter, &mut pixmap, 1.0, bbox).expect("apply filter");
  pixmap
}

fn rgba_at(pixmap: &Pixmap, x: u32, y: u32) -> [u8; 4] {
  let px = pixmap.pixel(x, y).expect("pixel");
  [px.red(), px.green(), px.blue(), px.alpha()]
}

fn assert_rgba_close(case: &str, x: u32, y: u32, actual: [u8; 4], expected: [u8; 4], tol: u8) {
  for ch in 0..4 {
    let diff = actual[ch].abs_diff(expected[ch]);
    assert!(
      diff <= tol,
      "{case} pixel ({x},{y}) channel {ch}: expected {expected:?}, got {actual:?} (diff {diff} > {tol})"
    );
  }
}

fn assert_pixmaps_match_samples_with_tolerance(
  case: &str,
  actual: &Pixmap,
  expected: &Pixmap,
  samples: &[(u32, u32)],
  tol: u8,
) {
  assert_eq!(
    (actual.width(), actual.height()),
    (expected.width(), expected.height()),
    "{case}: pixmap dimensions differ"
  );
  let mut worst: Option<(u32, u32, usize, u8, [u8; 4], [u8; 4])> = None;
  for &(x, y) in samples {
    let a = rgba_at(actual, x, y);
    let b = rgba_at(expected, x, y);
    for ch in 0..4 {
      let diff = a[ch].abs_diff(b[ch]);
      if diff > tol {
        if worst
          .map(|(_, _, _, worst_diff, _, _)| diff > worst_diff)
          .unwrap_or(true)
        {
          worst = Some((x, y, ch, diff, a, b));
        }
      }
    }
  }
  if let Some((x, y, ch, diff, a, b)) = worst {
    assert_rgba_close(case, x, y, a, b, tol);
    panic!("{case} max diff was {diff} at ({x},{y}) channel {ch}");
  }
}

fn assert_pixmaps_match_samples(
  case: &str,
  actual: &Pixmap,
  expected: &Pixmap,
  samples: &[(u32, u32)],
) {
  assert_pixmaps_match_samples_with_tolerance(case, actual, expected, samples, PIXEL_TOLERANCE);
}

fn assert_has_nonzero_pixels(case: &str, pixmap: &Pixmap) {
  let any = pixmap
    .pixels()
    .iter()
    .any(|px| px.red() != 0 || px.green() != 0 || px.blue() != 0 || px.alpha() != 0);
  assert!(any, "{case}: rendered output was fully transparent/black");
}

fn sample_points(width: u32, height: u32) -> Vec<(u32, u32)> {
  let w1 = width.saturating_sub(1);
  let h1 = height.saturating_sub(1);
  if width == 0 || height == 0 {
    return Vec::new();
  }
  let inset_x0 = 1.min(w1);
  let inset_y0 = 1.min(h1);
  let inset_x1 = w1.saturating_sub(1).max(inset_x0);
  let inset_y1 = h1.saturating_sub(1).max(inset_y0);

  let bump_x = ((width as f32 * 0.72).round() as u32).min(w1);
  let bump_y = ((height as f32 * 0.28).round() as u32).min(h1);
  let bump_x = bump_x.clamp(inset_x0, inset_x1);
  let bump_y = bump_y.clamp(inset_y0, inset_y1);

  let mut points = vec![
    (inset_x0, inset_y0),
    (inset_x1, inset_y0),
    (inset_x0, inset_y1),
    (inset_x1, inset_y1),
    (width / 2, height / 2),
    (width / 4, height / 3),
    (width.saturating_mul(3) / 4, height.saturating_mul(2) / 3),
    (bump_x, bump_y),
    ((bump_x + 1).min(inset_x1), bump_y),
    (bump_x, (bump_y + 1).min(inset_y1)),
  ];

  points.sort_unstable();
  points.dedup();
  points
}

fn svg_fixture(width: u32, height: u32, image_href: &str, filter_markup: &str) -> String {
  format!(
    r#"
    <svg xmlns="http://www.w3.org/2000/svg" width="{width}" height="{height}">
      <defs>
        {filter_markup}
      </defs>
      <image x="0" y="0" width="{width}" height="{height}" href="{image_href}" filter="url(#f)" />
    </svg>
    "#
  )
}

#[test]
fn resvg_parity_diffuse_lighting_distant_light() {
  let width = 32;
  let height = 24;
  let bump_map = make_bump_map_pixmap(width, height);
  let bump_map_url = pixmap_to_data_url_png(&bump_map);
  let bbox = Rect::from_xywh(0.0, 0.0, width as f32, height as f32);
  let samples = sample_points(width, height);

  let filter_default = format!(
    r#"
    <filter id="f" filterUnits="userSpaceOnUse" primitiveUnits="userSpaceOnUse"
            x="0" y="0" width="{width}" height="{height}"
            color-interpolation-filters="linearRGB">
      <feDiffuseLighting in="SourceAlpha" surfaceScale="2" diffuseConstant="1"
                         lighting-color="rgb(255,255,255)">
        <feDistantLight azimuth="0" elevation="45" />
      </feDiffuseLighting>
    </filter>
    "#
  );

  let svg_default = svg_fixture(width, height, &bump_map_url, &filter_default);
  let expected_default = render_with_resvg(&svg_default, width, height);
  assert_has_nonzero_pixels("resvg diffuse(default)", &expected_default);
  let actual_default = render_with_fastrender_filter(&svg_default, "f", &bump_map, bbox);
  assert_pixmaps_match_samples(
    "diffuse lighting parity (distant light)",
    &actual_default,
    &expected_default,
    &samples,
  );
}

#[test]
fn resvg_parity_diffuse_lighting_kernel_unit_length() {
  let width = 32;
  let height = 24;
  let bump_map = make_bump_map_pixmap(width, height);
  let bump_map_url = pixmap_to_data_url_png(&bump_map);
  let bbox = Rect::from_xywh(0.0, 0.0, width as f32, height as f32);
  let samples = sample_points(width, height);

  let filter_markup = format!(
    r#"
    <filter id="f" filterUnits="userSpaceOnUse" primitiveUnits="userSpaceOnUse"
            x="0" y="0" width="{width}" height="{height}"
            color-interpolation-filters="linearRGB">
      <feDiffuseLighting in="SourceAlpha" surfaceScale="2" diffuseConstant="1"
                         kernelUnitLength="3 1"
                         lighting-color="rgb(255,255,255)">
        <feDistantLight azimuth="0" elevation="45" />
      </feDiffuseLighting>
    </filter>
    "#
  );

  let svg = svg_fixture(width, height, &bump_map_url, &filter_markup);
  let expected = render_with_resvg(&svg, width, height);
  assert_has_nonzero_pixels("resvg diffuse(kernelUnitLength=3 1)", &expected);
  let actual = render_with_fastrender_filter(&svg, "f", &bump_map, bbox);

  // kernelUnitLength affects how the surface normal samples height deltas near the boundary;
  // allow one extra rounding step of tolerance compared to the default parity samples.
  assert_pixmaps_match_samples_with_tolerance(
    "diffuse lighting parity (kernelUnitLength=3 1)",
    &actual,
    &expected,
    &samples,
    PIXEL_TOLERANCE.saturating_add(1),
  );
}

#[test]
fn resvg_parity_specular_lighting_exponent() {
  let width = 32;
  let height = 24;
  let bump_map = make_bump_map_pixmap(width, height);
  let bump_map_url = pixmap_to_data_url_png(&bump_map);
  let bbox = Rect::from_xywh(0.0, 0.0, width as f32, height as f32);
  let samples = sample_points(width, height);

  let filter_markup = format!(
    r#"
    <filter id="f" filterUnits="userSpaceOnUse" primitiveUnits="userSpaceOnUse"
            x="0" y="0" width="{width}" height="{height}"
            color-interpolation-filters="linearRGB">
      <feSpecularLighting in="SourceAlpha" surfaceScale="2" specularConstant="1" specularExponent="16"
                          lighting-color="rgb(255,255,255)">
        <feDistantLight azimuth="0" elevation="60" />
      </feSpecularLighting>
    </filter>
    "#
  );

  let svg = svg_fixture(width, height, &bump_map_url, &filter_markup);
  let expected = render_with_resvg(&svg, width, height);
  assert_has_nonzero_pixels("resvg specular(exponent=16)", &expected);
  let actual = render_with_fastrender_filter(&svg, "f", &bump_map, bbox);

  assert_pixmaps_match_samples(
    "specular lighting parity (specularExponent=16)",
    &actual,
    &expected,
    &samples,
  );
}

#[test]
fn resvg_parity_point_light() {
  let width = 32;
  let height = 24;
  let bump_map = make_bump_map_pixmap(width, height);
  let bump_map_url = pixmap_to_data_url_png(&bump_map);
  let bbox = Rect::from_xywh(0.0, 0.0, width as f32, height as f32);
  let samples = sample_points(width, height);

  let filter_markup = format!(
    r#"
    <filter id="f" filterUnits="userSpaceOnUse" primitiveUnits="userSpaceOnUse"
            x="0" y="0" width="{width}" height="{height}"
            color-interpolation-filters="linearRGB">
      <feDiffuseLighting in="SourceAlpha" surfaceScale="2" diffuseConstant="1"
                         lighting-color="rgb(255,255,255)">
        <fePointLight x="8" y="6" z="18" />
      </feDiffuseLighting>
    </filter>
    "#
  );

  let svg = svg_fixture(width, height, &bump_map_url, &filter_markup);
  let expected = render_with_resvg(&svg, width, height);
  assert_has_nonzero_pixels("resvg pointLight", &expected);
  let actual = render_with_fastrender_filter(&svg, "f", &bump_map, bbox);

  assert_pixmaps_match_samples(
    "diffuse lighting parity (pointLight)",
    &actual,
    &expected,
    &samples,
  );
}

#[test]
fn resvg_parity_spot_light_cone_and_exponent() {
  let width = 32;
  let height = 24;
  let bump_map = make_bump_map_pixmap(width, height);
  let bump_map_url = pixmap_to_data_url_png(&bump_map);
  let bbox = Rect::from_xywh(0.0, 0.0, width as f32, height as f32);
  let samples = sample_points(width, height);

  let filter_markup = format!(
    r#"
    <filter id="f" filterUnits="userSpaceOnUse" primitiveUnits="userSpaceOnUse"
            x="0" y="0" width="{width}" height="{height}"
            color-interpolation-filters="linearRGB">
      <feDiffuseLighting in="SourceAlpha" surfaceScale="2" diffuseConstant="1"
                         lighting-color="rgb(255,255,255)">
        <feSpotLight x="8" y="4" z="30"
                     pointsAtX="26" pointsAtY="20" pointsAtZ="0"
                     specularExponent="12" limitingConeAngle="35" />
      </feDiffuseLighting>
    </filter>
    "#
  );

  let svg = svg_fixture(width, height, &bump_map_url, &filter_markup);
  let expected = render_with_resvg(&svg, width, height);
  assert_has_nonzero_pixels("resvg spotLight", &expected);
  let actual = render_with_fastrender_filter(&svg, "f", &bump_map, bbox);

  assert_pixmaps_match_samples(
    "diffuse lighting parity (spotLight cone/exponent)",
    &actual,
    &expected,
    &samples,
  );
}

fn make_edge_bump_map_pixmap(width: u32, height: u32) -> Pixmap {
  const BUMP_X: u32 = 10;
  const BUMP_Y: u32 = 12;
  const BUMP_W: u32 = 16;
  const BUMP_H: u32 = 12;

  let mut pixmap = Pixmap::new(width, height).expect("pixmap");
  for y in 0..height {
    for x in 0..width {
      let a = if (BUMP_X..BUMP_X + BUMP_W).contains(&x) && (BUMP_Y..BUMP_Y + BUMP_H).contains(&y) {
        255
      } else {
        0
      };
      let idx = (y as usize * width as usize + x as usize) as usize;
      pixmap.pixels_mut()[idx] = PremultipliedColorU8::from_rgba(0, 0, 0, a).unwrap();
    }
  }
  pixmap
}

#[test]
fn diffuse_lighting_distant_light_azimuth_matches_resvg() {
  const WIDTH: u32 = 64;
  const HEIGHT: u32 = 64;

  const BUMP_X: u32 = 10;
  const BUMP_Y: u32 = 12;
  const BUMP_W: u32 = 16;
  const BUMP_H: u32 = 12;

  let left = (BUMP_X, BUMP_Y + BUMP_H / 2);
  let right = (BUMP_X + BUMP_W - 1, BUMP_Y + BUMP_H / 2);
  let top = (BUMP_X + BUMP_W / 2, BUMP_Y);
  let bottom = (BUMP_X + BUMP_W / 2, BUMP_Y + BUMP_H - 1);

  let bump_map = make_edge_bump_map_pixmap(WIDTH, HEIGHT);
  let bump_map_url = pixmap_to_data_url_png(&bump_map);
  let bbox = Rect::from_xywh(0.0, 0.0, WIDTH as f32, HEIGHT as f32);

  for azimuth in [0, 90, 270] {
    let filter_markup = format!(
      r#"
      <filter id="f" filterUnits="userSpaceOnUse" primitiveUnits="userSpaceOnUse"
              x="0" y="0" width="{WIDTH}" height="{HEIGHT}"
              color-interpolation-filters="linearRGB">
        <feDiffuseLighting in="SourceAlpha" surfaceScale="4" diffuseConstant="1"
                           lighting-color="rgb(255,255,255)">
          <feDistantLight azimuth="{azimuth}" elevation="45" />
        </feDiffuseLighting>
      </filter>
      "#
    );

    let svg = svg_fixture(WIDTH, HEIGHT, &bump_map_url, &filter_markup);
    let expected = render_with_resvg(&svg, WIDTH, HEIGHT);

    // Sanity: ensure azimuth influences the output (i.e., we didn't accidentally set elevation=90).
    // In the SVG coordinate system, +y points down.
    let top_r = rgba_at(&expected, top.0, top.1)[0];
    let bottom_r = rgba_at(&expected, bottom.0, bottom.1)[0];
    match azimuth {
      90 => assert!(
        bottom_r > top_r.saturating_add(10),
        "expected azimuth=90° to light from +y (down), bottom should be brighter than top (top={top_r}, bottom={bottom_r})",
      ),
      270 => assert!(
        top_r > bottom_r.saturating_add(10),
        "expected azimuth=270° to light from -y (up), top should be brighter than bottom (top={top_r}, bottom={bottom_r})",
      ),
      _ => {}
    }
    if azimuth == 0 {
      let left_r = rgba_at(&expected, left.0, left.1)[0];
      let right_r = rgba_at(&expected, right.0, right.1)[0];
      assert!(
        right_r > left_r.saturating_add(10),
        "expected azimuth=0° to light from +x (right), right should be brighter than left (left={left_r}, right={right_r})",
      );
    }

    let actual = render_with_fastrender_filter(&svg, "f", &bump_map, bbox);
    for (label, (x, y)) in [
      ("left-edge", left),
      ("right-edge", right),
      ("top-edge", top),
      ("bottom-edge", bottom),
    ] {
      let expected_px = rgba_at(&expected, x, y);
      let actual_px = rgba_at(&actual, x, y);
      assert_rgba_close(
        &format!("azimuth={azimuth} {label}"),
        x,
        y,
        actual_px,
        expected_px,
        2,
      );
    }
  }
}

#[test]
fn point_light_object_bounding_box_z_scaling_matches_resvg() {
  const WIDTH: u32 = 80;
  const HEIGHT: u32 = 40; // Non-square bbox so any objectBBox scaling is observable.

  let center = (WIDTH / 2, HEIGHT / 2);
  let off_center = (WIDTH * 3 / 4, HEIGHT / 2);

  let svg = format!(
    r#"
    <svg xmlns="http://www.w3.org/2000/svg" width="{WIDTH}" height="{HEIGHT}">
      <defs>
        <filter id="f" filterUnits="userSpaceOnUse" primitiveUnits="objectBoundingBox"
                x="0" y="0" width="{WIDTH}" height="{HEIGHT}"
                color-interpolation-filters="linearRGB">
          <feDiffuseLighting in="SourceAlpha" surfaceScale="0" diffuseConstant="1"
                             lighting-color="rgb(255,255,255)">
            <fePointLight x="{center_x}" y="{center_y}" z="1" />
          </feDiffuseLighting>
        </filter>
      </defs>
      <rect x="0" y="0" width="{WIDTH}" height="{HEIGHT}" fill="black" filter="url(#f)" />
    </svg>
    "#,
    center_x = center.0,
    center_y = center.1
  );

  let expected = render_with_resvg(&svg, WIDTH, HEIGHT);
  assert_ne!(
    rgba_at(&expected, center.0, center.1),
    [0, 0, 0, 255],
    "resvg should apply lighting; got the unfiltered SourceGraphic at the center"
  );

  let filter =
    parse_svg_filter_from_svg_document(&svg, Some("f"), &ImageCache::new()).expect("parse filter");

  let mut source = Pixmap::new(WIDTH, HEIGHT).expect("pixmap");
  source.fill(tiny_skia::Color::from_rgba8(0, 0, 0, 255));

  let bbox = Rect::from_xywh(0.0, 0.0, WIDTH as f32, HEIGHT as f32);
  apply_svg_filter(&filter, &mut source, 1.0, bbox).expect("apply filter");

  for (label, (x, y)) in [("center", center), ("off-center", off_center)] {
    let expected_px = rgba_at(&expected, x, y);
    let actual_px = rgba_at(&source, x, y);
    assert_rgba_close(label, x, y, actual_px, expected_px, 2);
  }
}
