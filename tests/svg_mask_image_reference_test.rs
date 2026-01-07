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

