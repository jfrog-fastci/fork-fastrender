use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine;
use fastrender::style::color::Rgba;
use fastrender::FastRender;
use resvg::tiny_skia::{Pixmap, Transform};

fn color_at(pixmap: &Pixmap, x: u32, y: u32) -> [u8; 4] {
  let pixel = pixmap.pixel(x, y).expect("pixel");
  [pixel.red(), pixel.green(), pixel.blue(), pixel.alpha()]
}

fn render_svg_with_resvg(svg: &str, width: u32, height: u32) -> Pixmap {
  let options = resvg::usvg::Options::default();
  let tree = resvg::usvg::Tree::from_str(svg, &options).expect("parse SVG");
  let mut pixmap = Pixmap::new(width, height).expect("pixmap");
  resvg::render(&tree, Transform::default(), &mut pixmap.as_mut());
  pixmap
}

fn assert_rgba_near(actual: [u8; 4], expected: [u8; 4]) {
  for (channel, (actual, expected)) in actual.iter().zip(expected.iter()).enumerate() {
    let diff = actual.abs_diff(*expected);
    assert!(
      diff <= 1,
      "channel {channel}: expected {expected}±1, got {actual} (diff {diff})"
    );
  }
}

fn unpremultiply_rgba(pixel: [u8; 4]) -> [u8; 4] {
  let [r, g, b, a] = pixel;
  if a == 0 {
    return [0, 0, 0, 0];
  }
  let unpremul = |c: u8| -> u8 { ((c as u32 * 255 + (a as u32 / 2)) / a as u32).min(255) as u8 };
  [unpremul(r), unpremul(g), unpremul(b), a]
}

#[test]
fn filter_url_fragment_uses_inline_svg_filter() {
  let html = r#"
  <style>
    body { margin: 0; }
    #box { width: 20px; height: 20px; background: rgb(255, 0, 0); filter: url(#recolor); }
    svg { position: absolute; width: 0; height: 0; }
  </style>
  <svg width="0" height="0" aria-hidden="true">
    <defs>
      <filter id="recolor">
        <feFlood flood-color="rgb(0, 255, 0)" result="flood" />
        <feComposite in="flood" in2="SourceAlpha" operator="in" />
      </filter>
    </defs>
  </svg>
  <div id="box"></div>
  "#;

  let mut renderer = FastRender::new().expect("renderer");
  let pixmap = renderer.render_html(html, 30, 30).expect("render");

  assert_eq!(color_at(&pixmap, 10, 10), [0, 255, 0, 255]);
}

#[test]
fn filter_url_data_svg_is_applied() {
  let filter_svg = r#"
    <svg xmlns="http://www.w3.org/2000/svg">
      <filter id="recolor">
        <feFlood flood-color="rgb(0, 255, 0)" result="flood" />
        <feComposite in="flood" in2="SourceAlpha" operator="in" />
      </filter>
    </svg>
  "#;
  let encoded = BASE64.encode(filter_svg);
  let data_url = format!("data:image/svg+xml;base64,{}#recolor", encoded);
  let html = format!(
    r#"
    <style>
      body {{ margin: 0; }}
      #box {{ width: 20px; height: 20px; background: rgb(255, 0, 0); filter: url("{}"); }}
    </style>
    <div id="box"></div>
    "#,
    data_url
  );

  let mut renderer = FastRender::new().expect("renderer");
  let pixmap = renderer.render_html(&html, 30, 30).expect("render");

  assert_eq!(color_at(&pixmap, 10, 10), [0, 255, 0, 255]);
}

