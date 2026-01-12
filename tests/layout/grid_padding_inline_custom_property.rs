use fastrender::{
  BoxNode, BoxTree, FastRender, FastRenderConfig, FontConfig, FragmentContent, FragmentNode, Point,
  Rect, RenderArtifactRequest, RenderOptions,
};

fn find_box_id_by_dom_id(node: &BoxNode, id: &str) -> Option<usize> {
  if let Some(debug) = node.debug_info.as_ref() {
    if debug.id.as_deref() == Some(id) {
      return Some(node.id);
    }
  }
  for child in node.children.iter() {
    if let Some(found) = find_box_id_by_dom_id(child, id) {
      return Some(found);
    }
  }
  if let Some(body) = node.footnote_body.as_deref() {
    if let Some(found) = find_box_id_by_dom_id(body, id) {
      return Some(found);
    }
  }
  None
}

fn find_fragment_bounds_by_box_id(
  fragment: &FragmentNode,
  offset: Point,
  box_id: usize,
) -> Option<Rect> {
  let abs = Rect::from_xywh(
    fragment.bounds.x() + offset.x,
    fragment.bounds.y() + offset.y,
    fragment.bounds.width(),
    fragment.bounds.height(),
  );

  let matches = match &fragment.content {
    FragmentContent::Block { box_id: Some(id) }
    | FragmentContent::Inline {
      box_id: Some(id), ..
    }
    | FragmentContent::Text {
      box_id: Some(id), ..
    }
    | FragmentContent::Replaced {
      box_id: Some(id), ..
    } => *id == box_id,
    _ => false,
  };
  if matches {
    return Some(abs);
  }

  let next_offset = Point::new(abs.x(), abs.y());
  for child in fragment.children.iter() {
    if let Some(found) = find_fragment_bounds_by_box_id(child, next_offset, box_id) {
      return Some(found);
    }
  }

  None
}

#[test]
fn grid_padding_inline_custom_property_resolves_against_viewport() {
  let config = FastRenderConfig::default().with_font_sources(FontConfig::bundled_only());
  let mut renderer = FastRender::with_config(config).expect("renderer");

  // Match the MDN pageset viewport width; `--layout-side-padding` should resolve to 1rem (=16px).
  let options = RenderOptions::default().with_viewport(1040, 200);

  let html = r#"<!doctype html>
    <html>
      <head>
        <style>
          html { font-size: 16px; }
          :root {
            --layout-side-padding-min: 1rem;
            --layout-side-padding: max(var(--layout-side-padding-min), calc(50vw - 720px + 1rem));
          }
          * { box-sizing: border-box; }
          body { margin: 0; }
          .navigation {
            display: grid;
            grid-template-columns: min-content 1fr min-content min-content;
            align-items: center;
            justify-items: center;
            column-gap: 1rem;
            height: 4.125rem;
            padding-block: 0.75rem;
            padding-inline: var(--layout-side-padding);
          }
          .navigation__logo { margin-inline-start: -6px; }
          .logo { display: block; padding: 0.5rem; }
          .logo__image { display: block; width: 83px; height: 24px; background: black; }
          .navigation__menu { width: 730px; height: 37px; background: rgba(0, 0, 0, 0.1); }
          .navigation__search { width: 80px; height: 34px; background: rgba(0, 255, 0, 0.1); }
          .user-menu { width: 34px; height: 34px; background: rgba(255, 0, 0, 0.1); }
        </style>
      </head>
      <body>
        <nav class="navigation">
          <div class="navigation__logo">
            <a class="logo">
              <svg class="logo__image" id="image" width="83" height="24"></svg>
            </a>
          </div>
          <div class="navigation__menu"></div>
          <div class="navigation__search"></div>
          <div class="user-menu"></div>
        </nav>
      </body>
    </html>"#;

  let report = renderer
    .render_html_with_stylesheets_report(
      html,
      "https://example.test/",
      options,
      RenderArtifactRequest {
        box_tree: true,
        fragment_tree: true,
        ..RenderArtifactRequest::default()
      },
    )
    .expect("render");

  let box_tree: &BoxTree = report.artifacts.box_tree.as_ref().expect("box tree");
  let fragment_tree = report
    .artifacts
    .fragment_tree
    .as_ref()
    .expect("fragment tree artifact");

  let image_box_id = find_box_id_by_dom_id(&box_tree.root, "image").expect("logo image box");
  let image_bounds = find_fragment_bounds_by_box_id(&fragment_tree.root, Point::ZERO, image_box_id)
    .expect("image fragment");

  // Expected x = 16px (layout-side-padding) - 6px (navigation__logo margin) + 8px (logo padding).
  assert!(
    (image_bounds.x() - 18.0).abs() < 0.5,
    "expected logo image x≈18px, got {:?}",
    image_bounds
  );
}
