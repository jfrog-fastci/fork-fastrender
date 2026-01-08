use super::util::create_stacking_context_bounds_renderer;

#[test]
fn root_negative_z_index_children_paint_above_root_background() {
  let mut renderer = create_stacking_context_bounds_renderer();

  let html = r#"<!doctype html>
    <style>
      html, body {
        margin: 0;
        width: 100%;
        height: 100%;
      }
      html { background: rgb(255, 255, 255); }
      body { background: transparent; }
      #neg {
        position: absolute;
        left: 0;
        top: 0;
        width: 40px;
        height: 40px;
        background: rgb(255, 0, 0);
        z-index: -1;
      }
    </style>
    <div id="neg"></div>
  "#;

  let pixmap = renderer.render_html(html, 64, 64).expect("render");

  let px = pixmap.pixel(10, 10).expect("pixel");
  assert!(
    px.red() > 200 && px.green() < 80 && px.blue() < 80 && px.alpha() > 200,
    "expected negative z-index content (red) to paint above the root background, got rgba({}, {}, {}, {})",
    px.red(),
    px.green(),
    px.blue(),
    px.alpha()
  );
}