#[test]
fn filter_url_data_svg_component_transfer_inverts_rgb_and_preserves_alpha() {
  // Match the Wikipedia pageset pattern: a `filter:url(data:image/svg+xml...)` containing an
  // `feComponentTransfer` invert filter (no alpha transfer).
  //
  // Wikipedia sets `color-interpolation-filters="sRGB"` on the `feComponentTransfer` primitive,
  // not on the parent `<filter>` element (whose default is `linearRGB`).
  let data_url = r#"data:image/svg+xml;charset=utf-8,<svg xmlns="http://www.w3.org/2000/svg"><filter id="filter"><feComponentTransfer color-interpolation-filters="sRGB"><feFuncR type="table" tableValues="1 0" /><feFuncG type="table" tableValues="1 0" /><feFuncB type="table" tableValues="1 0" /></feComponentTransfer></filter></svg>#filter"#;

  let html = format!(
    r#"
    <style>
      body {{ margin: 0; }}
      #box1 {{
        width: 20px;
        height: 20px;
        background: rgba(255, 0, 0, 0.5);
        filter: url('{data_url}');
      }}
      #box2 {{
        position: absolute;
        left: 20px;
        top: 0;
        width: 20px;
        height: 20px;
        background: rgba(200, 0, 0, 0.5);
        filter: url('{data_url}');
      }}
    </style>
    <div id="box1"></div>
    <div id="box2"></div>
    "#
  );

  // Use a transparent canvas background so we can assert the filtered alpha.
  let mut renderer = FastRender::builder()
    .background_color(Rgba::TRANSPARENT)
    .build()
    .expect("renderer");
  let pixmap = renderer.render_html(&html, 50, 30).expect("render");

  assert_eq!(color_at(&pixmap, 45, 25), [0, 0, 0, 0]);

  // Red (rgba(255, 0, 0, 0.5)) → Cyan (rgba(0, 255, 255, 0.5)) with alpha preserved.
  //
  // Note: FastRender stores alpha as `u8` via truncation (see `Rgba::alpha_u8`), so `0.5`
  // becomes `127` instead of `128`.
  let box1_px = color_at(&pixmap, 10, 10);
  assert_eq!(box1_px[3], 127);
  assert_eq!(unpremultiply_rgba(box1_px)[..3], [0, 255, 255]);

  // Second sample uses non-extremal input channels so the `color-interpolation-filters="sRGB"`
  // override is observable (linearRGB vs sRGB differs at intermediate values).
  let box2_px = color_at(&pixmap, 30, 10);
  assert_eq!(box2_px[3], 127);
  let box2_unpremul = unpremultiply_rgba(box2_px);
  // rgba(200, 0, 0, 0.5) → rgba(55, 255, 255, 0.5)
  assert_rgba_near(box2_unpremul, [55, 255, 255, 127]);

  // Baseline against resvg for the same filter + rect.
  let svg = r#"
    <svg xmlns="http://www.w3.org/2000/svg" width="50" height="30">
      <filter id="filter">
        <feComponentTransfer color-interpolation-filters="sRGB">
          <feFuncR type="table" tableValues="1 0" />
          <feFuncG type="table" tableValues="1 0" />
          <feFuncB type="table" tableValues="1 0" />
        </feComponentTransfer>
      </filter>
      <rect width="20" height="20" fill="rgb(255, 0, 0)" fill-opacity="0.5" filter="url(#filter)" />
      <rect x="20" width="20" height="20" fill="rgb(200, 0, 0)" fill-opacity="0.5" filter="url(#filter)" />
    </svg>
  "#;
  let baseline = render_svg_with_resvg(svg, 50, 30);
  assert_rgba_near(box1_px, color_at(&baseline, 10, 10));
  assert_rgba_near(box2_px, color_at(&baseline, 30, 10));
}

#[test]
fn filter_url_data_svg_missing_in2_defaults_to_previous_result() {
  let filter_svg = r#"
    <svg xmlns="http://www.w3.org/2000/svg">
      <filter id="blend" x="0" y="0" width="20" height="20" filterUnits="userSpaceOnUse"
              color-interpolation-filters="sRGB">
        <feFlood flood-color="rgb(255,0,0)" result="a" />
        <feFlood flood-color="rgb(0,255,0)" result="b" />
        <!-- Omit `in2`: should default to the previous primitive result ("b"). -->
        <feBlend in="a" mode="difference" />
      </filter>
    </svg>
  "#;
  let encoded = BASE64.encode(filter_svg);
  let data_url = format!("data:image/svg+xml;base64,{}#blend", encoded);
  let html = format!(
    r#"
    <style>
      body {{ margin: 0; background: white; }}
      #box {{ width: 20px; height: 20px; background: rgb(0, 0, 255); filter: url("{}"); }}
    </style>
    <div id="box"></div>
    "#,
    data_url
  );

  let mut renderer = FastRender::new().expect("renderer");
  let pixmap = renderer.render_html(&html, 30, 30).expect("render");

  // difference(red, green) => yellow (if `in2` defaults to previous); difference(red, blue) =>
  // magenta (if `in2` incorrectly defaulted to SourceGraphic).
  assert_eq!(color_at(&pixmap, 10, 10), [255, 255, 0, 255]);
}

#[test]
fn filter_url_data_svg_missing_in2_defaults_to_previous_for_fe_composite() {
  let filter_svg = r#"
    <svg xmlns="http://www.w3.org/2000/svg">
      <filter id="comp" x="0" y="0" width="20" height="20" filterUnits="userSpaceOnUse"
              color-interpolation-filters="sRGB">
        <feFlood flood-color="rgb(255,0,0)" result="a" />
        <feFlood flood-color="rgb(0,0,0)" flood-opacity="0" />
        <!-- Missing `in2` should default to the previous primitive result (transparent flood),
             leaving the red flood unchanged. -->
        <feComposite in="a" operator="out" />
      </filter>
    </svg>
  "#;
  let encoded = BASE64.encode(filter_svg);
  let data_url = format!("data:image/svg+xml;base64,{}#comp", encoded);
  let html = format!(
    r#"
    <style>
      body {{ margin: 0; background: white; }}
      #box {{ width: 20px; height: 20px; background: rgb(0, 0, 255); filter: url("{}"); }}
    </style>
    <div id="box"></div>
    "#,
    data_url
  );

  let mut renderer = FastRender::new().expect("renderer");
  let pixmap = renderer.render_html(&html, 30, 30).expect("render");

  assert_eq!(color_at(&pixmap, 10, 10), [255, 0, 0, 255]);
}

