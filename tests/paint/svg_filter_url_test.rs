use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine;
use fastrender::style::color::Rgba;
use fastrender::FastRender;
use resvg::tiny_skia::Pixmap;

fn color_at(pixmap: &Pixmap, x: u32, y: u32) -> [u8; 4] {
  let pixel = pixmap.pixel(x, y).expect("pixel");
  [pixel.red(), pixel.green(), pixel.blue(), pixel.alpha()]
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
