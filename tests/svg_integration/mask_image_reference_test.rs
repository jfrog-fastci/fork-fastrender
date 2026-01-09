use fastrender::FastRender;
use resvg::tiny_skia::Pixmap;

fn pixel(pixmap: &Pixmap, x: u32, y: u32) -> [u8; 4] {
  let p = pixmap.pixel(x, y).expect("pixel");
  [p.red(), p.green(), p.blue(), p.alpha()]
}

#[test]
fn mask_image_url_fragment_uses_computed_svg_presentation_styles() {
  let html = r#"
    <style>
      html, body { margin: 0; padding: 0; background: white; }
      svg { position: absolute; width: 0; height: 0; }
      .cut { fill: black; }
      .keep { fill: white; }
      #box {
        width: 50px;
        height: 20px;
        background: rgb(255, 0, 0);
        mask-image: url(#m);
        mask-mode: alpha;
        mask-size: 100% 100%;
        mask-repeat: no-repeat;
        mask-position: 0 0;
      }
    </style>
    <svg xmlns="http://www.w3.org/2000/svg" width="0" height="0" aria-hidden="true">
      <defs>
        <mask id="m" maskUnits="userSpaceOnUse" maskContentUnits="userSpaceOnUse" x="0" y="0" width="50" height="20">
          <rect class="cut" width="25" height="20"></rect>
          <rect class="keep" x="25" width="25" height="20"></rect>
        </mask>
      </defs>
    </svg>
    <div id="box"></div>
  "#;

  let mut renderer = FastRender::new().expect("renderer");
  let pixmap = renderer.render_html(html, 60, 30).expect("render");

  // Left half is masked out by `.cut { fill: black }`, so it should show the white background.
  assert_eq!(pixel(&pixmap, 10, 10), [255, 255, 255, 255]);
  // Right half is masked in by `.keep { fill: white }`, so it should show the red box.
  assert_eq!(pixel(&pixmap, 40, 10), [255, 0, 0, 255]);
}

#[test]
fn mask_image_url_fragment_preserves_styles_for_defs_referenced_via_use() {
  let html = r##"
    <style>
      html, body { margin: 0; padding: 0; background: white; }
      svg { position: absolute; width: 0; height: 0; }
      .cut { fill: black; }
      .keep { fill: white; }
      #box {
        width: 50px;
        height: 20px;
        background: rgb(255, 0, 0);
        mask-image: url(#m);
        mask-mode: alpha;
        mask-size: 100% 100%;
        mask-repeat: no-repeat;
        mask-position: 0 0;
      }
    </style>
    <svg xmlns="http://www.w3.org/2000/svg" width="0" height="0" aria-hidden="true">
      <defs>
        <rect id="r" class="cut" width="25" height="20"></rect>
        <rect id="s" class="keep" x="25" width="25" height="20"></rect>
        <mask id="m" maskUnits="userSpaceOnUse" maskContentUnits="userSpaceOnUse" x="0" y="0" width="50" height="20">
          <use href="#r"></use>
          <use href="#s"></use>
        </mask>
      </defs>
    </svg>
    <div id="box"></div>
  "##;

  let mut renderer = FastRender::new().expect("renderer");
  let pixmap = renderer.render_html(html, 60, 30).expect("render");

  // `.cut { fill: black }` should still be applied even though the masked geometry is defined
  // inside `<defs>` and referenced by `<use>`.
  assert_eq!(pixel(&pixmap, 10, 10), [255, 255, 255, 255]);
  assert_eq!(pixel(&pixmap, 40, 10), [255, 0, 0, 255]);
}