#[test]
fn filter_url_data_svg_missing_in2_defaults_to_previous_for_fe_displacement_map() {
  let filter_svg = r#"
    <svg xmlns="http://www.w3.org/2000/svg">
      <filter id="disp" x="0" y="0" width="20" height="20" filterUnits="userSpaceOnUse"
              primitiveUnits="userSpaceOnUse" color-interpolation-filters="sRGB">
        <!-- Missing `in2` should default to the previous primitive result (black flood), yielding
             dx=dy=-1 (scale=2) and shifting the red pixel from (10,10) to (11,11). -->
        <feFlood flood-color="rgb(0,0,0)" />
        <feDisplacementMap in="SourceGraphic" scale="2" xChannelSelector="R" yChannelSelector="G" />
      </filter>
    </svg>
  "#;
  let encoded = BASE64.encode(filter_svg);
  let data_url = format!("data:image/svg+xml;base64,{}#disp", encoded);
  let html = format!(
    r#"
    <style>
      body {{ margin: 0; background: white; }}
      #box {{ width: 20px; height: 20px; background: white; position: relative; filter: url("{}"); }}
      #dot {{ width: 1px; height: 1px; background: rgb(255, 0, 0); position: absolute; left: 10px; top: 10px; }}
    </style>
    <div id="box"><div id="dot"></div></div>
    "#,
    data_url
  );

  let mut renderer = FastRender::new().expect("renderer");
  let pixmap = renderer.render_html(&html, 30, 30).expect("render");

  assert_eq!(color_at(&pixmap, 11, 11), [255, 0, 0, 255]);
  assert_eq!(color_at(&pixmap, 10, 10), [255, 255, 255, 255]);
  assert_eq!(color_at(&pixmap, 9, 9), [255, 255, 255, 255]);
}

#[test]
fn missing_fragment_filter_is_ignored() {
  let html = r#"
  <style>
    body { margin: 0; }
    #box { width: 20px; height: 20px; background: rgb(255, 0, 0); filter: url(#missing); }
  </style>
  <div id="box"></div>
  "#;

  let mut renderer = FastRender::new().expect("renderer");
  let pixmap = renderer.render_html(html, 30, 30).expect("render");

  assert_eq!(color_at(&pixmap, 10, 10), [255, 0, 0, 255]);
}

#[test]
fn filter_url_missing_in2_defaults_to_previous_result() {
  // resvg treats omitted/empty `in2` the same as a missing `in`: it defaults to the previous
  // primitive result. This integration test exercises the full CSS `filter:url(#...)` pipeline to
  // ensure FastRender keeps that behaviour.
  let html = r#"
  <style>
    body { margin: 0; }
    #box { width: 20px; height: 20px; background: rgb(0, 0, 255); filter: url(#blend); }
    svg { position: absolute; width: 0; height: 0; }
  </style>
  <svg width="0" height="0" aria-hidden="true">
    <defs>
      <filter id="blend" x="0" y="0" width="20" height="20" filterUnits="userSpaceOnUse"
              color-interpolation-filters="sRGB">
        <feFlood flood-color="rgb(255,0,0)" result="a" />
        <feFlood flood-color="rgb(0,255,0)" result="b" />
        <!-- Missing `in2` should default to the previous primitive result ("b"), yielding
             difference(red, green) = yellow. -->
        <feBlend in="a" mode="difference" />
      </filter>
    </defs>
  </svg>
  <div id="box"></div>
  "#;

  let mut renderer = FastRender::new().expect("renderer");
  let pixmap = renderer.render_html(html, 30, 30).expect("render");

  assert_eq!(color_at(&pixmap, 10, 10), [255, 255, 0, 255]);
}

