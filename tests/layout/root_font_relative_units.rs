use fastrender::debug::runtime::RuntimeToggles;
use fastrender::dom::DomNodeType;
use fastrender::style::cascade::StyledNode;
use fastrender::style::media::MediaType;
use fastrender::{
  BoxNode, FastRender, FastRenderConfig, FontConfig, FragmentContent, FragmentNode, Point, Rect,
  RenderArtifactRequest, RenderOptions,
};
use std::collections::HashMap;
use url::Url;

fn find_styled_node_id_for_dom_id(node: &StyledNode, id_value: &str) -> Option<usize> {
  if let DomNodeType::Element { attributes, .. } = &node.node.node_type {
    if attributes
      .iter()
      .any(|(k, v)| k.eq_ignore_ascii_case("id") && v == id_value)
    {
      return Some(node.node_id);
    }
  }

  for child in node.children.iter() {
    if let Some(found) = find_styled_node_id_for_dom_id(child, id_value) {
      return Some(found);
    }
  }

  None
}

fn find_box_id_for_styled_node_id(node: &BoxNode, styled_node_id: usize) -> Option<usize> {
  if node.generated_pseudo.is_none() && node.styled_node_id == Some(styled_node_id) {
    return Some(node.id);
  }
  for child in node.children.iter() {
    if let Some(found) = find_box_id_for_styled_node_id(child, styled_node_id) {
      return Some(found);
    }
  }
  if let Some(footnote_body) = node.footnote_body.as_deref() {
    if let Some(found) = find_box_id_for_styled_node_id(footnote_body, styled_node_id) {
      return Some(found);
    }
  }
  None
}

fn find_fragment_width_for_box_id(node: &FragmentNode, box_id: usize) -> Option<f32> {
  let matches_box = match &node.content {
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
  if matches_box {
    return Some(node.bounds.width());
  }

  for child in node.children.iter() {
    if let Some(found) = find_fragment_width_for_box_id(child, box_id) {
      return Some(found);
    }
  }

  None
}

fn element_width_with_config(
  html: &str,
  viewport: (u32, u32),
  element_id: &str,
  config: FastRenderConfig,
) -> f32 {
  let mut renderer = FastRender::with_config(config).expect("renderer");
  let dom = renderer.parse_html(html).expect("parse");
  let intermediates = renderer
    .layout_document_for_media_intermediates(&dom, viewport.0, viewport.1, MediaType::Screen)
    .expect("layout intermediates");

  let styled_node_id =
    find_styled_node_id_for_dom_id(&intermediates.styled_tree, element_id).expect("styled id");
  let box_id =
    find_box_id_for_styled_node_id(&intermediates.box_tree.root, styled_node_id).expect("box id");
  find_fragment_width_for_box_id(&intermediates.fragment_tree.root, box_id).expect("fragment width")
}

fn element_width(html: &str, viewport: (u32, u32), element_id: &str) -> f32 {
  element_width_with_config(
    html,
    viewport,
    element_id,
    FastRenderConfig::default().with_font_sources(FontConfig::bundled_only()),
  )
}

fn assert_close(actual: f32, expected: f32, label: &str) {
  let delta = (actual - expected).abs();
  assert!(
    delta < 0.2,
    "{label}: expected {expected:.2} got {actual:.2} (delta {delta:.2})"
  );
}

#[test]
fn rlh_resolves_using_root_computed_line_height() {
  let html = r#"<!doctype html>
    <html>
      <head>
        <style>
          html { font-size: 10px; line-height: 2; }
          body { margin: 0; }
          #box { width: 1rlh; height: 1px; }
        </style>
      </head>
      <body>
        <div id="box"></div>
      </body>
    </html>"#;

  let width = element_width(html, (200, 100), "box");
  assert_close(width, 20.0, "1rlh should equal root line-height");
}

#[test]
fn rcap_uses_root_font_cap_height_metrics() {
  let font_path = std::path::Path::new("tests/fixtures/fonts/mvar-metrics-test.ttf");
  let abs = std::fs::canonicalize(font_path).expect("canonicalize font fixture");
  let font_url = Url::from_file_path(&abs)
    .map_err(|()| ())
    .expect("file url for font fixture")
    .to_string();

  let html = format!(
    r#"<!doctype html>
    <html>
      <head>
        <style>
          @font-face {{
            font-family: MvarMetricsTest;
            src: url("{font_url}");
            font-display: swap;
            font-weight: 100 900;
          }}
          html, body {{ margin: 0; }}
          html {{
            font-family: MvarMetricsTest, sans-serif;
            font-size: 20px;
            font-weight: 900;
          }}
          #box {{ width: 1rcap; height: 1px; }}
        </style>
      </head>
      <body>
        <div id="box"></div>
      </body>
    </html>"#
  );

  let toggles = RuntimeToggles::from_map(HashMap::from([(
    "FASTR_WEB_FONT_WAIT_MS".to_string(),
    "500".to_string(),
  )]));
  let config = FastRenderConfig::default()
    .with_font_sources(FontConfig::bundled_only())
    .with_runtime_toggles(toggles);
  let width = element_width_with_config(&html, (200, 100), "box", config);

  // CAP_HEIGHT = 760 (wght=900) / 1000 * 20px = 15.2px
  assert_close(width, 15.2, "1rcap should use root cap-height metric");
}

#[test]
fn registered_custom_property_length_uses_root_metrics_for_rlh() {
  let html = r#"<!doctype html>
    <html>
      <head>
        <style>
          @property --w {
            syntax: "<length>";
            inherits: false;
            initial-value: 0px;
          }
          html { font-size: 10px; line-height: 2; }
          #box { --w: 1rlh; width: var(--w); height: 1px; }
        </style>
      </head>
      <body>
        <div id="box"></div>
      </body>
    </html>"#;

  let width = element_width(html, (200, 100), "box");
  assert_close(width, 20.0, "typed custom property 1rlh");
}

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
fn container_query_length_uses_root_metrics_for_rlh() {
  let config = FastRenderConfig::default().with_font_sources(FontConfig::bundled_only());
  let mut renderer = FastRender::with_config(config).expect("renderer");

  let options = RenderOptions::default().with_viewport(200, 200);

  // Root has `line-height: 2` and `font-size: 10px`, so `1rlh` must resolve to 20px. The container
  // is only 15px wide, so the query should NOT match and `.target` should remain `display: flex`.
  // If `1rlh` fell back to `1.2rem` (12px), the query would incorrectly match and `.target` would
  // become `display: block` (stacking AAA and BBB).
  let html = r#"<!doctype html>
    <html>
      <head>
        <style>
          html { font-size: 10px; line-height: 2; }
          body { margin: 0; }
          .container { width: 15px; container-type: inline-size; }
          .target { display: flex; }
          @container (min-width: 1rlh) {
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
