use super::util::create_stacking_context_bounds_renderer;
use tiny_skia::Pixmap;

fn render(html: &str, width: u32, height: u32) -> Pixmap {
  let mut renderer = create_stacking_context_bounds_renderer();
  renderer.render_html(html, width, height).expect("render")
}

fn rgba_at(pixmap: &Pixmap, x: u32, y: u32) -> (u8, u8, u8, u8) {
  let p = pixmap.pixel(x, y).expect("pixel");
  (p.red(), p.green(), p.blue(), p.alpha())
}

#[test]
fn box_shadow_blur_falloff_matches_chrome_tail() {
  // Regression for box-shadow blur falloff.
  //
  // Some pages (e.g. Airbnb's header) rely on box-shadow blur tails decaying fast enough that
  // pixels far above the element remain fully white. If our gaussian blur approximation is too
  // diffuse, the shadow darkens those pixels, producing obvious diffs.
  let html = r#"
    <style>
      body { margin: 0; background: white; }
      #wrapper {
        position: absolute;
        left: 512.5px;
        top: 94px;
        transform: translateX(-425px);
      }
      #target {
        width: 850px;
        height: 66px;
        border-radius: 33px;
        background: white;
        box-shadow: 0 8px 24px rgba(0, 0, 0, 0.1);
      }
    </style>
    <div id="wrapper"><div id="target"></div></div>
  "#;

  let pixmap = render(html, 1040, 240);

  // This sample point is 32px above the shadow's top edge (top + offset = 102px). Chrome renders
  // this as fully white; a too-diffuse blur will visibly gray it out.
  assert_eq!(rgba_at(&pixmap, 200, 70), (255, 255, 255, 255));
}
