use fastrender::{FastRender, Pixmap, RenderOptions};

fn assert_pixmap_eq(left: &Pixmap, right: &Pixmap) {
  assert_eq!(left.width(), right.width());
  assert_eq!(left.height(), right.height());
  assert_eq!(left.data(), right.data());
}

#[test]
fn prepare_dom_matches_prepare_html_and_render_html() {
  let html = r#"
    <style>
      body { margin: 0; }
      .top { height: 80px; background: rgb(255, 0, 0); }
      .bottom { height: 80px; background: rgb(0, 0, 255); }
    </style>
    <div class="top"></div>
    <div class="bottom"></div>
  "#;

  let mut renderer = FastRender::new().expect("build renderer");
  let dom = renderer.parse_html(html).expect("parse dom");

  // Include scroll to ensure scroll-state wiring matches across entrypoints.
  let options = RenderOptions::new()
    .with_viewport(50, 50)
    .with_scroll(0.0, 80.0);

  let baseline = renderer
    .render_html_with_options(html, options.clone())
    .expect("baseline render_html_with_options");

  let prepared_html = renderer
    .prepare_html(html, options.clone())
    .expect("prepare_html");
  let pixmap_prepared_html = prepared_html.paint_default().expect("paint prepared_html");

  let prepared_dom = renderer
    .prepare_dom(&dom, options)
    .expect("prepare_dom");
  let pixmap_prepared_dom = prepared_dom.paint_default().expect("paint prepared_dom");

  assert_pixmap_eq(&baseline, &pixmap_prepared_html);
  assert_pixmap_eq(&baseline, &pixmap_prepared_dom);
}

