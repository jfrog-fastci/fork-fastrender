use super::util::create_stacking_context_bounds_renderer;
use tiny_skia::Pixmap;

fn render(html: &str, width: u32, height: u32) -> Pixmap {
  let mut renderer = create_stacking_context_bounds_renderer();
  renderer.render_html(html, width, height).expect("render")
}

fn color_at(pixmap: &Pixmap, x: u32, y: u32) -> [u8; 4] {
  let pixel = pixmap.pixel(x, y).expect("pixel");
  [pixel.red(), pixel.green(), pixel.blue(), pixel.alpha()]
}

#[test]
fn box_shadow_outset_does_not_fill_transparent_interior() {
  let html = r#"
    <style>
      body { margin: 0; background: rgb(0, 0, 0); }
      #target {
        position: absolute;
        left: 30px;
        top: 30px;
        width: 40px;
        height: 40px;
        /* No background; interior should remain transparent. */
        box-shadow: 0 0 0 10px rgb(255, 0, 0);
      }
    </style>
    <div id="target"></div>
  "#;

  let pixmap = render(html, 100, 100);

  let inside = color_at(&pixmap, 50, 50);
  assert_eq!(
    inside,
    [0, 0, 0, 255],
    "expected outset box-shadow not to fill the transparent interior"
  );

  let shadow = color_at(&pixmap, 25, 50);
  assert!(
    shadow[0] > shadow[1] && shadow[0] > shadow[2] && shadow[0] > 0,
    "expected red shadow outside the border box, got {:?}",
    shadow
  );
}