#[test]
fn mask_image_url_fragment_inlines_svg_opacity_from_css() {
  let html = r#"
    <style>
      html, body { margin: 0; padding: 0; background: black; }
      svg { position: absolute; width: 0; height: 0; }
      .semi { opacity: 0.5; fill: white; }
      #box {
        position: absolute;
        left: 0;
        top: 0;
        width: 20px;
        height: 20px;
        background: rgb(255, 0, 0);
        mask-image: url(#m);
        mask-mode: alpha;
        mask-size: 100% 100%;
        mask-repeat: no-repeat;
        mask-position: 0 0;
      }
    </style>
    <svg xmlns="http://www.w3.org/2000/svg" width="0" height="0" aria-hidden="true">
      <defs>
        <mask id="m" maskUnits="userSpaceOnUse" maskContentUnits="userSpaceOnUse" x="0" y="0" width="20" height="20">
          <rect class="semi" width="20" height="20"></rect>
        </mask>
      </defs>
    </svg>
    <div id="box"></div>
  "#;

  let mut renderer = FastRender::new().expect("renderer");
  let pixmap = renderer.render_html(html, 30, 30).expect("render");

  assert_eq!(pixel(&pixmap, 25, 10), [0, 0, 0, 255], "background pixel");
  let masked = pixel(&pixmap, 10, 10);
  assert_eq!(masked[1], 0, "masked green");
  assert_eq!(masked[2], 0, "masked blue");
  assert_eq!(masked[3], 255, "masked alpha");
  assert!(
    (120..=136).contains(&masked[0]),
    "expected ~50% red over black background, got {masked:?}"
  );
}

#[test]
fn mask_image_url_fragment_inlines_svg_transform_from_css() {
  let html = r#"
    <style>
      html, body { margin: 0; padding: 0; background: white; }
      svg { position: absolute; width: 0; height: 0; }
      .cut { fill: black; }
      .keep { fill: white; transform: translateX(25px); }
      #box {
        width: 50px;
        height: 20px;
        background: rgb(255, 0, 0);
        mask-image: url(#m);
        mask-mode: alpha;
        mask-size: 100% 100%;
        mask-repeat: no-repeat;
        mask-position: 0 0;
      }
    </style>
    <svg xmlns="http://www.w3.org/2000/svg" width="0" height="0" aria-hidden="true">
      <defs>
        <mask id="m" maskUnits="userSpaceOnUse" maskContentUnits="userSpaceOnUse"
              x="0" y="0" width="50" height="20">
          <rect class="cut" width="25" height="20"></rect>
          <rect class="keep" width="25" height="20"></rect>
        </mask>
      </defs>
    </svg>
    <div id="box"></div>
  "#;

  let mut renderer = FastRender::new().expect("renderer");
  let pixmap = renderer.render_html(html, 60, 30).expect("render");

  // `.keep` is shifted into the right half via `transform`, so the left side should be masked out.
  assert_eq!(pixel(&pixmap, 10, 10), [255, 255, 255, 255]);
  assert_eq!(pixel(&pixmap, 40, 10), [255, 0, 0, 255]);
}

#[test]
fn mask_image_url_fragment_resolves_use_targets_outside_defs() {
  let html = r##"
    <style>
      html, body { margin: 0; padding: 0; background: white; }
      svg { position: absolute; width: 0; height: 0; }
      #box {
        width: 50px;
        height: 20px;
        background: rgb(255, 0, 0);
        mask-image: url(#m);
        mask-mode: alpha;
        mask-size: 100% 100%;
        mask-repeat: no-repeat;
        mask-position: 0 0;
      }
    </style>
    <svg xmlns="http://www.w3.org/2000/svg" width="0" height="0" aria-hidden="true">
      <rect id="r" width="25" height="20" fill="white"></rect>
      <defs>
        <mask id="m" maskUnits="userSpaceOnUse" maskContentUnits="userSpaceOnUse" x="0" y="0" width="50" height="20">
          <use href="#r"></use>
        </mask>
      </defs>
    </svg>
    <div id="box"></div>
  "##;

  let mut renderer = FastRender::new().expect("renderer");
  let pixmap = renderer.render_html(html, 60, 30).expect("render");

  assert_eq!(pixel(&pixmap, 10, 10), [255, 0, 0, 255]);
  assert_eq!(pixel(&pixmap, 40, 10), [255, 255, 255, 255]);
}

mod paint {
  // Keep the bulk of SVG mask reference coverage in the deterministic display-list paint pipeline.
  include!("../paint/svg_mask_image_reference_test.rs");
}
