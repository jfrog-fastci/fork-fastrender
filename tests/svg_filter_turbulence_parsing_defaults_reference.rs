use fastrender::geometry::Rect;
use fastrender::image_loader::ImageCache;
use fastrender::paint::svg_filter::{apply_svg_filter, parse_svg_filter_from_svg_document};
use tiny_skia::{Color, Pixmap};

const W: u32 = 32;
const H: u32 = 32;

fn svg_with_turbulence(attrs: &str) -> String {
  let attrs = attrs.trim();
  let attrs = if attrs.is_empty() {
    String::new()
  } else {
    format!(" {attrs}")
  };
  format!(
    r#"<svg xmlns="http://www.w3.org/2000/svg" width="{W}" height="{H}" viewBox="0 0 {W} {H}">
  <defs>
    <filter id="f" filterUnits="userSpaceOnUse" primitiveUnits="userSpaceOnUse" x="0" y="0" width="{W}" height="{H}">
      <feTurbulence{attrs}/>
    </filter>
  </defs>
  <rect x="0" y="0" width="{W}" height="{H}" fill="white" filter="url(#f)"/>
</svg>"#
  )
}

fn render_resvg(svg: &str) -> Pixmap {
  let mut options = resvg::usvg::Options::default();
  options.resources_dir = None;
  let tree = resvg::usvg::Tree::from_str(svg, &options).expect("parse svg with resvg");

  let mut pixmap = resvg::tiny_skia::Pixmap::new(W, H).expect("allocate resvg pixmap");
  resvg::render(&tree, resvg::tiny_skia::Transform::default(), &mut pixmap.as_mut());
  pixmap
}

fn render_fastrender(svg: &str) -> Pixmap {
  let image_cache = ImageCache::new();
  let filter = parse_svg_filter_from_svg_document(svg, Some("f"), &image_cache)
    .expect("parse <filter> with fastrender");
  let mut pixmap = Pixmap::new(W, H).expect("allocate fastrender pixmap");
  pixmap.fill(Color::WHITE);

  let bbox = Rect::from_xywh(0.0, 0.0, W as f32, H as f32);
  apply_svg_filter(filter.as_ref(), &mut pixmap, 1.0, bbox).expect("apply svg filter");
  pixmap
}

fn assert_pixmaps_match(test_case: &str, resvg_pixmap: &Pixmap, fastrender_pixmap: &Pixmap) {
  assert_eq!(
    (resvg_pixmap.width(), resvg_pixmap.height()),
    (fastrender_pixmap.width(), fastrender_pixmap.height()),
    "{test_case}: pixmap dimensions differ"
  );

  if resvg_pixmap.data() == fastrender_pixmap.data() {
    return;
  }

  let width = resvg_pixmap.width();
  let mut max_delta = 0u8;
  let mut max_at = (0u32, 0u32, 'r', 0u8, 0u8);

  for (idx, (&expected, &actual)) in resvg_pixmap
    .data()
    .iter()
    .zip(fastrender_pixmap.data().iter())
    .enumerate()
  {
    let delta = expected.abs_diff(actual);
    if delta <= max_delta {
      continue;
    }

    max_delta = delta;
    let px = (idx / 4) as u32;
    let channel = match idx % 4 {
      0 => 'r',
      1 => 'g',
      2 => 'b',
      3 => 'a',
      _ => unreachable!(),
    };
    max_at = (px % width, px / width, channel, expected, actual);
  }

  assert!(
    max_delta <= 1,
    "{test_case}: resvg vs fastrender mismatch (max channel Δ={max_delta} at ({},{}) channel {} (resvg={} fastrender={}))",
    max_at.0,
    max_at.1,
    max_at.2,
    max_at.3,
    max_at.4
  );
}

#[test]
fn fe_turbulence_missing_base_frequency_matches_resvg_default() {
  let svg = svg_with_turbulence(r#"type="turbulence" seed="2" numOctaves="3" stitchTiles="stitch""#);
  let resvg = render_resvg(&svg);
  let fastrender = render_fastrender(&svg);
  assert_pixmaps_match("missing baseFrequency", &resvg, &fastrender);
}

#[test]
fn fe_turbulence_single_value_base_frequency_matches_resvg_and_expands_to_both_axes() {
  let svg_single = svg_with_turbulence(
    r#"baseFrequency="0.07" type="fractalNoise" seed="9" numOctaves="2" stitchTiles="noStitch""#,
  );
  let svg_double = svg_with_turbulence(
    r#"baseFrequency="0.07 0.07" type="fractalNoise" seed="9" numOctaves="2" stitchTiles="noStitch""#,
  );

  let resvg_single = render_resvg(&svg_single);
  let resvg_double = render_resvg(&svg_double);
  assert_pixmaps_match("single-value baseFrequency expands in resvg", &resvg_double, &resvg_single);

  let fastrender_single = render_fastrender(&svg_single);
  let fastrender_double = render_fastrender(&svg_double);
  assert_pixmaps_match(
    "single-value baseFrequency expands in fastrender",
    &fastrender_double,
    &fastrender_single,
  );

  assert_pixmaps_match(
    "single-value baseFrequency resvg vs fastrender",
    &resvg_single,
    &fastrender_single,
  );
}

#[test]
fn fe_turbulence_missing_type_matches_resvg_default() {
  let svg = svg_with_turbulence(r#"baseFrequency="0.08 0.1" seed="4" numOctaves="2" stitchTiles="stitch""#);
  let resvg = render_resvg(&svg);
  let fastrender = render_fastrender(&svg);
  assert_pixmaps_match("missing type", &resvg, &fastrender);
}

#[test]
fn fe_turbulence_missing_seed_matches_resvg_default() {
  let svg = svg_with_turbulence(r#"baseFrequency="0.08 0.1" type="turbulence" numOctaves="2" stitchTiles="stitch""#);
  let resvg = render_resvg(&svg);
  let fastrender = render_fastrender(&svg);
  assert_pixmaps_match("missing seed", &resvg, &fastrender);
}

#[test]
fn fe_turbulence_missing_num_octaves_matches_resvg_default() {
  let svg = svg_with_turbulence(
    r#"baseFrequency="0.08 0.1" type="turbulence" seed="4" stitchTiles="stitch""#,
  );
  let resvg = render_resvg(&svg);
  let fastrender = render_fastrender(&svg);
  assert_pixmaps_match("missing numOctaves", &resvg, &fastrender);
}

#[test]
fn fe_turbulence_missing_stitch_tiles_matches_resvg_default() {
  let svg = svg_with_turbulence(
    r#"baseFrequency="0.08 0.1" type="turbulence" seed="4" numOctaves="2""#,
  );
  let resvg = render_resvg(&svg);
  let fastrender = render_fastrender(&svg);
  assert_pixmaps_match("missing stitchTiles", &resvg, &fastrender);
}