#[test]
fn filter_url_missing_in2_defaults_to_previous_for_fe_composite() {
  let html = r#"
  <style>
    body { margin: 0; background: white; }
    #box { width: 20px; height: 20px; background: rgb(0, 0, 255); filter: url(#comp); }
    svg { position: absolute; width: 0; height: 0; }
  </style>
  <svg width="0" height="0" aria-hidden="true">
    <defs>
      <filter id="comp" x="0" y="0" width="20" height="20" filterUnits="userSpaceOnUse"
              color-interpolation-filters="sRGB">
        <feFlood flood-color="rgb(255,0,0)" result="a" />
        <feFlood flood-color="rgb(0,0,0)" flood-opacity="0" />
        <!-- Missing `in2` should default to the previous primitive result (transparent flood),
             leaving the red flood unchanged. If `in2` incorrectly defaulted to SourceGraphic, the
             `out` operator would erase the red flood because SourceGraphic is opaque. -->
        <feComposite in="a" operator="out" />
      </filter>
    </defs>
  </svg>
  <div id="box"></div>
  "#;

  let mut renderer = FastRender::new().expect("renderer");
  let pixmap = renderer.render_html(html, 30, 30).expect("render");

  assert_eq!(color_at(&pixmap, 10, 10), [255, 0, 0, 255]);
}

#[test]
fn filter_url_missing_in2_defaults_to_previous_for_fe_displacement_map() {
  let html = r#"
  <style>
    body { margin: 0; background: white; }
    #box { width: 20px; height: 20px; background: white; position: relative; filter: url(#disp); }
    #dot { width: 1px; height: 1px; background: rgb(255, 0, 0); position: absolute; left: 10px; top: 10px; }
    svg { position: absolute; width: 0; height: 0; }
  </style>
  <svg width="0" height="0" aria-hidden="true">
    <defs>
      <filter id="disp" x="0" y="0" width="20" height="20" filterUnits="userSpaceOnUse"
              primitiveUnits="userSpaceOnUse" color-interpolation-filters="sRGB">
        <!-- Missing `in2` should default to the previous primitive result (black flood), yielding
             dx=dy=-1 (scale=2) and shifting the red pixel from (10,10) to (11,11). If `in2`
             incorrectly defaulted to SourceGraphic, the mostly-white map would instead shift the
             pixel to (9,9). -->
        <feFlood flood-color="rgb(0,0,0)" />
        <feDisplacementMap in="SourceGraphic" scale="2" xChannelSelector="R" yChannelSelector="G" />
      </filter>
    </defs>
  </svg>
  <div id="box"><div id="dot"></div></div>
  "#;

  let mut renderer = FastRender::new().expect("renderer");
  let pixmap = renderer.render_html(html, 30, 30).expect("render");

  assert_eq!(color_at(&pixmap, 11, 11), [255, 0, 0, 255]);
  assert_eq!(color_at(&pixmap, 10, 10), [255, 255, 255, 255]);
  assert_eq!(color_at(&pixmap, 9, 9), [255, 255, 255, 255]);
}

#[test]
fn filter_url_fragment_can_be_combined_with_css_filter_functions() {
  let html = r#"
  <style>
    body { margin: 0; }
    #box { width: 20px; height: 20px; background: rgb(255, 0, 0); filter: url(#recolor) opacity(0.5); }
    svg { position: absolute; width: 0; height: 0; }
  </style>
  <svg width="0" height="0" aria-hidden="true">
    <defs>
      <filter id="recolor">
        <feFlood flood-color="rgb(0, 255, 0)" result="flood" />
        <feComposite in="flood" in2="SourceAlpha" operator="in" />
      </filter>
    </defs>
  </svg>
  <div id="box"></div>
  "#;

  let mut renderer = FastRender::new().expect("renderer");
  renderer.set_background_color(Rgba::BLACK);
  let pixmap = renderer.render_html(html, 30, 30).expect("render");

  // The URL filter recolors the element to solid green, then opacity(0.5) halves the alpha.
  // Composited over the black background this yields a half-intensity green.
  assert_eq!(color_at(&pixmap, 10, 10), [0, 128, 0, 255]);
}

#[test]
fn filter_url_data_can_be_combined_with_css_filter_functions() {
  let filter_svg = r#"
    <svg xmlns="http://www.w3.org/2000/svg">
      <filter id="recolor">
        <feFlood flood-color="rgb(0, 255, 0)" result="flood" />
        <feComposite in="flood" in2="SourceAlpha" operator="in" />
      </filter>
    </svg>
  "#;
  let encoded = BASE64.encode(filter_svg);
  let data_url = format!("data:image/svg+xml;base64,{}#recolor", encoded);
  let html = format!(
    r#"
    <style>
      body {{ margin: 0; }}
      #box {{ width: 20px; height: 20px; background: rgb(255, 0, 0); filter: url("{}") opacity(0.5); }}
    </style>
    <div id="box"></div>
    "#,
    data_url
  );

  let mut renderer = FastRender::new().expect("renderer");
  renderer.set_background_color(Rgba::BLACK);
  let pixmap = renderer.render_html(&html, 30, 30).expect("render");

  assert_eq!(color_at(&pixmap, 10, 10), [0, 128, 0, 255]);
}
