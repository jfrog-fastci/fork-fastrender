use fastrender::{
  FastRender, FastRenderConfig, FontConfig, FragmentContent, FragmentNode, Rgba,
  RenderArtifactRequest, RenderOptions,
};

fn find_replaced_fragment(fragment: &FragmentNode) -> Option<&FragmentNode> {
  fragment
    .iter_fragments()
    .find(|node| matches!(node.content, FragmentContent::Replaced { .. }))
}

#[test]
fn container_query_refreshes_styles_for_replaced_fragments() {
  let config = FastRenderConfig::default().with_font_sources(FontConfig::bundled_only());
  let mut renderer = FastRender::with_config(config).expect("renderer");

  let options = RenderOptions::default().with_viewport(200, 200);

  // The container query flips the image's border color without changing any layout-affecting
  // properties. The container query pass should therefore reuse the existing layout and refresh
  // fragment styles in place, including for replaced elements.
  let html = r#"<!doctype html>
    <html>
      <head>
        <style>
          body { margin: 0; }
          .container {
            width: 200px;
            container-type: inline-size;
          }
          img {
            display: block;
            width: 10px;
            height: 10px;
            border: 2px solid red;
          }
          @container (min-width: 150px) {
            img { border-left-color: rgb(0, 255, 0); }
          }
        </style>
      </head>
      <body>
        <div class="container">
          <img src=""/>
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

  let img_fragment = find_replaced_fragment(&fragment_tree.root).expect("expected replaced fragment");
  let style = img_fragment
    .style
    .as_ref()
    .expect("expected style on replaced fragment");

  assert_eq!(
    style.border_left_color,
    Rgba::GREEN,
    "expected replaced fragment style to reflect container-query border color"
  );
}

