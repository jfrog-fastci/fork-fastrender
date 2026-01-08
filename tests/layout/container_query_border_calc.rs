use fastrender::{
  FastRender, FastRenderConfig, FontConfig, FragmentContent, FragmentNode, Point, Rect,
  RenderArtifactRequest, RenderOptions,
};

fn find_text_fragment_bounds(fragment: &FragmentNode, offset: Point, needle: &str) -> Option<Rect> {
  let abs = Rect::from_xywh(
    fragment.bounds.x() + offset.x,
    fragment.bounds.y() + offset.y,
    fragment.bounds.width(),
    fragment.bounds.height(),
  );

  if let FragmentContent::Text { text, .. } = &fragment.content {
    if text.as_ref().contains(needle) {
      return Some(abs);
    }
  }

  let next_offset = Point::new(abs.x(), abs.y());
  for child in fragment.children.iter() {
    if let Some(found) = find_text_fragment_bounds(child, next_offset, needle) {
      return Some(found);
    }
  }

  None
}

#[test]
fn container_query_resolves_calc_border_widths_in_content_box_size() {
  let config = FastRenderConfig::default().with_font_sources(FontConfig::bundled_only());
  let mut renderer = FastRender::with_config(config).expect("renderer");

  let options = RenderOptions::default().with_viewport(200, 200);

  // Border widths can be `calc()` expressions with font-relative units. The query container below
  // has a 20px left border (1em=10px + 10px) and a 200px border-box width, so the content box is
  // 180px wide. The `min-width: 185px` query should NOT match.
  let html = r#"<!doctype html>
    <html>
      <head>
        <style>
          body { margin: 0; }
          .container {
            width: 200px;
            box-sizing: border-box;
            font-size: 10px;
            border-left: calc(1em + 10px) solid transparent;
            container-type: inline-size;
          }
          .target { display: flex; }
          @container (min-width: 185px) {
            .target { display: block; }
          }
        </style>
      </head>
      <body>
        <div class="container">
          <div class="target">
            <div>AAA</div>
            <div>BBB</div>
          </div>
        </div>
      </body>
    </html>"#;

  let report = renderer
    .render_html_with_stylesheets_report(
      html,
      "https://example.test/",
      options,
      RenderArtifactRequest {
        fragment_tree: true,
        ..RenderArtifactRequest::default()
      },
    )
    .expect("render with container query");

  let fragment_tree = report
    .artifacts
    .fragment_tree
    .as_ref()
    .expect("expected fragment tree artifact");

  let aaa = find_text_fragment_bounds(&fragment_tree.root, Point::ZERO, "AAA")
    .expect("expected text fragment containing AAA");
  let bbb = find_text_fragment_bounds(&fragment_tree.root, Point::ZERO, "BBB")
    .expect("expected text fragment containing BBB");

  assert!(
    bbb.y() <= aaa.y() + 1.0,
    "expected container query to NOT match (BBB should be on the same row as AAA); aaa={:?} bbb={:?}",
    aaa,
    bbb
  );
}

