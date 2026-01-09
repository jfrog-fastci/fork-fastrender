use fastrender::image_loader::ImageCache;
use fastrender::paint::svg_filter::{apply_svg_filter, parse_svg_filter_from_svg_document};
use fastrender::Rect;
use resvg::tiny_skia::{Pixmap, PremultipliedColorU8, Transform};

const WIDTH: u32 = 4;
const HEIGHT: u32 = 4;

fn svg_turbulence(
  filter_cif: Option<&str>,
  primitive_cif: Option<&str>,
  base_frequency: &str,
  num_octaves: u32,
  seed: u32,
  kind: &str,
) -> String {
  let filter_cif_attr = filter_cif
    .map(|v| format!(" color-interpolation-filters=\"{v}\""))
    .unwrap_or_default();
  let primitive_cif_attr = primitive_cif
    .map(|v| format!(" color-interpolation-filters=\"{v}\""))
    .unwrap_or_default();

  format!(
    r##"<svg xmlns="http://www.w3.org/2000/svg" width="{WIDTH}" height="{HEIGHT}" viewBox="0 0 {WIDTH} {HEIGHT}">
  <filter id="f" filterUnits="userSpaceOnUse" primitiveUnits="userSpaceOnUse" x="0" y="0" width="{WIDTH}" height="{HEIGHT}"{filter_cif_attr}>
    <feTurbulence baseFrequency="{base_frequency}" numOctaves="{num_octaves}" seed="{seed}" type="{kind}"{primitive_cif_attr}/>
  </filter>
  <rect x="0" y="0" width="{WIDTH}" height="{HEIGHT}" fill="#fff" filter="url(#f)"/>
</svg>"##,
    WIDTH = WIDTH,
    HEIGHT = HEIGHT,
    filter_cif_attr = filter_cif_attr,
    primitive_cif_attr = primitive_cif_attr,
    base_frequency = base_frequency,
    num_octaves = num_octaves,
    seed = seed,
    kind = kind,
  )
}

fn render_with_resvg(svg: &str) -> Pixmap {
  let mut options = resvg::usvg::Options::default();
  options.resources_dir = None;
  let tree =
    resvg::usvg::Tree::from_str(svg, &options).expect("resvg failed to parse test SVG markup");

  let mut pixmap = Pixmap::new(WIDTH, HEIGHT).expect("failed to allocate pixmap");
  resvg::render(&tree, Transform::default(), &mut pixmap.as_mut());
  pixmap
}

fn render_with_fastrender(svg: &str) -> Pixmap {
  let image_cache = ImageCache::new();
  let filter = parse_svg_filter_from_svg_document(svg, Some("f"), &image_cache)
    .expect("failed to parse filter with FastRender");

  let mut pixmap = Pixmap::new(WIDTH, HEIGHT).expect("failed to allocate pixmap");
  let white = PremultipliedColorU8::from_rgba(255, 255, 255, 255).unwrap();
  for px in pixmap.pixels_mut() {
    *px = white;
  }

  let bbox = Rect::from_xywh(0.0, 0.0, WIDTH as f32, HEIGHT as f32);
  apply_svg_filter(filter.as_ref(), &mut pixmap, 1.0, bbox).unwrap();
  pixmap
}

fn channel_name(channel: usize) -> char {
  match channel {
    0 => 'r',
    1 => 'g',
    2 => 'b',
    3 => 'a',
    _ => '?',
  }
}

fn max_channel_delta(a: &Pixmap, b: &Pixmap) -> (u8, usize, usize, char, u8, u8) {
  assert_eq!((a.width(), a.height()), (b.width(), b.height()));
  let width = a.width() as usize;
  let height = a.height() as usize;

  let mut max_delta = 0u8;
  let mut max_at = (0usize, 0usize, 'r', 0u8, 0u8);

  let a_data = a.data();
  let b_data = b.data();
  for y in 0..height {
    for x in 0..width {
      let idx = (y * width + x) * 4;
      for channel in 0..4 {
        let av = a_data[idx + channel];
        let bv = b_data[idx + channel];
        let delta = av.abs_diff(bv);
        if delta > max_delta {
          max_delta = delta;
          max_at = (x, y, channel_name(channel), av, bv);
        }
      }
    }
  }

  (max_delta, max_at.0, max_at.1, max_at.2, max_at.3, max_at.4)
}

