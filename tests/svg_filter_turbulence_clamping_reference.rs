use fastrender::geometry::Rect;
use fastrender::image_loader::ImageCache;
use fastrender::paint::svg_filter::{apply_svg_filter, parse_svg_filter_from_svg_document};
use resvg::tiny_skia::Pixmap;
use tiny_skia::PremultipliedColorU8;

const WIDTH: u32 = 32;
const HEIGHT: u32 = 32;
const SEED: u32 = 7;

#[test]
fn resvg_render_helper_renders_solid_rect() {
  let svg = format!(
    r#"<svg xmlns="http://www.w3.org/2000/svg" width="{WIDTH}" height="{HEIGHT}" viewBox="0 0 {WIDTH} {HEIGHT}">
  <rect x="0" y="0" width="{WIDTH}" height="{HEIGHT}" fill="rgb(255,0,0)" />
</svg>"#
  );
  let pixmap = render_resvg(&svg);
  assert_eq!(&pixmap.data()[0..4], &[255, 0, 0, 255]);
}

fn turbulence_svg(num_octaves: Option<&str>, base_frequency: Option<&str>) -> String {
  let mut attrs = vec![format!(r#"seed="{SEED}""#)];
  if let Some(value) = num_octaves {
    attrs.push(format!(r#"numOctaves="{value}""#));
  }
  if let Some(value) = base_frequency {
    attrs.push(format!(r#"baseFrequency="{value}""#));
  }

  let attrs = attrs.join(" ");
  format!(
    r#"<svg xmlns="http://www.w3.org/2000/svg" width="{WIDTH}" height="{HEIGHT}" viewBox="0 0 {WIDTH} {HEIGHT}">
  <defs>
    <filter id="f" filterUnits="userSpaceOnUse" primitiveUnits="userSpaceOnUse" x="0" y="0" width="{WIDTH}" height="{HEIGHT}" color-interpolation-filters="sRGB">
      <feTurbulence {attrs} />
    </filter>
  </defs>
  <rect x="0" y="0" width="{WIDTH}" height="{HEIGHT}" fill="white" filter="url(#f)" />
</svg>"#
  )
}

fn render_resvg(svg: &str) -> Pixmap {
  let opt = usvg::Options::default();
  let tree = usvg::Tree::from_str(svg, &opt).expect("parse SVG with usvg");

  let mut pixmap = Pixmap::new(WIDTH, HEIGHT).expect("pixmap");
  let mut pixmap_mut = pixmap.as_mut();
  resvg::render(&tree, usvg::Transform::default(), &mut pixmap_mut);
  pixmap
}

fn render_fastrender(svg: &str) -> Pixmap {
  let cache = ImageCache::new();
  let filter = parse_svg_filter_from_svg_document(svg, Some("f"), &cache).expect("parse filter");

  let mut pixmap = Pixmap::new(WIDTH, HEIGHT).expect("pixmap");
  let white = PremultipliedColorU8::from_rgba(255, 255, 255, 255).expect("color");
  for px in pixmap.pixels_mut() {
    *px = white;
  }

  let bbox = Rect::from_xywh(0.0, 0.0, WIDTH as f32, HEIGHT as f32);
  apply_svg_filter(filter.as_ref(), &mut pixmap, 1.0, bbox).expect("apply filter");
  pixmap
}

fn pixmaps_strict_eq(a: &Pixmap, b: &Pixmap) -> bool {
  a.width() == b.width() && a.height() == b.height() && a.data() == b.data()
}

fn assert_pixmaps_match(context: &str, actual: &Pixmap, expected: &Pixmap) {
  assert_eq!(actual.width(), expected.width(), "{context}: width mismatch");
  assert_eq!(
    actual.height(),
    expected.height(),
    "{context}: height mismatch"
  );

  if actual.data() == expected.data() {
    return;
  }

  let mut max_delta = 0u8;
  let mut max_index = 0usize;
  let mut different_bytes = 0usize;

  for (idx, (&a, &b)) in actual.data().iter().zip(expected.data()).enumerate() {
    let delta = a.abs_diff(b);
    if delta != 0 {
      different_bytes += 1;
    }
    if delta > max_delta {
      max_delta = delta;
      max_index = idx;
    }
  }

  // Matching feTurbulence outputs should be bit-exact, but allow minor rounding differences
  // between engines while still catching any behavioral divergence in clamping/validation.
  let tolerance = 1u8;
  if max_delta <= tolerance {
    return;
  }

  let channels = ['r', 'g', 'b', 'a'];
  let channel = channels[max_index % 4];
  let pixel_index = max_index / 4;
  let x = (pixel_index % actual.width() as usize) as u32;
  let y = (pixel_index / actual.width() as usize) as u32;
  let actual_value = actual.data()[max_index];
  let expected_value = expected.data()[max_index];

  panic!(
    "{context}: pixmaps differ (tolerance={tolerance}), max Δ={max_delta} at ({x},{y}) channel {channel} (actual={actual_value} expected={expected_value}); different bytes={different_bytes}/{}",
    actual.data().len()
  );
}

#[test]
fn turbulence_num_octaves_zero_matches_resvg_and_clamps_consistently() {
  let svg_zero = turbulence_svg(Some("0"), Some("0.12 0.08"));
  let svg_one = turbulence_svg(Some("1"), Some("0.12 0.08"));

  let resvg_zero = render_resvg(&svg_zero);
  let resvg_one = render_resvg(&svg_one);
  let fast_zero = render_fastrender(&svg_zero);
  let fast_one = render_fastrender(&svg_one);

  assert_pixmaps_match("numOctaves=0 FastRender vs resvg", &fast_zero, &resvg_zero);
  assert_pixmaps_match("numOctaves=1 FastRender vs resvg", &fast_one, &resvg_one);

  let resvg_treats_zero_as_one = pixmaps_strict_eq(&resvg_zero, &resvg_one);
  let fast_treats_zero_as_one = pixmaps_strict_eq(&fast_zero, &fast_one);
  assert_eq!(
    fast_treats_zero_as_one, resvg_treats_zero_as_one,
    "numOctaves=0 clamping mismatch (FastRender equal? {fast_treats_zero_as_one}, resvg equal? {resvg_treats_zero_as_one})"
  );
}

#[test]
fn turbulence_num_octaves_large_matches_resvg_and_clamps_consistently() {
  let svg_100 = turbulence_svg(Some("100"), Some("0.12 0.08"));
  let svg_8 = turbulence_svg(Some("8"), Some("0.12 0.08"));
  let svg_16 = turbulence_svg(Some("16"), Some("0.12 0.08"));

  let resvg_100 = render_resvg(&svg_100);
  let resvg_8 = render_resvg(&svg_8);
  let resvg_16 = render_resvg(&svg_16);

  let inferred_clamp = if pixmaps_strict_eq(&resvg_100, &resvg_8) {
    8
  } else if pixmaps_strict_eq(&resvg_100, &resvg_16) {
    16
  } else {
    panic!("resvg numOctaves clamp was not 8 or 16; update the test to include the new behavior");
  };

  let fast_100 = render_fastrender(&svg_100);
  let fast_clamped = match inferred_clamp {
    8 => render_fastrender(&svg_8),
    16 => render_fastrender(&svg_16),
    other => unreachable!("unexpected inferred clamp {other}"),
  };

  assert_pixmaps_match("numOctaves=100 FastRender vs resvg", &fast_100, &resvg_100);
  assert_eq!(
    fast_100.data(),
    fast_clamped.data(),
    "FastRender should clamp numOctaves=100 like resvg (inferred clamp={inferred_clamp})"
  );
}

#[test]
fn turbulence_negative_base_frequency_matches_resvg_and_clamps_consistently() {
  let svg_negative = turbulence_svg(Some("2"), Some("-0.1 -0.08"));
  let svg_zero = turbulence_svg(Some("2"), Some("0 0"));
  let svg_positive = turbulence_svg(Some("2"), Some("0.1 0.08"));

  let resvg_negative = render_resvg(&svg_negative);
  let resvg_zero = render_resvg(&svg_zero);
  let resvg_positive = render_resvg(&svg_positive);

  enum NegativeBehavior {
    ClampToZero,
    Abs,
  }

  let inferred = if pixmaps_strict_eq(&resvg_negative, &resvg_zero) {
    NegativeBehavior::ClampToZero
  } else if pixmaps_strict_eq(&resvg_negative, &resvg_positive) {
    NegativeBehavior::Abs
  } else {
    panic!("resvg baseFrequency negative handling did not match 0 or abs(); update the test to cover the new behavior");
  };

  let fast_negative = render_fastrender(&svg_negative);
  let fast_equivalent = match inferred {
    NegativeBehavior::ClampToZero => render_fastrender(&svg_zero),
    NegativeBehavior::Abs => render_fastrender(&svg_positive),
  };

  assert_pixmaps_match(
    "baseFrequency negative FastRender vs resvg",
    &fast_negative,
    &resvg_negative,
  );
  assert_eq!(
    fast_negative.data(),
    fast_equivalent.data(),
    "FastRender negative baseFrequency handling does not match resvg"
  );
}

#[test]
fn turbulence_nan_base_frequency_matches_resvg_and_falls_back_consistently() {
  let svg_nan = turbulence_svg(Some("2"), Some("NaN"));
  let svg_default = turbulence_svg(Some("2"), None);
  let svg_zero = turbulence_svg(Some("2"), Some("0"));

  let resvg_nan = render_resvg(&svg_nan);
  let resvg_default = render_resvg(&svg_default);
  let resvg_zero = render_resvg(&svg_zero);

  enum NanBehavior {
    Default,
    Zero,
  }

  let inferred = if pixmaps_strict_eq(&resvg_nan, &resvg_default) {
    NanBehavior::Default
  } else if pixmaps_strict_eq(&resvg_nan, &resvg_zero) {
    NanBehavior::Zero
  } else {
    panic!("resvg baseFrequency=NaN handling did not match default or zero; update the test to cover the new behavior");
  };

  let fast_nan = render_fastrender(&svg_nan);
  let fast_equivalent = match inferred {
    NanBehavior::Default => render_fastrender(&svg_default),
    NanBehavior::Zero => render_fastrender(&svg_zero),
  };

  assert_pixmaps_match("baseFrequency=NaN FastRender vs resvg", &fast_nan, &resvg_nan);
  assert_eq!(
    fast_nan.data(),
    fast_equivalent.data(),
    "FastRender baseFrequency=NaN handling does not match resvg"
  );
}
