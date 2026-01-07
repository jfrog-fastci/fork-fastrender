use super::util::create_stacking_context_bounds_renderer;
use tiny_skia::Pixmap;

fn color_at(pixmap: &Pixmap, x: u32, y: u32) -> [u8; 4] {
  let pixel = pixmap.pixel(x, y).expect("pixel");
  [pixel.red(), pixel.green(), pixel.blue(), pixel.alpha()]
}

#[test]
fn stacking_context_layer_bounds_do_not_clip_outline() {
  let mut renderer = create_stacking_context_bounds_renderer();

  let html = r#"
  <style>
    body { margin: 0; background: black; }
    #box {
      position: absolute;
      left: 40px;
      top: 40px;
      width: 20px;
      height: 20px;
      background: blue;
      isolation: isolate;
      outline: 10px solid rgb(255, 0, 0);
      outline-offset: 5px;
    }
  </style>
  <div id="box"></div>
  "#;

  let pixmap = renderer.render_html(html, 120, 120).expect("render");

  let outline_px = color_at(&pixmap, 30, 50);
  assert!(
    outline_px[0] > outline_px[1] && outline_px[0] > outline_px[2] && outline_px[0] > 0,
    "expected outline to paint outside the border box, got {:?}",
    outline_px
  );
}

#[test]
fn stacking_context_outline_em_units_resolve_against_font_size() {
  let mut renderer = create_stacking_context_bounds_renderer();

  let html = r#"
  <style>
    body { margin: 0; background: black; }
    #box {
      position: absolute;
      left: 60px;
      top: 60px;
      width: 20px;
      height: 20px;
      background: blue;
      font-size: 20px;
      isolation: isolate;
      outline: 1em solid rgb(255, 0, 0);
      outline-offset: 0.5em;
    }
  </style>
  <div id="box"></div>
  "#;

  let pixmap = renderer.render_html(html, 120, 120).expect("render");

  // Outline should have width 20px and offset 10px (1em/0.5em with font-size 20px), reaching
  // left to x=30. Sample a point inside the outline stroke but outside the border box/gap.
  let outline_px = color_at(&pixmap, 40, 70);
  assert!(
    outline_px[0] > 200 && outline_px[1] < 50 && outline_px[2] < 50 && outline_px[3] > 200,
    "expected em-based outline to paint, got {:?}",
    outline_px
  );
}