fn assert_pixmap_matches_reference(case: &str, reference: &Pixmap, actual: &Pixmap) {
  assert_eq!(
    (reference.width(), reference.height()),
    (actual.width(), actual.height()),
    "{case}: pixmap dimensions differ"
  );

  if reference.data() == actual.data() {
    return;
  }

  let (max_delta, x, y, channel, expected, got) = max_channel_delta(reference, actual);
  assert!(
    max_delta <= 1,
    "{case}: pixmap bytes differ (max channel Δ={max_delta} at ({x},{y}) channel {channel} expected={expected} got={got})"
  );
}

fn assert_pixmaps_differ(case: &str, a: &Pixmap, b: &Pixmap) {
  let (max_delta, x, y, channel, av, bv) = max_channel_delta(a, b);
  assert!(
    max_delta > 1,
    "{case}: expected outputs to differ, but max channel Δ={max_delta} at ({x},{y}) channel {channel} (a={av} b={bv})"
  );
}

#[test]
fn turbulence_respects_color_interpolation_filters_reference_base_frequency_zero() {
  let svg_default = svg_turbulence(None, None, "0", 1, 0, "fractalNoise");
  let svg_srgb = svg_turbulence(Some("sRGB"), None, "0", 1, 0, "fractalNoise");

  let resvg_default = render_with_resvg(&svg_default);
  let fastr_default = render_with_fastrender(&svg_default);
  assert_pixmap_matches_reference(
    "baseFrequency=0 default CIF (linearRGB) vs resvg",
    &resvg_default,
    &fastr_default,
  );

  let resvg_srgb = render_with_resvg(&svg_srgb);
  let fastr_srgb = render_with_fastrender(&svg_srgb);
  assert_pixmap_matches_reference(
    "baseFrequency=0 CIF=sRGB vs resvg",
    &resvg_srgb,
    &fastr_srgb,
  );

  assert_pixmaps_differ(
    "baseFrequency=0 resvg output should differ between default CIF and CIF=sRGB",
    &resvg_default,
    &resvg_srgb,
  );
  assert_pixmaps_differ(
    "baseFrequency=0 FastRender output should differ between default CIF and CIF=sRGB",
    &fastr_default,
    &fastr_srgb,
  );
}

#[test]
fn turbulence_respects_primitive_color_interpolation_filters_override_reference() {
  let svg_override = svg_turbulence(Some("linearRGB"), Some("sRGB"), "0", 1, 0, "fractalNoise");

  let resvg = render_with_resvg(&svg_override);
  let fastr = render_with_fastrender(&svg_override);
  assert_pixmap_matches_reference(
    "primitive-level CIF override (filter linearRGB, feTurbulence sRGB) vs resvg",
    &resvg,
    &fastr,
  );
}

#[test]
fn turbulence_respects_color_interpolation_filters_reference_non_zero_base_frequency() {
  let svg_default = svg_turbulence(None, None, "0.1 0.08", 2, 2, "turbulence");
  let svg_srgb = svg_turbulence(Some("sRGB"), None, "0.1 0.08", 2, 2, "turbulence");

  let resvg_default = render_with_resvg(&svg_default);
  let fastr_default = render_with_fastrender(&svg_default);
  assert_pixmap_matches_reference(
    "baseFrequency=0.1 0.08 default CIF (linearRGB) vs resvg",
    &resvg_default,
    &fastr_default,
  );

  let resvg_srgb = render_with_resvg(&svg_srgb);
  let fastr_srgb = render_with_fastrender(&svg_srgb);
  assert_pixmap_matches_reference(
    "baseFrequency=0.1 0.08 CIF=sRGB vs resvg",
    &resvg_srgb,
    &fastr_srgb,
  );
}
